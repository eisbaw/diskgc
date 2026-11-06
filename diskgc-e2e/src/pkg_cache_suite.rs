//! TASK-351: admission evidence for the package-manager cache built-ins
//! (`pip-cache`, `uv-cache`, `npm-cache`, `pnpm-store`, `yarn-cache`,
//! `ccache`, `go-build-cache`, `platformio-cache`, `jest-cache`),
//! docs/testing.md, Admission evidence.
//!
//! Their liveness class is *delegated*: the tool's own cleaner acts, diskgc
//! decides only when (after its own checks: ownership, mounts, the checkout
//! guard, the in-use scan). Per profile, the pinned real tool fills its
//! cache from a local registry the test serves (the containers have no
//! network); real caches at a non-default location (the tool's own
//! environment variable or option) and hand-made lookalikes at the wrong
//! path are the decoys; a dry run plans exactly the default cache with the
//! profile's command; the reclaim runs the cleaner under the deletion
//! oracle; the tool then fetches or builds again. The live counterexample
//! starts a tool run that uses the cache and holds it in flight (a gated
//! registry request, a gated compiler or test) while diskgc reclaims, and,
//! where diskgc keeps the cache `in_use`, also runs the tool's own cleaner
//! directly: what the tool's locking (or its absence) does to that run is
//! the evidence the liveness row records.
//!
//! [`delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path`]
//! shows what the profiles do with a tool that is not on `PATH`, and that
//! no tool version is checked (stand-in executables log the exact argv,
//! working directory and environment they are run with).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::admission::{
    CTL, Expected, HOME, InFlight, NODE, NPM, REG, REGISTRY, Registry, TOOL_TIMEOUT, UV, actions,
    any_file, control_dir, count_files, exists, expect_in_use_by, expect_kept, home, lookalike,
    npm_packages, payload, plan_of, python, release, skipped, sorted, tool, versions, wait_file,
    write, write_script,
};
use crate::ensure_suite::report;
use crate::oracle::{Fixture, Oracle};
use crate::owner_pid_suite::wait_for;
use crate::proc_locks::Watched;
use crate::suite::{Ctx, TestResult};
use crate::under_test::{Run, run_as_tester_with_env};
use crate::{TESTER, ensure, info};

/// The pinned tools of the Nix image (flake.nix: `cacheTools`) and its C
/// compiler (the Cargo toolchains' `cc`).
const PIP: &str = "/bin/pip";
const PNPM: &str = "/bin/pnpm";
const YARN: &str = "/bin/yarn";
const CCACHE: &str = "/bin/ccache";
const GO: &str = "/bin/go";
const PIO: &str = "/bin/pio";
const JEST: &str = "/bin/jest";
const CC_PATH: &str = "/bin:/opt/cargo-under-test/cc/bin";
const GCC: &str = "/opt/cargo-under-test/cc/bin/gcc";

/// Each profile's command, as the plan's `operation` prints it (builtin.yaml).
const PIP_OP: &str =
    r#"command ["pip", "--cache-dir", "{path}", "cache", "purge"], cwd: candidate"#;
const UV_OP: &str = r#"command ["uv", "cache", "clean", "--cache-dir", "{path}"], cwd: parent"#;
const NPM_OP: &str =
    r#"command ["npm", "cache", "clean", "--force", "--cache", "{path}"], cwd: candidate"#;
const PNPM_OP: &str =
    r#"command ["pnpm", "store", "prune", "--store-dir", "{path}"], cwd: candidate"#;
const YARN_OP: &str =
    r#"command ["env", "YARN_CACHE_FOLDER={path}", "yarn", "cache", "clean"], cwd: parent"#;
const CCACHE_OP: &str =
    r#"command ["env", "CCACHE_DIR={path}", "ccache", "--clear"], cwd: candidate"#;
const GO_OP: &str = r#"command ["env", "GOCACHE={path}", "go", "clean", "-cache"], cwd: candidate"#;
const PIO_OP: &str = r#"command ["env", "PLATFORMIO_CACHE_DIR=.cache", "pio", "system", "prune", "--cache", "--force"], cwd: parent"#;
const JEST_OP: &str = r#"command ["jest", "--config", "{}", "--cacheDirectory", "{path}", "--clearCache"], cwd: parent"#;

// ---- the local registry ----------------------------------------------------

/// A pure-Python wheel `NAME-1.0-py3-none-any.whl` with a 200 kB payload,
/// written to `OUT/NAME/` (the simple index's project page).
const MKWHEEL_PY: &str = r#"
import base64, hashlib, os, sys, zipfile
out, names = sys.argv[1], sys.argv[2:]
for name in names:
    dist = f"{name}-1.0.dist-info"
    files = {
        f"{name}/__init__.py": f"NAME = {name!r}\n".encode(),
        f"{name}/blob.bin": os.urandom(200_000),
        f"{dist}/METADATA": f"Metadata-Version: 2.1\nName: {name}\nVersion: 1.0\n".encode(),
        f"{dist}/WHEEL": b"Wheel-Version: 1.0\nGenerator: diskgc-e2e\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    }
    record = [f"{p},sha256={base64.urlsafe_b64encode(hashlib.sha256(d).digest()).rstrip(b'=').decode()},{len(d)}"
              for p, d in files.items()] + [f"{dist}/RECORD,,"]
    files[f"{dist}/RECORD"] = ("\n".join(record) + "\n").encode()
    os.makedirs(os.path.join(out, name), exist_ok=True)
    with zipfile.ZipFile(os.path.join(out, name, f"{name}-1.0-py3-none-any.whl"), "w") as z:
        for p, d in files.items():
            z.writestr(p, d)
"#;

/// Closes the gate again for a second run in flight: removes `gate.go`
/// and the registry's `gate.hit`.
fn rearm(ctx: &Ctx) -> TestResult {
    ctx.home.remove_file(&format!("{CTL}/gate.go"))?;
    let hit = format!("{CTL}/gate.hit");
    if exists(&home(&hit)) {
        ctx.home.remove_file(&hit)?;
    }
    Ok(())
}

/// Wheels for `names` in the registry's simple index (`/simple/NAME/`).
fn wheels(ctx: &Ctx, names: &[&str]) -> TestResult {
    write(ctx, &format!("{CTL}/mkwheel.py"), MKWHEEL_PY.as_bytes())?;
    let mut args = vec![
        format!("{HOME}/{CTL}/mkwheel.py"),
        format!("{HOME}/{REG}/simple"),
    ];
    args.extend(names.iter().map(|n| (*n).to_owned()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    tool(ctx, Path::new(HOME), &[], python(ctx)?, &args)?;
    Ok(())
}

// ---- diskgc runs -----------------------------------------------------------

/// Whether a scenario also runs the tool's own cleaner directly against a
/// run in flight and reads its lock wait from `/proc/locks` (uv, pnpm).
/// That part tests the tool, not diskgc's binary, and needs one consistent
/// read of `/proc/locks` ([`crate::proc_locks::sample`]): in the container's
/// own PID namespace it lists the container's locks only, but the release
/// variants run in the host's (the release candidate refuses its own),
/// where it lists every host lock, more than one read returns on a busy
/// host. So the development variant runs it and the release variants stop
/// after diskgc's own verdict on the run in flight (TASK-360).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCleaner {
    RunDirectly,
    LeftToTheDevelopmentVariant,
}

/// `diskgc ensure` of `binary` (the development build, or the gate-lifted
/// release-candidate package) as tester (traced), `--min-free 100%` so a
/// reclaim takes every eligible candidate, with `env` added to its
/// environment.
#[allow(clippy::too_many_arguments)]
fn ensure_with(
    ctx: &Ctx,
    binary: &Path,
    env: &[(String, String)],
    dry_run: bool,
    select: &str,
    roots: Option<&str>,
    path: &str,
    config: Option<&str>,
) -> Result<Run, String> {
    let mut args = vec!["ensure"];
    if dry_run {
        args.push("--dry-run");
    }
    args.extend([
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--timeout",
        "120s",
        "--select",
        select,
    ]);
    if let Some(roots) = roots {
        args.extend(["--roots", roots]);
    }
    if let Some(config) = config {
        args.extend(["--config", config]);
    }
    args.push(path);
    let run = run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        env,
        binary,
        &args,
    )?;
    info!(
        "{} {} -> {:?}\n--- stdout\n{}--- stderr\n{}",
        binary.display(),
        args.join(" "),
        run.code,
        run.stdout,
        run.stderr
    );
    Ok(run)
}

/// Another home for diskgc, so a cleaner that ignored the path it is given
/// and fell back to its default under `HOME` would miss the measured cache
/// (every measured cache here is the tool's default for tester's real home).
const OTHER_HOME: &str = "/home/tester/other-home";

/// [`ensure_with`] with `HOME` set to [`OTHER_HOME`].
fn ensure(
    ctx: &Ctx,
    binary: &Path,
    dry_run: bool,
    select: &str,
    roots: Option<&str>,
    path: &str,
    config: Option<&str>,
) -> Result<Run, String> {
    ctx.home.mkdirs("other-home", 0o755, TESTER)?;
    let env = [("HOME".to_owned(), OTHER_HOME.to_owned())];
    ensure_with(ctx, binary, &env, dry_run, select, roots, path, config)
}

/// One scenario's selection: the binary under test, the profile, its roots
/// and measured path, an optional configuration, and its command as the
/// plan prints it.
struct Selection<'a> {
    binary: &'a Path,
    profile: &'a str,
    roots: Option<&'a str>,
    path: &'a str,
    config: Option<&'a str>,
    operation: &'a str,
}

impl Selection<'_> {
    fn select(&self) -> String {
        format!("profile:{}", self.profile)
    }
}

/// A dry run, then a reclaim, both judged by one deletion oracle whose
/// eligible set is exactly the expected reclaimable candidates (declared from
/// the fixture, never read from the report). The dry run plans exactly
/// `expected`, each with the profile's command; the reclaim acts on exactly
/// the reclaimable ones and keeps every other one for its expected reason.
/// A cleaner may leave its cache directory (or parts it keeps by design):
/// what it removed is each scenario's own check. Returns the reclaim report.
fn plan_then_reclaim(ctx: &Ctx, sel: &Selection, expected: &Expected) -> Result<Value, String> {
    let fixture = expected
        .iter()
        .filter(|(_, reason)| reason.is_none())
        .fold(Fixture::new(), |fixture, (path, _)| {
            fixture.eligible(path.clone())
        });
    let select = sel.select();
    let oracle = Oracle::arm(fixture)?;
    let dry = ensure(
        ctx, sel.binary, true, &select, sel.roots, sel.path, sel.config,
    )?;
    let run = ensure(
        ctx, sel.binary, false, &select, sel.roots, sel.path, sel.config,
    )?;
    let verdict = oracle.judge_runs(&[&dry, &run])?;
    dry.expect_code(0)?;
    run.expect_code(1)?;
    let planned = report(&dry)?;
    ensure!(
        plan_of(&planned)? == sorted(expected),
        "{select}: the dry run planned {:?}, expected {:?}: {planned}",
        plan_of(&planned)?,
        sorted(expected)
    );
    for entry in planned["plan"].as_array().into_iter().flatten() {
        ensure!(
            entry["operation"] == sel.operation,
            "{select}: planned operation {}, expected {}",
            entry["operation"],
            sel.operation
        );
    }
    let reclaimed = report(&run)?;
    let want: BTreeSet<String> = expected
        .iter()
        .filter(|(_, reason)| reason.is_none())
        .map(|(path, _)| path.display().to_string())
        .collect();
    ensure!(
        actions(&reclaimed)? == want,
        "{select}: the reclaim acted on {:?}, expected {want:?}: {reclaimed}",
        actions(&reclaimed)?
    );
    for (candidate, reason) in expected {
        if let Some(reason) = reason {
            let entry = skipped(&reclaimed, candidate)?;
            ensure!(
                entry["reason"] == *reason,
                "{select}: {} skipped as {}, expected {reason}: {entry}",
                candidate.display(),
                entry["reason"]
            );
        }
    }
    verdict.assert_clean()?;
    Ok(reclaimed)
}

/// A reclaim while a tool run is held in flight, judged by the oracle with
/// `eligible` (the candidates the cleaner may act on). Returns its report.
fn reclaim_in_flight(ctx: &Ctx, sel: &Selection, eligible: &[&Path]) -> Result<Value, String> {
    let fixture = eligible
        .iter()
        .fold(Fixture::new(), |f, path| f.eligible(*path));
    let oracle = Oracle::arm(fixture)?;
    let run = ensure(
        ctx,
        sel.binary,
        false,
        &sel.select(),
        sel.roots,
        sel.path,
        sel.config,
    )?;
    oracle.judge(&run)?.assert_clean()?;
    run.expect_code(1)?;
    report(&run)
}

/// The cleaner ran on exactly `candidates` (diskgc saw no use of them).
fn expect_actions(report: &Value, candidates: &[&Path]) -> TestResult {
    let acted = actions(report)?;
    let want: BTreeSet<String> = candidates.iter().map(|c| c.display().to_string()).collect();
    ensure!(
        acted == want,
        "expected the cleaner to run on {want:?}, acted on {acted:?}: {report}"
    );
    Ok(())
}

// ---- inspection helpers ----------------------------------------------------

/// The directories below `root` (0 when it is gone).
fn count_dirs(root: &Path) -> Result<usize, String> {
    if !exists(root) {
        return Ok(0);
    }
    let mut count = 0;
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
            if entry
                .file_type()
                .map_err(|e| format!("stat {}: {e}", entry.path().display()))?
                .is_dir()
            {
                count += 1;
                pending.push(entry.path());
            }
        }
    }
    Ok(count)
}

// ---- pip -------------------------------------------------------------------

const PIP_CACHE: &str = ".cache/pip";

fn pip_install(ctx: &Ctx, target: &str, env: &[(&str, &str)], names: &[&str]) -> TestResult {
    let target = home(target);
    let index = format!("{REGISTRY}/simple");
    let mut args = vec![
        "install",
        "--disable-pip-version-check",
        "--no-input",
        "--target",
        target.to_str().ok_or("non-UTF-8 target")?,
        "--index-url",
        &index,
        "--trusted-host",
        "127.0.0.1",
    ];
    args.extend(names);
    tool(ctx, Path::new(HOME), env, PIP, &args)?;
    Ok(())
}

/// pip's HTTP cache entries (its wheel cache stays empty: no sdist builds).
fn pip_entries(cache: &Path) -> Result<usize, String> {
    count_files(&cache.join("http-v2"), &any_file)
}

/// `pip-cache`: real `pip install`s from the local index fill
/// `~/.cache/pip/http-v2` (pip caches only through a trusted host on
/// plain HTTP). A real cache under `PIP_CACHE_DIR` and a lookalike below a
/// project are never found; `pip cache purge` empties the HTTP cache (it
/// keeps the directories); the next install fetches the wheel again. An
/// install held in flight (its next wheel gated) holds nothing in the
/// cache, so diskgc purges it; pip has no cache lock, and the install
/// completes (a cache miss is a download).
pub fn pip_cache_real_pip_installs_reclaim_then_refetch_and_an_install_in_flight_completes(
    ctx: &Ctx,
) -> TestResult {
    pip_scenario(ctx, &ctx.diskgc())
}

/// TASK-360: [`pip_cache_real_pip_installs_reclaim_then_refetch_and_an_install_in_flight_completes`] through the gate-lifted release-candidate package,
/// whose 0.2 catalog holds `pip-cache` and whose release row lists its
/// command.
pub fn release_candidate_pip_cache_reclaim_then_refetch_and_an_install_in_flight_completes(
    ctx: &Ctx,
) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "pip-cache")?;
    pip_scenario(ctx, &binary)
}

fn pip_scenario(ctx: &Ctx, binary: &Path) -> TestResult {
    versions(ctx, &[(PIP, &["--version"], "pip 26.1.2 ")])?;
    control_dir(ctx)?;
    wheels(ctx, &["alpha", "beta", "gamma"])?;
    let registry = Registry::start(ctx, "gamma-1.0")?;
    pip_install(ctx, "site/one", &[], &["alpha", "beta"])?;
    pip_install(
        ctx,
        "site/alt",
        &[("PIP_CACHE_DIR", "/home/tester/alt/pip")],
        &["alpha"],
    )?;
    let decoy = lookalike(ctx, "src/proj/.cache/pip/http-v2")?;
    let cache = home(PIP_CACHE);
    ensure!(
        pip_entries(&cache)? > 0,
        "pip cached nothing in {}",
        cache.display()
    );
    let alt = home("alt/pip");
    let alt_entries = pip_entries(&alt)?;
    ensure!(alt_entries > 0, "pip cached nothing under PIP_CACHE_DIR");

    let sel = Selection {
        binary,
        profile: "pip-cache",
        roots: Some(HOME),
        path: HOME,
        config: None,
        operation: PIP_OP,
    };
    plan_then_reclaim(ctx, &sel, &vec![(cache.clone(), None)])?;
    ensure!(
        pip_entries(&cache)? == 0,
        "pip cache purge left HTTP entries"
    );
    ensure!(
        pip_entries(&alt)? == alt_entries,
        "the PIP_CACHE_DIR cache changed"
    );
    expect_kept(&[&decoy])?;

    let wheel = "/simple/alpha/alpha-1.0-py3-none-any.whl";
    let fetched = registry.served(wheel)?;
    pip_install(ctx, "site/two", &[], &["alpha"])?;
    ensure!(
        registry.served(wheel)? > fetched && pip_entries(&cache)? > 0,
        "pip did not fetch and cache alpha again"
    );

    let mut install = InFlight::start(
        ctx,
        "pip-install",
        Path::new(HOME),
        &[],
        PIP,
        &[
            "install",
            "--disable-pip-version-check",
            "--no-input",
            "--target",
            "/home/tester/site/three",
            "--index-url",
            &format!("{REGISTRY}/simple"),
            "--trusted-host",
            "127.0.0.1",
            "alpha",
            "gamma",
            "beta",
        ],
    )?;
    let gated = registry.wait_gated(&mut install)?;
    info!("pip in flight at {gated}");
    let reclaimed = reclaim_in_flight(ctx, &sel, &[&cache])?;
    expect_actions(&reclaimed, &[&cache])?;
    ensure!(
        pip_entries(&cache)? == 0,
        "the purge under the install in flight left entries"
    );
    release(ctx)?;
    install.expect_success()?;
    tool(
        ctx,
        Path::new(HOME),
        &[
            ("PYTHONPATH", "/home/tester/site/three"),
            ("PYTHONDONTWRITEBYTECODE", "1"),
        ],
        python(ctx)?,
        &[
            "-c",
            "import alpha, beta, gamma; assert gamma.NAME == 'gamma'",
        ],
    )?;

    // Jest's failure mode (TASK-360), for pip: the purge lands after the
    // install's cache lookup, and the install then only writes. A single
    // install of gamma, held at its wheel (the purge before makes it a
    // cache miss; the version check, off, would read the cache later):
    // after the gate pip stores the response in the purged cache, creating
    // its directories, and installs it.
    tool(
        ctx,
        Path::new(HOME),
        &[],
        PIP,
        &[
            "--cache-dir",
            cache.to_str().ok_or("cache")?,
            "cache",
            "purge",
        ],
    )?;
    rearm(ctx)?;
    let mut last = InFlight::start(
        ctx,
        "pip-install-last",
        Path::new(HOME),
        &[],
        PIP,
        &[
            "install",
            "--disable-pip-version-check",
            "--no-input",
            "--target",
            "/home/tester/site/four",
            "--index-url",
            &format!("{REGISTRY}/simple"),
            "--trusted-host",
            "127.0.0.1",
            "gamma",
        ],
    )?;
    let gated = registry.wait_gated(&mut last)?;
    ensure!(
        gated.ends_with("/gamma-1.0-py3-none-any.whl"),
        "pip was held at {gated}, not at its only wheel"
    );
    let wheel = "/simple/gamma/gamma-1.0-py3-none-any.whl";
    let served = registry.served(wheel)?;
    let reclaimed = reclaim_in_flight(ctx, &sel, &[&cache])?;
    expect_actions(&reclaimed, &[&cache])?;
    ensure!(
        pip_entries(&cache)? == 0,
        "diskgc's purge under the held install left entries"
    );
    // Diagnostic only: pip creates the directories it writes into
    // (`ensure_dir` before each cache write), whatever the purge kept.
    info!(
        "after pip cache purge: {} directories left in {}",
        count_dirs(&cache)?,
        cache.display()
    );
    release(ctx)?;
    let output = last.expect_success()?;
    ensure!(
        registry.served(wheel)? == served + 1,
        "pip fetched gamma's wheel other than once after the gate"
    );
    ensure!(
        !output.to_lowercase().contains("warning"),
        "pip warned after its cache was purged under it:\n{output}"
    );
    ensure!(
        pip_entries(&cache)? > 0,
        "pip did not store gamma in the purged cache"
    );
    tool(
        ctx,
        Path::new(HOME),
        &[
            ("PYTHONPATH", "/home/tester/site/four"),
            ("PYTHONDONTWRITEBYTECODE", "1"),
        ],
        python(ctx)?,
        &["-c", "import gamma; assert gamma.NAME == 'gamma'"],
    )?;
    Ok(())
}

// ---- uv --------------------------------------------------------------------

const UV_CACHE: &str = ".cache/uv";
const UV_ENV: &[(&str, &str)] = &[
    ("UV_NO_PROGRESS", "1"),
    ("UV_PYTHON_DOWNLOADS", "never"),
    ("PYTHONDONTWRITEBYTECODE", "1"),
];

/// A venv at `rel` made by `uv venv`, with `names` installed by `uv pip
/// install` from the local index (`env` added, e.g. `UV_CACHE_DIR`).
fn uv_venv_install(ctx: &Ctx, rel: &str, env: &[(&str, &str)], names: &[&str]) -> TestResult {
    let env: Vec<(&str, &str)> = UV_ENV.iter().chain(env).copied().collect();
    let python = python(ctx)?;
    let venv = home(rel);
    let venv = venv.to_str().ok_or("non-UTF-8 venv")?;
    let interpreter = python.to_str().ok_or("non-UTF-8 python")?;
    tool(
        ctx,
        Path::new(HOME),
        &env,
        UV,
        &["venv", "--quiet", "--python", interpreter, venv],
    )?;
    let mut args = uv_install_args(venv);
    args.extend(names.iter().map(|n| (*n).to_owned()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    tool(ctx, Path::new(HOME), &env, UV, &args)?;
    Ok(())
}

fn uv_install_args(venv: &str) -> Vec<String> {
    [
        "pip",
        "install",
        "--python",
        &format!("{venv}/bin/python"),
        "--index-url",
        &format!("{REGISTRY}/simple"),
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

fn uv_imports(ctx: &Ctx, rel: &str, names: &str) -> TestResult {
    let python = home(&format!("{rel}/bin/python"));
    tool(
        ctx,
        Path::new(HOME),
        UV_ENV,
        &python,
        &["-c", &format!("import {names}")],
    )?;
    Ok(())
}

/// `uv-cache`: a real uv cache always holds `sdists-v10/.git`, an empty
/// regular file uv writes when it creates the cache, which the profile's
/// `empty_checkout_markers` tolerate (Mark, 2026-11-05, TASK-360): the dry
/// run plans the cache with the profile's command and `uv cache clean`
/// removes it whole; an environment installed from it (uv's default
/// hardlink mode) still imports; the next install fetches again. Only that
/// file, empty, is tolerated: dry runs with a `.git` directory or a
/// non-empty `.git` file in its place, an empty `.git` elsewhere in the
/// cache, or a checkout-shaped `.git` directory (HEAD, objects, refs; the
/// image has no git) below `git-v0/checkouts` each keep the cache
/// `contains_checkout`. A real cache under `UV_CACHE_DIR` and a lookalike
/// below a project are never found. A `uv pip install` held in flight
/// holds uv's cache lock (`.lock`, a shared `flock`) open: diskgc keeps
/// the cache `in_use`, and uv's own `cache clean`, run directly, waits for
/// the lock ("Cache is currently in-use"): the install completes, then the
/// clean does.
pub fn uv_cache_real_uv_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    uv_scenario(ctx, &ctx.diskgc(), ToolCleaner::RunDirectly)
}

/// TASK-360: [`uv_cache_real_uv_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept`]
/// through the gate-lifted release-candidate package, whose 0.2 catalog
/// holds `uv-cache` and whose release row lists its command, up to
/// diskgc's `in_use` verdict on the install in flight
/// ([`ToolCleaner::LeftToTheDevelopmentVariant`]).
pub fn release_candidate_uv_cache_reclaim_then_refetch_and_an_install_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "uv-cache")?;
    uv_scenario(ctx, &binary, ToolCleaner::LeftToTheDevelopmentVariant)
}

/// A dry run of `sel` alone (nothing eligible for the oracle) that plans
/// exactly `expected`.
fn plan_only(ctx: &Ctx, sel: &Selection, expected: &Expected, what: &str) -> TestResult {
    let oracle = Oracle::arm(Fixture::new())?;
    let dry = ensure(
        ctx,
        sel.binary,
        true,
        &sel.select(),
        sel.roots,
        sel.path,
        sel.config,
    )?;
    oracle.judge(&dry)?.assert_clean()?;
    dry.expect_code(0)?;
    let planned = report(&dry)?;
    ensure!(
        plan_of(&planned)? == sorted(expected),
        "{what}: the dry run planned {:?}, expected {:?}: {planned}",
        plan_of(&planned)?,
        sorted(expected)
    );
    Ok(())
}

/// Each place and shape of a checkout marker the uv-cache exception does
/// not tolerate, made in the real cache one at a time (as tester), keeps
/// it `contains_checkout` in a dry run; the cache is restored after each
/// (a directory added is moved out to `ctl/`, a file added is removed).
fn uv_markers_it_does_not_tolerate(ctx: &Ctx, sel: &Selection, cache: &Path) -> TestResult {
    let refused = vec![(cache.to_owned(), Some("contains_checkout"))];
    let marker = ".cache/uv/sdists-v10/.git";
    let aside = format!("{CTL}/uv-sdists-git");
    let empty = || -> TestResult {
        let path = home(marker);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("stat {}: {e}", path.display()))?;
        ensure!(
            meta.file_type().is_file() && meta.len() == 0,
            "{} is not the empty regular file uv writes: {meta:?}",
            path.display()
        );
        Ok(())
    };
    empty()?;

    // A `.git` directory in its place.
    ctx.home.rename(marker, &aside)?;
    ctx.home
        .mkdirs(&format!("{marker}/objects"), 0o755, TESTER)?;
    write(ctx, &format!("{marker}/HEAD"), b"ref: refs/heads/main\n")?;
    plan_only(ctx, sel, &refused, "a .git directory at sdists-v10/.git")?;
    ctx.home.rename(marker, &format!("{CTL}/uv-git-dir"))?;
    ctx.home.rename(&aside, marker)?;
    empty()?;

    // A `.git` file with content in its place (a worktree's pointer).
    ctx.home.rename(marker, &aside)?;
    write(
        ctx,
        marker,
        b"gitdir: /home/tester/src/proj/.git/worktrees/x\n",
    )?;
    plan_only(
        ctx,
        sel,
        &refused,
        "a non-empty .git file at sdists-v10/.git",
    )?;
    ctx.home.remove_file(marker)?;
    ctx.home.rename(&aside, marker)?;
    empty()?;

    // An empty `.git` file elsewhere in the cache.
    let elsewhere = ".cache/uv/archive-v0/.git";
    ensure!(!exists(&home(elsewhere)), "uv wrote {elsewhere}");
    write(ctx, elsewhere, b"")?;
    plan_only(ctx, sel, &refused, "an empty .git at archive-v0/.git")?;
    ctx.home.remove_file(elsewhere)?;

    // A git dependency's checkout below `git-v0` (uv's git cache bucket).
    let git = ".cache/uv/git-v0";
    ensure!(
        !exists(&home(git)),
        "uv wrote {git} without a git dependency"
    );
    let checkout = format!("{git}/checkouts/4f0a/0123abcd");
    ctx.home
        .mkdirs(&format!("{checkout}/.git/objects/pack"), 0o755, TESTER)?;
    ctx.home
        .mkdirs(&format!("{checkout}/.git/refs/heads"), 0o755, TESTER)?;
    write(
        ctx,
        &format!("{checkout}/.git/HEAD"),
        b"ref: refs/heads/main\n",
    )?;
    write(
        ctx,
        &format!("{checkout}/pyproject.toml"),
        b"[project]\nname = \"dep\"\n",
    )?;
    plan_only(ctx, sel, &refused, "a checkout below git-v0/checkouts")?;
    ctx.home.rename(git, &format!("{CTL}/uv-git-v0"))?;
    Ok(())
}

fn uv_scenario(ctx: &Ctx, binary: &Path, tool_cleaner: ToolCleaner) -> TestResult {
    versions(ctx, &[(UV, &["--version"], "uv 0.13.0 ")])?;
    control_dir(ctx)?;
    wheels(ctx, &["alpha", "beta", "gamma"])?;
    let registry = Registry::start(ctx, "gamma-1.0")?;
    uv_venv_install(ctx, "src/app/.venv", &[], &["alpha", "beta"])?;
    uv_venv_install(
        ctx,
        "src/alt/.venv",
        &[("UV_CACHE_DIR", "/home/tester/alt/uv")],
        &["alpha"],
    )?;
    let decoy = lookalike(ctx, "src/proj/.cache/uv/archive-v0")?;
    let cache = home(UV_CACHE);
    let marker = cache.join("sdists-v10/.git");
    ensure!(
        marker.is_file(),
        "uv no longer writes {} (the tolerated marker)",
        marker.display()
    );
    let alt = home("alt/uv");
    let alt_files = count_files(&alt, &any_file)?;
    ensure!(
        alt_files > 0,
        "the tool wrote nothing to its non-default cache {}",
        alt.display()
    );

    let sel = Selection {
        binary,
        profile: "uv-cache",
        roots: Some(HOME),
        path: HOME,
        config: None,
        operation: UV_OP,
    };
    uv_markers_it_does_not_tolerate(ctx, &sel, &cache)?;
    plan_then_reclaim(ctx, &sel, &vec![(cache.clone(), None)])?;
    ensure!(!exists(&cache), "uv cache clean left {}", cache.display());
    ensure!(
        count_files(&alt, &any_file)? == alt_files,
        "the UV_CACHE_DIR cache changed"
    );
    expect_kept(&[&decoy])?;
    uv_imports(ctx, "src/app/.venv", "alpha, beta")?;

    let wheel = "/simple/alpha/alpha-1.0-py3-none-any.whl";
    let fetched = registry.served(wheel)?;
    uv_venv_install(ctx, "src/two/.venv", &[], &["alpha"])?;
    ensure!(
        registry.served(wheel)? > fetched && count_files(&cache, &any_file)? > 0,
        "uv did not fetch and cache alpha again"
    );

    tool(
        ctx,
        Path::new(HOME),
        UV_ENV,
        UV,
        &[
            "venv",
            "--quiet",
            "--python",
            python(ctx)?.to_str().ok_or("python")?,
            "/home/tester/src/three/.venv",
        ],
    )?;
    let mut args = uv_install_args("/home/tester/src/three/.venv");
    args.extend(["alpha", "gamma"].map(str::to_owned));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut install = InFlight::start(ctx, "uv-install", Path::new(HOME), UV_ENV, UV, &args)?;
    registry.wait_gated(&mut install)?;
    let reclaimed = reclaim_in_flight(ctx, &sel, &[&cache])?;
    let lock = cache.join(".lock");
    expect_in_use_by(&reclaimed, &cache, &install, &["fd"], "uv lock", &|p| {
        p == lock
    })?;
    if tool_cleaner == ToolCleaner::LeftToTheDevelopmentVariant {
        release(ctx)?;
        install.expect_success()?;
        ensure!(exists(&cache), "the cache kept in use is gone");
        uv_imports(ctx, "src/three/.venv", "alpha, gamma")?;
        return Ok(());
    }
    let watched = Watched::files(std::slice::from_ref(&lock))?;
    watched.wait_for(
        install.pid(),
        TOOL_TIMEOUT,
        "holding uv's cache lock shared",
        |u| u.holds.iter().any(|(p, mode)| *p == lock && mode == "READ"),
    )?;

    let mut clean = InFlight::start(
        ctx,
        "uv-cache-clean",
        &home(".cache"),
        UV_ENV,
        UV,
        &[
            "cache",
            "clean",
            "--cache-dir",
            cache.to_str().ok_or("cache")?,
        ],
    )?;
    watched.wait_for(
        clean.pid(),
        TOOL_TIMEOUT,
        "waiting for uv's cache lock",
        |u| u.waits(&lock),
    )?;
    clean.still_running()?;
    ensure!(
        clean.output().contains("Cache is currently in-use"),
        "uv cache clean did not report waiting for the lock: {}",
        clean.output()
    );
    ensure!(
        cache.join("sdists-v10").is_dir(),
        "uv cache clean did not wait for the lock"
    );
    release(ctx)?;
    install.expect_success()?;
    clean.expect_success()?;
    ensure!(
        !exists(&cache),
        "uv cache clean left {} after the lock",
        cache.display()
    );
    uv_imports(ctx, "src/three/.venv", "alpha, gamma")?;
    Ok(())
}

// ---- npm -------------------------------------------------------------------

const NPM_CACHE: &str = ".npm";
const NPM_ENV: &[(&str, &str)] = &[
    ("npm_config_registry", "http://127.0.0.1:4873/"),
    ("npm_config_audit", "false"),
    ("npm_config_fund", "false"),
    ("npm_config_update_notifier", "false"),
];

/// An npm project at `src/NAME` depending on `deps` (each `1.0.0`).
fn node_project(ctx: &Ctx, name: &str, deps: &[&str]) -> Result<PathBuf, String> {
    let deps: Vec<String> = deps.iter().map(|d| format!(r#""{d}":"1.0.0""#)).collect();
    write(
        ctx,
        &format!("src/{name}/package.json"),
        format!(
            r#"{{"name":"{name}","version":"1.0.0","private":true,"license":"MIT","dependencies":{{{}}}}}"#,
            deps.join(",")
        )
        .as_bytes(),
    )?;
    Ok(home(&format!("src/{name}")))
}

/// Node in `project` loads each of `deps` and gets its name.
fn node_loads(ctx: &Ctx, project: &Path, deps: &[&str]) -> TestResult {
    let script: String = deps
        .iter()
        .map(|d| format!("if (require('{d}') !== '{d}') process.exit(1);"))
        .collect();
    tool(ctx, project, &[], NODE, &["-e", &script])?;
    Ok(())
}

/// `npm-cache`: real `npm install`s from the local registry fill
/// `~/.npm/_cacache`. A real cache under `npm_config_cache` and a
/// lookalike at `~/.cache/npm` are never found; `npm cache clean --force`
/// removes `_cacache` (npm keeps its `_logs`); the next install fetches
/// the tarball again. Every running npm holds its debug log
/// (`~/.npm/_logs/*-debug-0.log`) open, so an install held in flight keeps
/// the cache `in_use`. npm has no cache lock: its own clean, run directly
/// meanwhile, removes `_cacache`, and the install still completes (it
/// writes what it fetches anew).
pub fn npm_cache_real_npm_installs_reclaim_then_refetch_and_a_running_npm_keeps_it(
    ctx: &Ctx,
) -> TestResult {
    let binary = &ctx.diskgc();
    versions(
        ctx,
        &[
            (NPM, &["--version"], "11.19.0"),
            (NODE, &["--version"], "v24.21.0"),
        ],
    )?;
    control_dir(ctx)?;
    npm_packages(ctx, &["alpha", "beta", "gamma"])?;
    let registry = Registry::start(ctx, "gamma-1.0.0.tgz")?;
    let app = node_project(ctx, "app", &["alpha", "beta"])?;
    tool(ctx, &app, NPM_ENV, NPM, &["install"])?;
    let other = node_project(ctx, "other", &["alpha"])?;
    let alt_env: Vec<(&str, &str)> = NPM_ENV
        .iter()
        .copied()
        .chain([("npm_config_cache", "/home/tester/alt/npm")])
        .collect();
    tool(ctx, &other, &alt_env, NPM, &["install"])?;
    let decoy = lookalike(ctx, ".cache/npm/_cacache")?;
    let cache = home(NPM_CACHE);
    let cacache = cache.join("_cacache");
    ensure!(count_files(&cacache, &any_file)? > 0, "npm cached nothing");
    let alt = home("alt/npm/_cacache");
    let alt_files = count_files(&alt, &any_file)?;
    ensure!(
        alt_files > 0,
        "the tool wrote nothing to its non-default cache {}",
        alt.display()
    );

    let sel = Selection {
        binary,
        profile: "npm-cache",
        roots: Some(HOME),
        path: HOME,
        config: None,
        operation: NPM_OP,
    };
    plan_then_reclaim(ctx, &sel, &vec![(cache.clone(), None)])?;
    ensure!(
        !exists(&cacache) && cache.join("_logs").is_dir(),
        "npm cache clean left _cacache, or took _logs"
    );
    ensure!(
        count_files(&alt, &any_file)? == alt_files,
        "the npm_config_cache cache changed"
    );
    expect_kept(&[&decoy])?;
    node_loads(ctx, &app, &["alpha", "beta"])?;

    let tarball = "/alpha/-/alpha-1.0.0.tgz";
    let fetched = registry.served(tarball)?;
    let two = node_project(ctx, "two", &["alpha"])?;
    tool(ctx, &two, NPM_ENV, NPM, &["install"])?;
    ensure!(
        registry.served(tarball)? > fetched && count_files(&cacache, &any_file)? > 0,
        "npm did not fetch and cache alpha again"
    );

    let three = node_project(ctx, "three", &["alpha", "gamma"])?;
    let mut install = InFlight::start(ctx, "npm-install", &three, NPM_ENV, NPM, &["install"])?;
    registry.wait_gated(&mut install)?;
    let reclaimed = reclaim_in_flight(ctx, &sel, &[&cache])?;
    expect_in_use_by(&reclaimed, &cache, &install, &["fd"], "debug log", &|p| {
        p.starts_with(cache.join("_logs")) && p.to_string_lossy().ends_with("-debug-0.log")
    })?;
    tool(
        ctx,
        Path::new(HOME),
        &[],
        NPM,
        &[
            "cache",
            "clean",
            "--force",
            "--cache",
            cache.to_str().ok_or("cache")?,
        ],
    )?;
    ensure!(!exists(&cacache), "npm cache clean left _cacache");
    release(ctx)?;
    install.expect_success()?;
    node_loads(ctx, &three, &["alpha", "gamma"])?;
    Ok(())
}

// ---- pnpm ------------------------------------------------------------------

const PNPM_STORE: &str = ".local/share/pnpm/store";
const PNPM_ARGS: &[&str] = &[
    "--registry",
    "http://127.0.0.1:4873/",
    "--reporter",
    "append-only",
];

fn pnpm(ctx: &Ctx, project: &Path, command: &[&str], extra: &[&str]) -> TestResult {
    let args: Vec<&str> = command
        .iter()
        .chain(PNPM_ARGS)
        .chain(extra)
        .copied()
        .collect();
    tool(ctx, project, &[], PNPM, &args)?;
    Ok(())
}

/// `pnpm-store`: real `pnpm install`s fill the store (pnpm 12.9.0:
/// `store/v11`, an SQLite `index.db` and content files); a second project
/// then drops `gamma` (`pnpm remove`), so no project references it. A real
/// store under `--store-dir` and a lookalike at pnpm's legacy default
/// `~/.pnpm-store` are never found. `pnpm store prune` removes only what
/// no project references: the first project still loads its packages, and
/// adding `gamma` back fetches it again. An install held in flight holds
/// the store's `index.db` open (diskgc keeps it `in_use`) and pnpm's
/// store-operation lock (`/tmp/pnpm-store-operation-locks-UID`, a shared
/// `flock`, whatever `TMPDIR` is): its own prune, run directly, waits for
/// it, and the install completes.
pub fn pnpm_store_real_pnpm_prune_keeps_referenced_packages_and_an_install_in_flight(
    ctx: &Ctx,
) -> TestResult {
    pnpm_scenario(ctx, &ctx.diskgc(), ToolCleaner::RunDirectly)
}

/// TASK-360: [`pnpm_store_real_pnpm_prune_keeps_referenced_packages_and_an_install_in_flight`]
/// through the gate-lifted release-candidate package, whose 0.2 catalog
/// holds `pnpm-store` and whose release row lists its command, up to
/// diskgc's `in_use` verdict on the install in flight
/// ([`ToolCleaner::LeftToTheDevelopmentVariant`]).
pub fn release_candidate_pnpm_store_prune_keeps_referenced_packages_and_an_install_in_flight(
    ctx: &Ctx,
) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "pnpm-store")?;
    pnpm_scenario(ctx, &binary, ToolCleaner::LeftToTheDevelopmentVariant)
}

fn pnpm_scenario(ctx: &Ctx, binary: &Path, tool_cleaner: ToolCleaner) -> TestResult {
    versions(ctx, &[(PNPM, &["--version"], "12.9.0")])?;
    control_dir(ctx)?;
    npm_packages(ctx, &["alpha", "beta", "gamma", "delta"])?;
    let registry = Registry::start(ctx, "delta-1.0.0.tgz")?;
    let app = node_project(ctx, "app", &["alpha", "beta"])?;
    pnpm(ctx, &app, &["install"], &[])?;
    let two = node_project(ctx, "two", &["gamma"])?;
    pnpm(ctx, &two, &["install"], &[])?;
    pnpm(ctx, &two, &["remove", "gamma"], &[])?;
    let other = node_project(ctx, "other", &["alpha"])?;
    pnpm(
        ctx,
        &other,
        &["install"],
        &["--store-dir", "/home/tester/alt/pnpm-store"],
    )?;
    let decoy = lookalike(ctx, ".pnpm-store/v10/files")?;
    let store = home(PNPM_STORE);
    ensure!(
        store.join("v11/index.db").is_file(),
        "pnpm 12.9.0's store layout changed"
    );
    let before = count_files(&store, &any_file)?;
    let alt = home("alt/pnpm-store");
    let alt_files = count_files(&alt, &any_file)?;
    ensure!(
        alt_files > 0,
        "the tool wrote nothing to its non-default cache {}",
        alt.display()
    );

    let sel = Selection {
        binary,
        profile: "pnpm-store",
        roots: Some(HOME),
        path: HOME,
        config: None,
        operation: PNPM_OP,
    };
    // pnpm writes its store-operation lock below /tmp, outside the candidate.
    plan_then_reclaim(ctx, &sel, &vec![(store.clone(), None)])?;
    ensure!(
        count_files(&store, &any_file)? < before,
        "pnpm store prune removed nothing ({before} files)"
    );
    ensure!(
        count_files(&alt, &any_file)? == alt_files,
        "the --store-dir store changed"
    );
    expect_kept(&[&decoy])?;
    node_loads(ctx, &app, &["alpha", "beta"])?;

    let tarball = "/gamma/-/gamma-1.0.0.tgz";
    let fetched = registry.served(tarball)?;
    pnpm(ctx, &two, &["add", "gamma@1.0.0"], &[])?;
    ensure!(
        registry.served(tarball)? > fetched,
        "pnpm did not fetch the pruned gamma again"
    );

    // The prune kept what a project references: alpha is installed again
    // without a fetch.
    let alpha = "/alpha/-/alpha-1.0.0.tgz";
    let alpha_fetched = registry.served(alpha)?;
    let three = node_project(ctx, "three", &["alpha", "delta"])?;
    let args: Vec<&str> = ["install"].iter().chain(PNPM_ARGS).copied().collect();
    // pnpm's lock lives in /tmp whatever TMPDIR is: the install has another.
    ctx.home.mkdirs("tmp-elsewhere", 0o700, TESTER)?;
    let mut install = InFlight::start(
        ctx,
        "pnpm-install",
        &three,
        &[("TMPDIR", "/home/tester/tmp-elsewhere")],
        PNPM,
        &args,
    )?;
    registry.wait_gated(&mut install)?;
    let reclaimed = reclaim_in_flight(ctx, &sel, &[&store])?;
    let index = store.join("v11/index.db");
    expect_in_use_by(&reclaimed, &store, &install, &["fd"], "store index", &|p| {
        p == index
    })?;
    if tool_cleaner == ToolCleaner::LeftToTheDevelopmentVariant {
        release(ctx)?;
        install.expect_success()?;
        node_loads(ctx, &three, &["alpha", "delta"])?;
        node_loads(ctx, &app, &["alpha", "beta"])?;
        return Ok(());
    }
    let lock = PathBuf::from("/tmp/pnpm-store-operation-locks-1000/all-stores.lock");
    let watched = Watched::files(std::slice::from_ref(&lock))?;
    watched.wait_for(
        install.pid(),
        TOOL_TIMEOUT,
        "holding pnpm's store lock shared",
        |u| u.holds.iter().any(|(p, mode)| *p == lock && mode == "READ"),
    )?;
    let files = count_files(&store, &any_file)?;
    let mut prune = InFlight::start(
        ctx,
        "pnpm-store-prune",
        Path::new(HOME),
        &[],
        PNPM,
        &[
            "store",
            "prune",
            "--store-dir",
            store.to_str().ok_or("store")?,
        ],
    )?;
    watched.wait_for(
        prune.pid(),
        TOOL_TIMEOUT,
        "waiting for pnpm's store lock",
        |u| u.waits(&lock),
    )?;
    prune.still_running()?;
    ensure!(
        count_files(&store, &any_file)? == files,
        "pnpm store prune changed the store while it waited"
    );
    release(ctx)?;
    install.expect_success()?;
    prune.expect_success()?;
    node_loads(ctx, &three, &["alpha", "delta"])?;
    node_loads(ctx, &app, &["alpha", "beta"])?;
    ensure!(
        registry.served(alpha)? == alpha_fetched,
        "pnpm fetched alpha again: the prune removed a referenced package"
    );
    Ok(())
}

// ---- yarn ------------------------------------------------------------------

const YARN_CACHE: &str = ".cache/yarn";
const YARN_ARGS: &[&str] = &[
    "install",
    "--registry",
    "http://127.0.0.1:4873/",
    "--non-interactive",
    "--no-progress",
];

/// Yarn's package entries (`v6/npm-NAME-...`) below `cache`.
fn yarn_entries(cache: &Path) -> Result<usize, String> {
    count_files(cache, &|name| name == "package.json")
}

/// `yarn-cache`: real `yarn install`s (Yarn Classic) fill
/// `~/.cache/yarn/v6`. A real cache under `YARN_CACHE_FOLDER` and a
/// lookalike at `~/.yarn/cache` (Yarn Berry's) are never found; `yarn cache
/// clean` empties it; the next install fetches again. An install held in
/// flight writes the gated tarball into the cache, so diskgc keeps it
/// `in_use`. Yarn Classic has no cache lock: its own clean, run directly
/// meanwhile, empties the cache, and the install then fails (`ENOENT` while
/// linking a package it had fetched): the named residual when the clean
/// lands where diskgc sees no open file.
pub fn yarn_cache_real_yarn_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    let binary = &ctx.diskgc();
    versions(ctx, &[(YARN, &["--version"], "1.22.22")])?;
    control_dir(ctx)?;
    npm_packages(ctx, &["alpha", "beta", "gamma"])?;
    let registry = Registry::start(ctx, "gamma-1.0.0.tgz")?;
    let app = node_project(ctx, "app", &["alpha", "beta"])?;
    tool(ctx, &app, &[], YARN, YARN_ARGS)?;
    let other = node_project(ctx, "other", &["alpha"])?;
    tool(
        ctx,
        &other,
        &[("YARN_CACHE_FOLDER", "/home/tester/alt/yarn")],
        YARN,
        YARN_ARGS,
    )?;
    let decoy = lookalike(ctx, ".yarn/cache")?;
    let cache = home(YARN_CACHE);
    ensure!(
        yarn_entries(&cache)? >= 2,
        "yarn cached fewer than two packages"
    );
    let alt = home("alt/yarn");
    let alt_files = count_files(&alt, &any_file)?;
    ensure!(
        alt_files > 0,
        "the tool wrote nothing to its non-default cache {}",
        alt.display()
    );

    let sel = Selection {
        binary,
        profile: "yarn-cache",
        roots: Some(HOME),
        path: HOME,
        config: None,
        operation: YARN_OP,
    };
    plan_then_reclaim(ctx, &sel, &vec![(cache.clone(), None)])?;
    ensure!(yarn_entries(&cache)? == 0, "yarn cache clean left packages");
    ensure!(
        count_files(&alt, &any_file)? == alt_files,
        "the YARN_CACHE_FOLDER cache changed"
    );
    expect_kept(&[&decoy])?;
    node_loads(ctx, &app, &["alpha", "beta"])?;

    let tarball = "/alpha/-/alpha-1.0.0.tgz";
    let fetched = registry.served(tarball)?;
    let two = node_project(ctx, "two", &["alpha", "beta"])?;
    tool(ctx, &two, &[], YARN, YARN_ARGS)?;
    ensure!(
        registry.served(tarball)? > fetched && yarn_entries(&cache)? >= 2,
        "yarn did not fetch and cache alpha again"
    );

    let three = node_project(ctx, "three", &["alpha", "beta", "gamma"])?;
    let mut install = InFlight::start(ctx, "yarn-install", &three, &[], YARN, YARN_ARGS)?;
    registry.wait_gated(&mut install)?;
    let reclaimed = reclaim_in_flight(ctx, &sel, &[&cache])?;
    expect_in_use_by(&reclaimed, &cache, &install, &["fd"], "tarball", &|p| {
        p.ends_with("node_modules/gamma/.yarn-tarball.tgz")
    })?;
    tool(
        ctx,
        &home(".cache"),
        &[("YARN_CACHE_FOLDER", cache.to_str().ok_or("cache")?)],
        YARN,
        &["cache", "clean"],
    )?;
    release(ctx)?;
    let (status, output) = install.finish()?;
    ensure!(
        !status.success() && output.contains("ENOENT"),
        "the install in flight survived yarn cache clean ({status}); Yarn Classic gained a \
         cache lock? Update the yarn-cache liveness row:\n{output}"
    );
    tool(ctx, &three, &[], YARN, YARN_ARGS)?;
    node_loads(ctx, &three, &["alpha", "beta", "gamma"])?;
    Ok(())
}

// ---- ccache ----------------------------------------------------------------

const CCACHE_ENV: &[(&str, &str)] = &[("PATH", CC_PATH)];

/// ccache's cache entries (hex-named files; not `stats`, `CACHEDIR.TAG`).
fn ccache_entries(cache: &Path) -> Result<usize, String> {
    count_files(cache, &|name| {
        name.len() >= 30 && name.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// Compiles the C project at `src/c` with `ccache gcc` (`env` added, e.g.
/// `CCACHE_DIR`), links and runs it.
fn ccache_build(ctx: &Ctx, env: &[(&str, &str)]) -> TestResult {
    let env: Vec<(&str, &str)> = CCACHE_ENV.iter().chain(env).copied().collect();
    let project = home("src/c");
    for unit in ["one", "two", "main"] {
        tool(
            ctx,
            &project,
            &env,
            CCACHE,
            &[
                "gcc",
                "-c",
                &format!("{unit}.c"),
                "-o",
                &format!("{unit}.o"),
            ],
        )?;
    }
    tool(
        ctx,
        &project,
        &env,
        GCC,
        &["-o", "app", "one.o", "two.o", "main.o"],
    )?;
    tool(ctx, &project, &[], project.join("app"), &[])?;
    Ok(())
}

fn c_project(ctx: &Ctx) -> TestResult {
    write(ctx, "src/c/one.c", b"int one(void) { return 1; }\n")?;
    write(ctx, "src/c/two.c", b"int two(void) { return 2; }\n")?;
    write(
        ctx,
        "src/c/main.c",
        b"int one(void);\nint two(void);\nint main(void) { return one() + two() - 3; }\n",
    )?;
    // Stops when ccache compiles main.c (not when it preprocesses it:
    // `-E`) until the gate opens, and records the arguments: ccache has
    // then done its cache lookup for the compile (TASK-360); afterwards it
    // stores, reading only its manifest and stats, which may be gone.
    write_script(
        ctx,
        "gated-cc/gcc",
        b"#!/bin/sh\ncase \"$*\" in *-E*) ;; *main.c*) printf '%s\\n' \"$*\" > /home/tester/ctl/cc.hit\n  while [ ! -e /home/tester/ctl/gate.go ]; do sleep 0.1; done;; esac\nexec /opt/cargo-under-test/cc/bin/gcc \"$@\"\n",
    )?;
    Ok(())
}

/// `ccache`: real `ccache gcc` compiles fill both standard locations,
/// `~/.cache/ccache` (the default) and `~/.ccache` (through `CCACHE_DIR`,
/// as a legacy setup has it). A real cache under another `CCACHE_DIR` and a
/// lookalike at `~/.cache/sccache` are never found; `ccache --clear`
/// removes every entry of both (it keeps `stats` and `CACHEDIR.TAG`); the
/// next build compiles and caches again. A compile held in flight (the
/// compiler gated) holds ccache's inode cache (`tmp/inode-cache-*`) open
/// in its cache, so diskgc keeps that cache `in_use` and clears only the
/// other one. ccache needs no lock against its own clear: `ccache --clear`,
/// run directly meanwhile, empties the cache, and the compile completes and
/// the program runs (ccache stores its result anew).
pub fn ccache_real_compiles_reclaim_then_recompile_and_a_compile_in_flight_completes(
    ctx: &Ctx,
) -> TestResult {
    ccache_scenario(ctx, &ctx.diskgc())
}

/// TASK-360: [`ccache_real_compiles_reclaim_then_recompile_and_a_compile_in_flight_completes`] through the gate-lifted release-candidate package,
/// whose 0.2 catalog holds `ccache` and whose release row lists its
/// command.
pub fn release_candidate_ccache_reclaim_then_recompile_and_a_compile_in_flight_completes(
    ctx: &Ctx,
) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "ccache")?;
    ccache_scenario(ctx, &binary)
}

fn ccache_scenario(ctx: &Ctx, binary: &Path) -> TestResult {
    versions(ctx, &[(CCACHE, &["--version"], "ccache version 4.14.1\n")])?;
    control_dir(ctx)?;
    c_project(ctx)?;
    ccache_build(ctx, &[])?;
    ccache_build(ctx, &[("CCACHE_DIR", "/home/tester/.ccache")])?;
    ccache_build(ctx, &[("CCACHE_DIR", "/home/tester/alt/ccache")])?;
    let decoy = lookalike(ctx, ".cache/sccache")?;
    let caches = [home(".cache/ccache"), home(".ccache")];
    for cache in &caches {
        ensure!(
            ccache_entries(cache)? > 0,
            "ccache cached nothing in {}",
            cache.display()
        );
    }
    let alt = home("alt/ccache");
    let alt_entries = ccache_entries(&alt)?;
    ensure!(
        alt_entries > 0,
        "the tool wrote nothing to its non-default cache {}",
        alt.display()
    );

    let sel = Selection {
        binary,
        profile: "ccache",
        roots: Some(HOME),
        path: HOME,
        config: None,
        operation: CCACHE_OP,
    };
    plan_then_reclaim(
        ctx,
        &sel,
        &caches.iter().map(|c| (c.clone(), None)).collect(),
    )?;
    for cache in &caches {
        ensure!(
            ccache_entries(cache)? == 0,
            "ccache --clear left entries in {}",
            cache.display()
        );
    }
    ensure!(
        ccache_entries(&alt)? == alt_entries,
        "the other CCACHE_DIR cache changed"
    );
    expect_kept(&[&decoy])?;
    // ccache 4 uses `~/.ccache` whenever it exists, so name the default.
    ccache_build(ctx, &[("CCACHE_DIR", "/home/tester/.cache/ccache")])?;
    ensure!(
        ccache_entries(&caches[0])? > 0,
        "ccache did not cache again"
    );

    let project = home("src/c");
    let env: Vec<(&str, &str)> = CCACHE_ENV
        .iter()
        .chain(&[
            ("CCACHE_COMPILERTYPE", "gcc"),
            ("CCACHE_DIR", "/home/tester/.cache/ccache"),
        ])
        .copied()
        .collect();
    let mut compile = InFlight::start(
        ctx,
        "ccache-compile",
        &project,
        &env,
        CCACHE,
        &[
            "/home/tester/gated-cc/gcc",
            "-DIN_FLIGHT",
            "-c",
            "main.c",
            "-o",
            "main.o",
        ],
    )?;
    wait_file(&mut compile, &home(&format!("{CTL}/cc.hit")))?;
    // The file appears before its line is written: wait for the newline.
    let hit = wait_for(TOOL_TIMEOUT, "the gated compile's arguments", || {
        let text = std::fs::read_to_string(home(&format!("{CTL}/cc.hit"))).unwrap_or_default();
        Ok(text.ends_with('\n').then_some(text))
    })?;
    info!("ccache in flight at the compiler: {}", hit.trim());
    ensure!(
        hit.split_whitespace().any(|a| a == "-c") && !hit.contains("-E"),
        "the compile was held at {hit:?}, not at the compiler's compile step"
    );
    let reclaimed = reclaim_in_flight(ctx, &sel, &[&caches[0], &caches[1]])?;
    expect_actions(&reclaimed, &[&caches[1]])?;
    ensure!(
        ccache_entries(&caches[1])? == 0,
        "ccache --clear left entries in ~/.ccache"
    );
    expect_in_use_by(
        &reclaimed,
        &caches[0],
        &compile,
        &["fd"],
        "inode cache",
        &|p| {
            p.parent() == Some(caches[0].join("tmp").as_path())
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("inode-cache"))
        },
    )?;
    tool(
        ctx,
        Path::new(HOME),
        &[("CCACHE_DIR", "/home/tester/.cache/ccache")],
        CCACHE,
        &["--clear"],
    )?;
    ensure!(
        ccache_entries(&caches[0])? == 0,
        "ccache --clear left entries"
    );
    // Diagnostic only: ccache creates the directories it stores into.
    info!(
        "after ccache --clear: {} directories left in {}",
        count_dirs(&caches[0])?,
        caches[0].display()
    );
    release(ctx)?;
    compile.expect_success()?;
    ensure!(
        ccache_entries(&caches[0])? > 0,
        "the compile in flight did not store its result"
    );
    tool(
        ctx,
        &project,
        CCACHE_ENV,
        GCC,
        &["-o", "app", "one.o", "two.o", "main.o"],
    )?;
    tool(ctx, &project, &[], project.join("app"), &[])?;
    Ok(())
}

// ---- Go --------------------------------------------------------------------

const GO_CACHE: &str = ".cache/go-build";
const GO_ENV: &[(&str, &str)] = &[
    ("GOFLAGS", "-mod=mod"),
    ("GOPROXY", "off"),
    ("GOTOOLCHAIN", "local"),
    ("GOTELEMETRY", "off"),
    ("CGO_ENABLED", "0"),
];

/// Go's cache entries (`XX/HASH-d` outputs, `XX/HASH-a` action records).
fn go_entries(cache: &Path) -> Result<usize, String> {
    count_files(cache, &|name| name.ends_with("-d") || name.ends_with("-a"))
}

fn go_project(ctx: &Ctx) -> TestResult {
    write(
        ctx,
        "src/goapp/go.mod",
        b"module example.com/goapp\n\ngo 1.26\n",
    )?;
    write(
        ctx,
        "src/goapp/lib/lib.go",
        b"package lib\n\nfunc Answer() int { return 42 }\n",
    )?;
    write(
        ctx,
        "src/goapp/main.go",
        b"package main\n\nimport \"example.com/goapp/lib\"\n\nfunc main() { println(lib.Answer()) }\n",
    )?;
    // Stops the compile of package main until the gate opens.
    write_script(
        ctx,
        "gated-go/toolexec",
        b"#!/bin/sh\ncase \"$*\" in *\"-p main\"*) : > /home/tester/ctl/go.hit\n  while [ ! -e /home/tester/ctl/gate.go ]; do sleep 0.1; done;; esac\nexec \"$@\"\n",
    )?;
    Ok(())
}

fn go_build(ctx: &Ctx, env: &[(&str, &str)], output: &str) -> TestResult {
    let env: Vec<(&str, &str)> = GO_ENV.iter().chain(env).copied().collect();
    let project = home("src/goapp");
    tool(ctx, &project, &env, GO, &["build", "-o", output, "."])?;
    tool(ctx, &project, &[], project.join(output), &[])?;
    Ok(())
}

/// `go-build-cache`: real `go build`s fill `~/.cache/go-build`. A real
/// cache under `GOCACHE` and a lookalike below a project are never found;
/// `go clean -cache` removes every entry; the next build compiles again.
/// A build held in flight (the compile of package main gated, its imports
/// already compiled into the cache) has a cache entry mapped in the `go`
/// command, so diskgc keeps the cache `in_use` (evidence `map`). Go has no
/// cache lock: `go clean -cache`, run directly meanwhile, removes the
/// entries the build still needs, and the build fails ("could not import
/// ... no such file or directory"); one failed, retryable build, the next
/// one succeeds.
pub fn go_build_cache_real_go_builds_reclaim_then_rebuild_and_a_build_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    let binary = &ctx.diskgc();
    versions(ctx, &[(GO, &["version"], "go1.26.8 ")])?;
    control_dir(ctx)?;
    go_project(ctx)?;
    go_build(ctx, &[], "app")?;
    go_build(ctx, &[("GOCACHE", "/home/tester/alt/go-build")], "app-alt")?;
    let decoy = lookalike(ctx, "src/proj/.cache/go-build/00")?;
    let cache = home(GO_CACHE);
    ensure!(go_entries(&cache)? > 0, "go cached nothing");
    let alt = home("alt/go-build");
    let alt_entries = go_entries(&alt)?;
    ensure!(
        alt_entries > 0,
        "the tool wrote nothing to its non-default cache {}",
        alt.display()
    );

    let sel = Selection {
        binary,
        profile: "go-build-cache",
        roots: Some(HOME),
        path: HOME,
        config: None,
        operation: GO_OP,
    };
    plan_then_reclaim(ctx, &sel, &vec![(cache.clone(), None)])?;
    ensure!(go_entries(&cache)? == 0, "go clean -cache left entries");
    ensure!(
        go_entries(&alt)? == alt_entries,
        "the GOCACHE cache changed"
    );
    expect_kept(&[&decoy])?;
    go_build(ctx, &[], "app")?;
    ensure!(go_entries(&cache)? > 0, "go did not cache again");

    write(
        ctx,
        "src/goapp/version.go",
        b"package main\n\nconst version = 2\n",
    )?;
    let env: Vec<(&str, &str)> = GO_ENV.to_vec();
    let mut build = InFlight::start(
        ctx,
        "go-build",
        &home("src/goapp"),
        &env,
        GO,
        &[
            "build",
            "-toolexec",
            "/home/tester/gated-go/toolexec",
            "-o",
            "app2",
            ".",
        ],
    )?;
    wait_file(&mut build, &home(&format!("{CTL}/go.hit")))?;
    let reclaimed = reclaim_in_flight(ctx, &sel, &[&cache])?;
    expect_in_use_by(&reclaimed, &cache, &build, &["map"], "cache entry", &|p| {
        p.to_string_lossy().ends_with("-d")
    })?;
    let clean_env: Vec<(&str, &str)> = GO_ENV
        .iter()
        .copied()
        .chain([("GOCACHE", cache.to_str().ok_or("cache")?)])
        .collect();
    tool(ctx, &cache, &clean_env, GO, &["clean", "-cache"])?;
    ensure!(go_entries(&cache)? == 0, "go clean -cache left entries");
    release(ctx)?;
    let (status, output) = build.finish()?;
    ensure!(
        !status.success() && output.contains("could not import") && output.contains("no such file"),
        "the build in flight survived go clean -cache ({status}); update the go-build-cache \
         liveness row:\n{output}"
    );
    go_build(ctx, &[], "app2")?;
    Ok(())
}

// ---- PlatformIO ------------------------------------------------------------

const PIO_CACHE: &str = ".platformio/.cache";
const PIO_ENV: &[(&str, &str)] = &[
    ("PLATFORMIO_SETTING_ENABLE_TELEMETRY", "no"),
    ("PLATFORMIO_SETTING_CHECK_PLATFORMIO_INTERVAL", "36500"),
];

/// A PlatformIO tool package `NAME` (a `package.json` and a 400 kB
/// payload) packed by tar into `reg/pio/FILE`.
fn pio_archive(ctx: &Ctx, name: &str, file: &str) -> TestResult {
    write(
        ctx,
        &format!("pkgs/{name}/package.json"),
        format!(r#"{{"name":"{name}","version":"1.0.0","description":"diskgc e2e"}}"#).as_bytes(),
    )?;
    write(ctx, &format!("pkgs/{name}/payload.bin"), &payload(400_000))?;
    ctx.home.mkdirs(&format!("{REG}/pio"), 0o755, TESTER)?;
    tool(
        ctx,
        Path::new(HOME),
        &[],
        "/bin/tar",
        &[
            "-czf",
            &format!("{HOME}/{REG}/pio/{file}"),
            "-C",
            &format!("{HOME}/pkgs/{name}"),
            ".",
        ],
    )?;
    Ok(())
}

fn pio_install_args(file: &str) -> Vec<String> {
    [
        "pkg",
        "install",
        "--global",
        "--tool",
        &format!("{REGISTRY}/pio/{file}"),
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

fn pio_install(ctx: &Ctx, env: &[(&str, &str)], file: &str) -> TestResult {
    let env: Vec<(&str, &str)> = PIO_ENV.iter().chain(env).copied().collect();
    let args = pio_install_args(file);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    tool(ctx, Path::new(HOME), &env, PIO, &args)?;
    Ok(())
}

/// `platformio-cache`: real `pio pkg install --global --tool URL`s fill
/// `~/.platformio/.cache/downloads`. A real cache under
/// `PLATFORMIO_CACHE_DIR` and a lookalike at `~/.platformio/cache` are
/// never found; `pio system prune --cache` removes `.cache` while the
/// installed packages stay; reinstalling fetches the archive again. An
/// install held in flight holds its download (`.cache/downloads/tmp*`) and
/// its lock file open, so diskgc keeps the cache `in_use`. PIO's prune does
/// not take that lock: run directly meanwhile, it removes `.cache`, and the
/// install fails (`FileNotFoundError`); a retry installs it.
pub fn platformio_cache_real_pio_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    let binary = &ctx.diskgc();
    versions(
        ctx,
        &[(PIO, &["--version"], "PlatformIO Core, version 6.1.19")],
    )?;
    control_dir(ctx)?;
    pio_archive(ctx, "tool-demo", "tool-demo-1.0.0.tar.gz")?;
    pio_archive(ctx, "tool-alt", "tool-alt-1.0.0.tar.gz")?;
    pio_archive(ctx, "tool-gated", "tool-gated-GATE-1.0.0.tar.gz")?;
    let registry = Registry::start(ctx, "GATE")?;
    pio_install(ctx, &[], "tool-demo-1.0.0.tar.gz")?;
    pio_install(
        ctx,
        &[("PLATFORMIO_CACHE_DIR", "/home/tester/alt/pio-cache")],
        "tool-alt-1.0.0.tar.gz",
    )?;
    let decoy = lookalike(ctx, ".platformio/cache")?;
    let cache = home(PIO_CACHE);
    let downloads = cache.join("downloads");
    ensure!(
        count_files(&downloads, &any_file)? >= 2,
        "PlatformIO cached no download"
    );
    let installed = home(".platformio/packages/tool-demo/payload.bin");
    ensure!(installed.is_file(), "PlatformIO did not install tool-demo");
    let alt = home("alt/pio-cache");
    let alt_files = count_files(&alt, &any_file)?;
    ensure!(
        alt_files > 0,
        "the tool wrote nothing to its non-default cache {}",
        alt.display()
    );

    let sel = Selection {
        binary,
        profile: "platformio-cache",
        roots: Some(HOME),
        path: HOME,
        config: None,
        operation: PIO_OP,
    };
    plan_then_reclaim(ctx, &sel, &vec![(cache.clone(), None)])?;
    // The prune removes `.cache`; PlatformIO's telemetry (on by default,
    // as in diskgc's environment) may write `.cache/telemetry.json` again.
    ensure!(
        !exists(&downloads),
        "pio system prune --cache left {}",
        downloads.display()
    );
    ensure!(
        count_files(&alt, &any_file)? == alt_files,
        "the PLATFORMIO_CACHE_DIR cache changed"
    );
    expect_kept(&[&decoy, &installed])?;

    let archive = "/pio/tool-demo-1.0.0.tar.gz";
    let fetched = registry.served(archive)?;
    tool(
        ctx,
        Path::new(HOME),
        PIO_ENV,
        PIO,
        &["pkg", "uninstall", "--global", "--tool", "tool-demo"],
    )?;
    pio_install(ctx, &[], "tool-demo-1.0.0.tar.gz")?;
    ensure!(
        registry.served(archive)? > fetched && count_files(&downloads, &any_file)? > 0,
        "PlatformIO did not fetch and cache tool-demo again"
    );

    let args = pio_install_args("tool-gated-GATE-1.0.0.tar.gz");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut install = InFlight::start(ctx, "pio-install", Path::new(HOME), PIO_ENV, PIO, &args)?;
    registry.wait_gated(&mut install)?;
    let reclaimed = reclaim_in_flight(ctx, &sel, &[&cache])?;
    expect_in_use_by(
        &reclaimed,
        &cache,
        &install,
        &["fd"],
        "download lock",
        &|p| p.parent() == Some(downloads.as_path()) && p.to_string_lossy().ends_with(".lock"),
    )?;
    let prune_env: Vec<(&str, &str)> = PIO_ENV
        .iter()
        .chain(&[("PLATFORMIO_CACHE_DIR", ".cache")])
        .copied()
        .collect();
    tool(
        ctx,
        &home(".platformio"),
        &prune_env,
        PIO,
        &["system", "prune", "--cache", "--force"],
    )?;
    ensure!(
        !exists(&cache),
        "pio system prune --cache left {}",
        cache.display()
    );
    release(ctx)?;
    let (status, output) = install.finish()?;
    ensure!(
        !status.success() && output.contains("FileNotFoundError"),
        "the install in flight survived pio system prune ({status}); PlatformIO's prune now \
         respects its download lock? Update the platformio-cache liveness row:\n{output}"
    );
    pio_install(ctx, &[], "tool-gated-GATE-1.0.0.tar.gz")?;
    expect_kept(&[&installed])?;
    Ok(())
}

// ---- Jest ------------------------------------------------------------------

/// The `TMPDIR` of a shell in the temp-prefix v1 preset (`nix-shell.*`).
const SHELL_TMP: &str = "/tmp/nix-shell.Ab1";

fn jest_project(ctx: &Ctx) -> TestResult {
    write(
        ctx,
        "src/jproj/package.json",
        br#"{"name":"jproj","version":"1.0.0"}"#,
    )?;
    for n in 1..=3 {
        write(
            ctx,
            &format!("src/jproj/m{n}.js"),
            format!("module.exports = () => {n};\n").as_bytes(),
        )?;
        write(
            ctx,
            &format!("src/jproj/m{n}.test.js"),
            format!(
                "const m = require('./m{n}');\ntest('m{n}', () => {{ expect(m()).toBe({n}); }});\n"
            )
            .as_bytes(),
        )?;
    }
    // Runs only when asked for (`slow`): stops until the gate opens.
    write(
        ctx,
        "src/jproj/slow.test.js",
        b"const fs = require('fs');\ntest('slow', async () => {\n  fs.writeFileSync('/home/tester/ctl/jest.hit', '');\n  while (!fs.existsSync('/home/tester/ctl/gate.go')) await new Promise((r) => setTimeout(r, 50));\n  expect(require('./m1')()).toBe(1);\n}, 120000);\n",
    )?;
    Ok(())
}

/// Jest's default cache directory under `tmpdir` (`--showConfig`).
fn jest_default_cache(ctx: &Ctx, tmpdir: &str) -> Result<PathBuf, String> {
    jest_default_cache_in(ctx, "src/jproj", tmpdir)
}

/// Jest's default cache directory for the project at `project` (below
/// tester's home) under `tmpdir`.
fn jest_default_cache_in(ctx: &Ctx, project: &str, tmpdir: &str) -> Result<PathBuf, String> {
    let out = tool(
        ctx,
        &home(project),
        &[("TMPDIR", tmpdir)],
        JEST,
        &["--showConfig"],
    )?;
    let config: Value =
        serde_json::from_str(&out).map_err(|e| format!("parse jest --showConfig: {e}: {out}"))?;
    let dir = config["configs"][0]["cacheDirectory"]
        .as_str()
        .ok_or("jest reported no cacheDirectory")?;
    Ok(PathBuf::from(dir))
}

fn jest_run(ctx: &Ctx, env: &[(&str, &str)], extra: &[&str]) -> TestResult {
    let args: Vec<&str> = ["--ci", "--testPathIgnorePatterns", "slow"]
        .iter()
        .chain(extra)
        .copied()
        .collect();
    tool(ctx, &home("src/jproj"), env, JEST, &args)?;
    Ok(())
}

/// `jest-cache`: real Jest runs fill their default cache under `TMPDIR`:
/// `/tmp/jest_<uid in base 36>` and, from a shell whose `TMPDIR` is a
/// `nix-shell.*` directory (the temp-prefix v1 preset), the same name in
/// it. A real cache at a nested `--cacheDirectory` (`/tmp/sub/jest_custom`)
/// and a lookalike `/tmp/jest` are never found; `jest --clearCache`
/// removes each measured directory; the next run caches again. A run held
/// in flight (a test gated) holds nothing open in the cache, so diskgc
/// clears it; this run's held test then loads a module, which recreates
/// the cache directory, and the run passes and writes its cache again.
/// Jest has no cache lock: a run that loads no module after the clear
/// fails
/// ([`jest_cache_clear_under_a_run_with_no_later_module_load_fails_the_run`]).
pub fn jest_cache_real_jest_runs_reclaim_then_rerun_and_a_run_in_flight_completes(
    ctx: &Ctx,
) -> TestResult {
    jest_scenario(ctx, &ctx.diskgc())
}

fn jest_scenario(ctx: &Ctx, binary: &Path) -> TestResult {
    // The Jest 30.2.0 package prints 30.1.3 (its lockfile pins jest,
    // jest-cli and @jest/core at 30.2.0).
    versions(ctx, &[(JEST, &["--version"], "30.1.3")])?;
    control_dir(ctx)?;
    jest_project(ctx)?;
    ctx.scratch.mkdirs("nix-shell.Ab1", 0o700, TESTER)?;
    ctx.scratch.mkdirs("sub", 0o700, TESTER)?;
    let default = jest_default_cache(ctx, "/tmp")?;
    let shell = jest_default_cache(ctx, SHELL_TMP)?;
    ensure!(
        default.parent() == Some(Path::new("/tmp")) && shell.parent() == Some(Path::new(SHELL_TMP)),
        "Jest's default cache is not directly in TMPDIR: {} and {}",
        default.display(),
        shell.display()
    );
    jest_run(ctx, &[("TMPDIR", "/tmp")], &[])?;
    jest_run(ctx, &[("TMPDIR", SHELL_TMP)], &[])?;
    jest_run(ctx, &[], &["--cacheDirectory", "/tmp/sub/jest_custom"])?;
    ctx.scratch.mkdirs("jest", 0o700, TESTER)?;
    let decoy = ctx
        .scratch
        .write_file("jest/lookalike.bin", &payload(4096), 0o600, TESTER)?;
    let custom = PathBuf::from("/tmp/sub/jest_custom");
    let custom_files = count_files(&custom, &any_file)?;
    ensure!(
        custom_files > 0,
        "Jest wrote nothing to its --cacheDirectory cache"
    );
    for cache in [&default, &shell] {
        ensure!(
            count_files(cache, &any_file)? > 0,
            "Jest cached nothing in {}",
            cache.display()
        );
    }

    let sel = Selection {
        binary,
        profile: "jest-cache",
        roots: None,
        path: "/tmp",
        config: None,
        operation: JEST_OP,
    };
    plan_then_reclaim(
        ctx,
        &sel,
        // The profile's other fixed root, `/var/tmp`, is another
        // filesystem here: its own plan row, `other_fs`.
        &vec![
            (default.clone(), None),
            (shell.clone(), None),
            (PathBuf::from("/var/tmp"), Some("other_fs")),
        ],
    )?;
    ensure!(
        !exists(&default) && !exists(&shell),
        "jest --clearCache left a cache"
    );
    ensure!(
        count_files(&custom, &any_file)? == custom_files,
        "the --cacheDirectory cache changed"
    );
    expect_kept(&[&decoy])?;
    jest_run(ctx, &[("TMPDIR", "/tmp")], &[])?;
    ensure!(
        count_files(&default, &any_file)? > 0,
        "Jest did not cache again"
    );

    let mut run = InFlight::start(
        ctx,
        "jest-run",
        &home("src/jproj"),
        &[("TMPDIR", "/tmp")],
        JEST,
        &["--ci", "--runInBand"],
    )?;
    wait_file(&mut run, &home(&format!("{CTL}/jest.hit")))?;
    let reclaimed = reclaim_in_flight(ctx, &sel, &[&default])?;
    expect_actions(&reclaimed, &[&default])?;
    ensure!(
        !exists(&default),
        "jest --clearCache under the run in flight left its cache"
    );
    release(ctx)?;
    let output = run.expect_success()?;
    ensure!(
        output.contains("4 passed"),
        "the run in flight did not pass its four tests:\n{output}"
    );
    ensure!(
        count_files(&default, &any_file)? > 0,
        "the run in flight did not write its cache again"
    );
    Ok(())
}

/// `jest-cache`'s counterexample (Codex review of TASK-360): a run in
/// flight whose test, held after Jest transformed and cached everything it
/// needs, loads no module after the clear. diskgc clears the cache (the run
/// holds nothing open in it), the test then passes, and Jest exits 1: at
/// its end it writes its test timings (`perf-cache-*`) into the cache
/// directory the clear removed, without creating it. A run that loads a
/// module after the clear recreates the directory first and passes
/// ([`jest_cache_real_jest_runs_reclaim_then_rerun_and_a_run_in_flight_completes`]),
/// so Jest needs a lock against its own `--clearCache` and has none.
pub fn jest_cache_clear_under_a_run_with_no_later_module_load_fails_the_run(
    ctx: &Ctx,
) -> TestResult {
    versions(ctx, &[(JEST, &["--version"], "30.1.3")])?;
    control_dir(ctx)?;
    write(
        ctx,
        "src/jidle/package.json",
        br#"{"name":"jidle","version":"1.0.0"}"#,
    )?;
    // Requires nothing after the gate: no transform, no cache write.
    write(
        ctx,
        "src/jidle/idle.test.js",
        b"const fs = require('fs');\ntest('idle', async () => {\n  fs.writeFileSync('/home/tester/ctl/jest.hit', '');\n  while (!fs.existsSync('/home/tester/ctl/gate.go')) await new Promise((r) => setTimeout(r, 50));\n}, 120000);\n",
    )?;
    let cache = jest_default_cache_in(ctx, "src/jidle", "/tmp")?;
    // Space no profile frees, so the reclaim cannot meet its goal on a
    // near-empty /tmp and runs the cleaner once (exhausted, exit 1).
    ctx.scratch
        .write_file("filler.bin", &payload(1 << 20), 0o600, TESTER)?;
    let sel = Selection {
        binary: &ctx.diskgc(),
        profile: "jest-cache",
        roots: None,
        path: "/tmp",
        config: None,
        operation: JEST_OP,
    };
    let mut run = InFlight::start(
        ctx,
        "jest-idle",
        &home("src/jidle"),
        &[("TMPDIR", "/tmp")],
        JEST,
        &["--ci", "--runInBand"],
    )?;
    wait_file(&mut run, &home(&format!("{CTL}/jest.hit")))?;
    ensure!(
        count_files(&cache, &any_file)? > 0,
        "the run in flight cached nothing in {}",
        cache.display()
    );
    let reclaimed = reclaim_in_flight(ctx, &sel, &[&cache])?;
    expect_actions(&reclaimed, &[&cache])?;
    ensure!(
        !exists(&cache),
        "jest --clearCache under the run in flight left its cache"
    );
    release(ctx)?;
    let (status, output) = run.finish()?;
    ensure!(
        output.contains("1 passed"),
        "the held test did not pass:\n{output}"
    );
    ensure!(
        status.code() == Some(1) && output.contains("ENOENT") && output.contains("perf-cache"),
        "the run in flight ended {status} after jest --clearCache, not exit 1 writing its \
         perf-cache into the removed directory; Jest now survives its own clear? Update the \
         jest-cache liveness row:\n{output}"
    );
    Ok(())
}

// ---- a missing cleaner, and no cleaner version check -------------------

/// Each delegated profile's cache, measured path and roots, and what its
/// command must look like when run: the program, its argv after the
/// program (with `{path}` the candidate), its working directory
/// (`candidate` or `parent`) and the environment assignment an `env`
/// wrapper adds.
struct Delegated {
    profile: &'static str,
    cache: &'static str,
    roots: Option<&'static str>,
    path: &'static str,
    program: &'static str,
    argv: &'static [&'static str],
    cwd_parent: bool,
    env: Option<&'static str>,
}

const DELEGATED: &[Delegated] = &[
    Delegated {
        profile: "pip-cache",
        cache: "/home/tester/.cache/pip",
        roots: Some(HOME),
        path: HOME,
        program: "pip",
        argv: &["--cache-dir", "{path}", "cache", "purge"],
        cwd_parent: false,
        env: None,
    },
    Delegated {
        profile: "uv-cache",
        cache: "/home/tester/.cache/uv",
        roots: Some(HOME),
        path: HOME,
        program: "uv",
        argv: &["cache", "clean", "--cache-dir", "{path}"],
        cwd_parent: true,
        env: None,
    },
    Delegated {
        profile: "npm-cache",
        cache: "/home/tester/.npm",
        roots: Some(HOME),
        path: HOME,
        program: "npm",
        argv: &["cache", "clean", "--force", "--cache", "{path}"],
        cwd_parent: false,
        env: None,
    },
    Delegated {
        profile: "pnpm-store",
        cache: "/home/tester/.local/share/pnpm/store",
        roots: Some(HOME),
        path: HOME,
        program: "pnpm",
        argv: &["store", "prune", "--store-dir", "{path}"],
        cwd_parent: false,
        env: None,
    },
    Delegated {
        profile: "yarn-cache",
        cache: "/home/tester/.cache/yarn",
        roots: Some(HOME),
        path: HOME,
        program: "yarn",
        argv: &["cache", "clean"],
        cwd_parent: true,
        env: Some("YARN_CACHE_FOLDER={path}"),
    },
    Delegated {
        profile: "ccache",
        cache: "/home/tester/.cache/ccache",
        roots: Some(HOME),
        path: HOME,
        program: "ccache",
        argv: &["--clear"],
        cwd_parent: false,
        env: Some("CCACHE_DIR={path}"),
    },
    Delegated {
        profile: "go-build-cache",
        cache: "/home/tester/.cache/go-build",
        roots: Some(HOME),
        path: HOME,
        program: "go",
        argv: &["clean", "-cache"],
        cwd_parent: false,
        env: Some("GOCACHE={path}"),
    },
    Delegated {
        profile: "platformio-cache",
        cache: "/home/tester/.platformio/.cache",
        roots: Some(HOME),
        path: HOME,
        program: "pio",
        argv: &["system", "prune", "--cache", "--force"],
        cwd_parent: true,
        env: Some("PLATFORMIO_CACHE_DIR=.cache"),
    },
    Delegated {
        profile: "jest-cache",
        cache: "/tmp/jest_rs",
        roots: None,
        path: "/tmp",
        program: "jest",
        argv: &[
            "--config",
            "{}",
            "--cacheDirectory",
            "{path}",
            "--clearCache",
        ],
        cwd_parent: true,
        env: None,
    },
];

/// A stand-in for every cleaner: logs one line per run (its name, each
/// argument in brackets, its working directory and the variables the
/// profiles set) and answers a version query with a version no profile was
/// verified with; it deletes nothing.
const STAND_IN: &str = "#!/bin/sh\n{\n  printf '%s' \"${0##*/}\"\n  for a in \"$@\"; do printf ' [%s]' \"$a\"; done\n  printf ' cwd=[%s] env=[%s%s%s%s]\\n' \"$(pwd -P)\" \"${YARN_CACHE_FOLDER:+YARN_CACHE_FOLDER=$YARN_CACHE_FOLDER}\" \"${CCACHE_DIR:+CCACHE_DIR=$CCACHE_DIR}\" \"${GOCACHE:+GOCACHE=$GOCACHE}\" \"${PLATFORMIO_CACHE_DIR:+PLATFORMIO_CACHE_DIR=$PLATFORMIO_CACHE_DIR}\"\n} >> /home/tester/ctl/stand-in.log\ncase \"$1\" in --version|-v|version) echo 0.0.1-stand-in;; esac\nexit 0\n";

impl Delegated {
    /// The stand-in's log line for this profile's command on its cache.
    fn logged(&self) -> String {
        let cache = Path::new(self.cache);
        let args: String = self
            .argv
            .iter()
            .map(|a| format!(" [{}]", a.replace("{path}", self.cache)))
            .collect();
        let cwd = if self.cwd_parent {
            cache.parent().unwrap_or(cache)
        } else {
            cache
        };
        let env = self
            .env
            .map_or(String::new(), |e| e.replace("{path}", self.cache));
        format!("{}{args} cwd=[{}] env=[{env}]", self.program, cwd.display())
    }
}

/// What the delegated profiles do about the cleaner itself: diskgc checks no
/// cleaner's version. With `PATH` holding only `env`, every profile's
/// cleaner is not found: its tier is `missing` (skipped, not an error) and
/// nothing changes. With stand-ins on `PATH` that would answer a version
/// query with `0.0.1-stand-in`, diskgc runs each one exactly once, without
/// asking its version, with the profile's argv, working directory and
/// environment on the measured cache. A cleaner of any version, or any
/// program of its name first on `PATH`, is trusted (docs/safety.md,
/// Command actions); a cleaner version allowlist does not exist.
pub fn delegated_cleaners_skip_a_missing_tool_and_run_any_version_on_path(ctx: &Ctx) -> TestResult {
    control_dir(ctx)?;
    ctx.home.mkdirs("env-only", 0o755, TESTER)?;
    ctx.home.symlink("env-only/env", Path::new("/bin/env"))?;
    ctx.home.mkdirs("stand-ins", 0o755, TESTER)?;
    ctx.home.symlink("stand-ins/env", Path::new("/bin/env"))?;
    for d in DELEGATED {
        let script = ctx.home.write_file(
            &format!("stand-ins/{}", d.program),
            STAND_IN.as_bytes(),
            0o755,
            TESTER,
        )?;
        info!("stand-in {}", script.display());
        let payload_file = format!("{}/payload.bin", d.cache);
        if let Some(rel) = d.cache.strip_prefix("/home/tester/") {
            ctx.home.mkdirs(rel, 0o755, TESTER)?;
            ctx.home
                .write_file(&format!("{rel}/payload.bin"), &payload(4096), 0o644, TESTER)?;
        } else if let Some(rel) = d.cache.strip_prefix("/tmp/") {
            ctx.scratch.mkdirs(rel, 0o700, TESTER)?;
            ctx.scratch
                .write_file(&format!("{rel}/payload.bin"), &payload(4096), 0o600, TESTER)?;
        }
        ensure!(
            Path::new(&payload_file).is_file(),
            "no fixture at {payload_file}"
        );
    }
    let env_only = [("PATH".to_owned(), "/home/tester/env-only".to_owned())];
    let stand_ins = [("PATH".to_owned(), "/home/tester/stand-ins".to_owned())];
    let log = home(&format!("{CTL}/stand-in.log"));
    for d in DELEGATED {
        let select = format!("profile:{}", d.profile);
        let oracle = Oracle::arm(Fixture::new())?;
        let missing = ensure_with(
            ctx,
            &ctx.diskgc(),
            &env_only,
            false,
            &select,
            d.roots,
            d.path,
            None,
        )?;
        oracle.judge(&missing)?.assert_clean()?;
        missing.expect_code(1)?;
        let missing_report = report(&missing)?;
        let refused = missing_report["skipped"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| {
                s["profile"] == d.profile && s["reason"] == "missing" && s["candidate"].is_null()
            });
        ensure!(
            refused && actions(&missing_report)?.is_empty(),
            "{}: without its cleaner on PATH, expected a missing tier: {missing_report}",
            d.profile
        );
        ensure!(
            !exists(&log),
            "{}: a stand-in ran with only env on PATH",
            d.profile
        );

        let oracle = Oracle::arm(Fixture::new())?;
        let ran = ensure_with(
            ctx,
            &ctx.diskgc(),
            &stand_ins,
            false,
            &select,
            d.roots,
            d.path,
            None,
        )?;
        oracle.judge(&ran)?.assert_clean()?;
        ran.expect_code(1)?;
        expect_actions(&report(&ran)?, &[Path::new(d.cache)])?;
        let lines =
            std::fs::read_to_string(&log).map_err(|e| format!("read {}: {e}", log.display()))?;
        ensure!(
            lines.lines().collect::<Vec<_>>() == [d.logged()],
            "{}: the stand-in logged {lines:?}, expected exactly [{:?}] (no version query)",
            d.profile,
            d.logged()
        );
        ctx.home.remove_file(&format!("{CTL}/stand-in.log"))?;
        ensure!(
            Path::new(&format!("{}/payload.bin", d.cache)).is_file(),
            "{}: the stand-in run changed the cache",
            d.profile
        );
    }
    Ok(())
}
