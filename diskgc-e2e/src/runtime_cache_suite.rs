//! TASK-352: admission evidence for the language and runtime cache built-ins
//! (`go-mod-cache`, `bun-cache`, `node-gyp-headers`,
//! `electron-download-cache`, `huggingface-hub-repos`, `cargo-registry-src`,
//! `nix-user-cache`), docs/testing.md, Admission evidence.
//!
//! All seven are plain deletes of a cache below a supplied home root. Per
//! profile, the pinned real tool fills its cache from a local server or file
//! tree the test makes (the containers have no network): a Go module proxy
//! tree, an npm registry, a Node.js headers dist, an Electron mirror, a
//! Hugging Face hub endpoint, a sparse Cargo registry, and a dumb-HTTP Git
//! repository and tarball for Nix. A real cache at the tool's own
//! non-default location (its environment variable or option) and hand-made
//! lookalikes are the decoys. A dry run plans exactly the expected set, a
//! reclaim under the deletion oracle deletes exactly its reclaimable part,
//! and the tool then makes the cache again. The live counterexample runs the
//! tool in or against the cache during the reclaim: where the tool holds
//! something there diskgc keeps the candidate `in_use` (the blockers must be
//! processes of that run, which `/proc` shows holding a file, a mapping or
//! its working directory there); where it holds nothing, the reclaim
//! deletes the cache under it, and what the run then does is the named
//! residual of its liveness row (docs/reference.md, Liveness contracts).
//!
//! Four of the profiles have a one-hour floor (`min_idle_for: 1h`) and
//! every cache here is made now, so their reclaims advance the sandboxed
//! binary's clock past it ([`PAST_THE_HOUR`]); without the offset a fresh
//! entry is `not_idle`, which the `node-gyp-headers` scenario shows once.
//!
//! [`candidate_catalog_runtime_cache_family_matches_the_development_build`]
//! runs one fixture of all seven through the release build's catalog code
//! path with the family in its catalog (docs/testing.md, the candidate
//! catalog).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::admission::{
    CTL, Expected, HOME, InFlight, NODE, REG, REGISTRY, Registry, check_candidate_catalog,
    control_dir, count_files, cwds_below, dry_run_then_reclaim, exists, expect_in_use_by,
    expect_kept, expect_used_by, held_by, home, lookalike, npm_packages, open_gate, plan_of,
    python, release, run_ensure, sorted, tool, versions, wait_file, write, write_script,
};
use crate::cargo_under_test::Toolchain;
use crate::differential_suite::SandboxClockOffset;
use crate::ensure_suite::report;
use crate::oracle::{Fixture, Oracle};
use crate::proc_locks::Watched;
use crate::suite::{Ctx, TestResult};
use crate::{ensure, info};

/// The family this suite builds fixtures for (the candidate catalog's
/// check, [`crate::admission::check_candidate_catalog`]).
pub(crate) const FAMILY: &[&str] = &[
    "go-mod-cache",
    "bun-cache",
    "node-gyp-headers",
    "electron-download-cache",
    "huggingface-hub-repos",
    "cargo-registry-src",
    "nix-user-cache",
];

/// Past the one-hour `min_idle_for` of `node-gyp-headers`,
/// `electron-download-cache`, `huggingface-hub-repos` and
/// `cargo-registry-src`.
const PAST_THE_HOUR: Duration = Duration::from_secs(2 * 3600);

/// The projects that use the caches (not a root of any profile here).
const SRC: &str = "src";

/// The pinned Go of the Nix image (flake.nix: `cacheTools`) and Nix (its
/// `/opt/nix-eval` link); the other tools are links of the under-test
/// bundle into the store closure the container mounts read-only.
const GO: &str = "/bin/go";
const NIX: &str = "/opt/nix-eval/nix";
const CC_BIN: &str = "/opt/cargo-under-test/cc/bin";

fn bun(ctx: &Ctx) -> PathBuf {
    ctx.bin_dir.join("bun")
}

fn hf_python(ctx: &Ctx) -> PathBuf {
    ctx.bin_dir.join("hf-python3")
}

fn node_gyp(ctx: &Ctx) -> PathBuf {
    ctx.bin_dir.join("node-gyp.js")
}

fn bin_dir(ctx: &Ctx) -> Result<&str, String> {
    ctx.bin_dir
        .to_str()
        .ok_or_else(|| "non-UTF-8 bin dir".to_owned())
}

fn select(profile: &str) -> String {
    format!("profile:{profile}")
}

/// A dry run and a reclaim of one profile below tester's home with the
/// development build.
fn reclaim(ctx: &Ctx, profile: &str, expected: &Expected) -> Result<Value, String> {
    reclaim_with(ctx, &ctx.diskgc(), profile, expected)
}

/// [`reclaim`] with `binary` (the release candidate's, TASK-370).
fn reclaim_with(
    ctx: &Ctx,
    binary: &Path,
    profile: &str,
    expected: &Expected,
) -> Result<Value, String> {
    dry_run_then_reclaim(ctx, binary, &select(profile), Some(HOME), HOME, expected)
}

/// The one entry of `dir` (a tool-named directory such as Cargo's
/// registry directory or a cache's hash directory).
fn only_entry(dir: &Path) -> Result<PathBuf, String> {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("list {}: {e}", dir.display()))?
        .map(|e| e.map(|e| e.path()).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    match entries.as_slice() {
        [one] => Ok(one.clone()),
        other => Err(format!(
            "expected one entry in {}, found {other:?}",
            dir.display()
        )),
    }
}

// ---- go-mod-cache ----------------------------------------------------------

/// Two modules in a GOPROXY file tree (the module proxy protocol: `list`,
/// `.info`, `.mod`, `.zip`), which `GOPROXY=file://` serves to the real
/// `go` command.
const MKGOPROXY_PY: &str = r#"
import json, os, sys, zipfile
root = sys.argv[1]
for name, body in [("lib", "func Answer() int { return 42 }\n"), ("util", "func Twice(x int) int { return 2 * x }\n")]:
    path, at = f"example.com/{name}", f"example.com/{name}@v1.0.0"
    d = os.path.join(root, path, "@v")
    os.makedirs(d, exist_ok=True)
    mod = f"module {path}\n\ngo 1.26\n"
    open(os.path.join(d, "list"), "w").write("v1.0.0\n")
    json.dump({"Version": "v1.0.0", "Time": "2026-01-01T00:00:00Z"}, open(os.path.join(d, "v1.0.0.info"), "w"))
    open(os.path.join(d, "v1.0.0.mod"), "w").write(mod)
    with zipfile.ZipFile(os.path.join(d, "v1.0.0.zip"), "w") as z:
        z.writestr(f"{at}/go.mod", mod)
        z.writestr(f"{at}/{name}.go", f"package {name}\n\n{body}")
"#;

/// The module proxy tree below [`REG`] (`file://`, no server).
const GO_PROXY: &str = "file:///home/tester/reg/goproxy";
const GO_ENV: &[(&str, &str)] = &[
    ("GOPROXY", GO_PROXY),
    ("GOSUMDB", "off"),
    ("GOFLAGS", "-mod=mod"),
    ("GOTOOLCHAIN", "local"),
    ("GOTELEMETRY", "off"),
    ("CGO_ENABLED", "0"),
];

fn go_fixture(ctx: &Ctx) -> TestResult {
    write(ctx, &format!("{CTL}/mkgoproxy.py"), MKGOPROXY_PY.as_bytes())?;
    tool(
        ctx,
        Path::new(HOME),
        &[],
        python(ctx)?,
        &[
            &format!("{HOME}/{CTL}/mkgoproxy.py"),
            &format!("{HOME}/{REG}/goproxy"),
        ],
    )?;
    write(
        ctx,
        &format!("{SRC}/goapp/go.mod"),
        b"module example.com/goapp\n\ngo 1.26\n\nrequire (\n\texample.com/lib v1.0.0\n\texample.com/util v1.0.0\n)\n",
    )?;
    write(
        ctx,
        &format!("{SRC}/goapp/main.go"),
        b"package main\n\nimport (\n\t\"fmt\"\n\n\t\"example.com/lib\"\n\t\"example.com/util\"\n)\n\nfunc main() { fmt.Println(util.Twice(lib.Answer())) }\n",
    )?;
    // Stops the compile of example.com/util until the gate opens.
    write_script(
        ctx,
        "gated-go/toolexec",
        b"#!/bin/sh\ncase \"$*\" in *\"-p example.com/util \"*) : > /home/tester/ctl/go.hit\n  while [ ! -e /home/tester/ctl/gate.go ]; do sleep 0.1; done;; esac\nexec \"$@\"\n",
    )
}

/// `go build` of the project with `env` added, then runs it (prints 84).
fn go_build(ctx: &Ctx, env: &[(&str, &str)]) -> Result<String, String> {
    let env: Vec<(&str, &str)> = GO_ENV.iter().chain(env).copied().collect();
    let project = home(&format!("{SRC}/goapp"));
    tool(ctx, &project, &env, GO, &["build", "-o", "app", "."])?;
    let out = tool(ctx, &project, &[], project.join("app"), &[])?;
    Ok(out)
}

fn go_module_files(cache: &Path) -> Result<usize, String> {
    count_files(cache, &|name| {
        name.ends_with(".go") || name.ends_with(".zip")
    })
}

/// docs/testing.md, Admission evidence (`go-mod-cache`): a real `go
/// build` downloads two modules from a module proxy tree into `~/go/pkg/mod`
/// (read-only directories, as Go makes them). A real cache at `GOMODCACHE`
/// and lookalikes outside the profile's anchor are never found; the reclaim
/// deletes the whole cache (`make_writable`). Without the proxy
/// (`GOPROXY=off`) the next build fails (the row's network residual), with
/// it Go downloads again. A build in flight (`-toolexec` holds the compile
/// of `example.com/util`, whose sources it reads only then) holds no file,
/// mapping or working directory in the cache, but the go command passes
/// `GOMODCACHE` to every tool it runs: diskgc keeps the cache `in_use` by
/// that subprocess's initial environment (`environment`), and the build
/// then succeeds. Between its subprocesses only the go command runs, which
/// names nothing (the row's residual, not reproduced here).
pub fn go_mod_cache_real_go_downloads_reclaim_then_redownload_and_a_build_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    go_mod_scenario(ctx, &ctx.diskgc())
}

/// TASK-370: [`go_mod_cache_real_go_downloads_reclaim_then_redownload_and_a_build_in_flight_is_kept`] through the gate-lifted release-candidate
/// package, whose 0.2 catalog holds `go-mod-cache`.
pub fn release_candidate_go_mod_cache_reclaim_then_redownload_and_a_build_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "go-mod-cache")?;
    go_mod_scenario(ctx, &binary)
}

fn go_mod_scenario(ctx: &Ctx, binary: &Path) -> TestResult {
    versions(ctx, &[(GO, &["version"], "go1.26.8 ")])?;
    control_dir(ctx)?;
    go_fixture(ctx)?;
    let out = go_build(ctx, &[])?;
    ensure!(out.trim() == "84", "the Go program printed {out:?}");
    let cache = home("go/pkg/mod");
    ensure!(go_module_files(&cache)? > 0, "go downloaded nothing");
    go_build(ctx, &[("GOMODCACHE", "/home/tester/alt/gomod")])?;
    let alt = home("alt/gomod");
    let alt_files = go_module_files(&alt)?;
    ensure!(alt_files > 0, "the GOMODCACHE cache is empty");
    let decoys = [
        lookalike(ctx, &format!("{SRC}/proj/go/pkg/mod/example.com/x@v1.0.0"))?,
        lookalike(ctx, "go/pkg/mod.old")?,
    ];

    reclaim_with(ctx, binary, "go-mod-cache", &vec![(cache.clone(), None)])?;
    ensure!(
        go_module_files(&alt)? == alt_files,
        "the GOMODCACHE cache changed"
    );
    expect_kept(&[&decoys[0], &decoys[1]])?;
    let offline = tool(
        ctx,
        &home(&format!("{SRC}/goapp")),
        &[GO_ENV, &[("GOPROXY", "off")]].concat(),
        GO,
        &["build", "-o", "app", "."],
    );
    info!("go build with GOPROXY=off after the reclaim: {offline:?}");
    ensure!(
        offline.is_err_and(|e| e.contains("GOPROXY=off")),
        "a build without the proxy did not fail on the missing modules"
    );
    go_build(ctx, &[])?;
    ensure!(go_module_files(&cache)? > 0, "go did not download again");

    let env: Vec<(&str, &str)> = GO_ENV
        .iter()
        .copied()
        .chain([("GOCACHE", "/home/tester/alt/gocache-live")])
        .collect();
    let mut build = InFlight::start(
        ctx,
        "go-build",
        &home(&format!("{SRC}/goapp")),
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
    let held = held_by(&build, &cache)?;
    let cwds = cwds_below(&build, &cache)?;
    info!("the build in flight holds {held:?} and has its working directories {cwds:?} there");
    ensure!(
        held.is_empty() && cwds.is_empty(),
        "precondition: the build in flight holds files or working directories in the module \
         cache ({held:?}, {cwds:?}); update the scenario"
    );
    let reclaimed = reclaim_with(
        ctx,
        binary,
        "go-mod-cache",
        &vec![(cache.clone(), Some("in_use"))],
    )?;
    expect_used_by(&reclaimed, &cache, &build, &["environment"], &|u| {
        u == "GOMODCACHE=/home/tester/go/pkg/mod"
    })?;
    release(ctx)?;
    build.expect_success()?;
    let out = tool(
        ctx,
        &home(&format!("{SRC}/goapp")),
        &[],
        home(&format!("{SRC}/goapp/app2")),
        &[],
    )?;
    ensure!(
        out.trim() == "84",
        "the build in flight's program printed {out:?}"
    );
    Ok(())
}

// ---- bun-cache -------------------------------------------------------------

const BUN_ENV: &[(&str, &str)] = &[("BUN_CONFIG_REGISTRY", REGISTRY)];

/// A Bun project `src/NAME` depending on `deps` (all `1.0.0`).
fn bun_project(ctx: &Ctx, name: &str, deps: &[&str]) -> Result<PathBuf, String> {
    let deps: Vec<String> = deps.iter().map(|d| format!(r#""{d}":"1.0.0""#)).collect();
    write(
        ctx,
        &format!("{SRC}/{name}/package.json"),
        format!(
            r#"{{"name":"{name}","version":"1.0.0","dependencies":{{{}}}}}"#,
            deps.join(",")
        )
        .as_bytes(),
    )?;
    Ok(home(&format!("{SRC}/{name}")))
}

fn bun_install(ctx: &Ctx, project: &Path, env: &[(&str, &str)]) -> TestResult {
    let env: Vec<(&str, &str)> = BUN_ENV.iter().chain(env).copied().collect();
    tool(ctx, project, &env, bun(ctx), &["install"])?;
    Ok(())
}

fn node_loads(ctx: &Ctx, project: &Path, dep: &str) -> TestResult {
    let out = tool(
        ctx,
        project,
        &[],
        NODE,
        &["-e", &format!("console.log(require('{dep}'))")],
    )?;
    ensure!(
        out.trim() == dep,
        "{}: require('{dep}') printed {out:?}",
        project.display()
    );
    Ok(())
}

/// docs/testing.md, Admission evidence (`bun-cache`): real `bun install`s
/// from a local registry fill the default cache `~/.bun/install/cache` and,
/// with `BUN_INSTALL_CACHE_DIR` set to it, the second matched path
/// `~/.cache/.bun/install/cache`. A real cache at another
/// `BUN_INSTALL_CACHE_DIR` and lookalikes are never found. Both caches are
/// reclaimed; the installed `node_modules` still load (Bun's default
/// backend on Linux hardlinks from the cache), and the next install fetches
/// again. An install in flight (its tarball request held at the registry)
/// holds the cache directory open, so diskgc keeps that cache `in_use`
/// (`fd`) and reclaims the other; the install then completes.
pub fn bun_cache_real_bun_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    bun_scenario(ctx, &ctx.diskgc())
}

/// TASK-370: [`bun_cache_real_bun_installs_reclaim_then_refetch_and_an_install_in_flight_is_kept`] through the gate-lifted release-candidate
/// package, whose 0.2 catalog holds `bun-cache`.
pub fn release_candidate_bun_cache_reclaim_then_refetch_and_an_install_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "bun-cache")?;
    bun_scenario(ctx, &binary)
}

fn bun_scenario(ctx: &Ctx, binary: &Path) -> TestResult {
    versions(
        ctx,
        &[(bun(ctx).to_str().ok_or("bun")?, &["--version"], "1.4.2")],
    )?;
    control_dir(ctx)?;
    let registry = Registry::start(ctx, "gamma/-/")?;
    npm_packages(ctx, &["alpha", "beta", "gamma"])?;
    let default_project = bun_project(ctx, "bunapp", &["alpha", "beta"])?;
    bun_install(ctx, &default_project, &[])?;
    let second = home(".cache/.bun/install/cache");
    let second_project = bun_project(ctx, "bunapp2", &["alpha"])?;
    bun_install(
        ctx,
        &second_project,
        &[(
            "BUN_INSTALL_CACHE_DIR",
            "/home/tester/.cache/.bun/install/cache",
        )],
    )?;
    let alt_project = bun_project(ctx, "bunapp3", &["beta"])?;
    bun_install(
        ctx,
        &alt_project,
        &[("BUN_INSTALL_CACHE_DIR", "/home/tester/alt/bun-cache")],
    )?;
    let cache = home(".bun/install/cache");
    let alt = home("alt/bun-cache");
    for dir in [&cache, &second, &alt] {
        ensure!(
            count_files(dir, &|n| n == "payload.bin")? > 0,
            "bun cached no package in {}",
            dir.display()
        );
    }
    let alt_files = count_files(&alt, &|_| true)?;
    let decoys = [
        lookalike(ctx, &format!("{SRC}/x/.bun/install/cache"))?,
        lookalike(ctx, ".bun/install/cache-old")?,
    ];

    reclaim_with(
        ctx,
        binary,
        "bun-cache",
        &vec![(cache.clone(), None), (second.clone(), None)],
    )?;
    ensure!(
        count_files(&alt, &|_| true)? == alt_files,
        "the other BUN_INSTALL_CACHE_DIR cache changed"
    );
    expect_kept(&[&decoys[0], &decoys[1]])?;
    node_loads(ctx, &default_project, "alpha")?;
    node_loads(ctx, &second_project, "alpha")?;
    let before = registry.served("/alpha/-/alpha-1.0.0.tgz")?;
    let again = bun_project(ctx, "bunapp4", &["alpha"])?;
    bun_install(ctx, &again, &[])?;
    ensure!(
        registry.served("/alpha/-/alpha-1.0.0.tgz")? > before,
        "bun did not fetch alpha again"
    );
    bun_install(
        ctx,
        &again,
        &[(
            "BUN_INSTALL_CACHE_DIR",
            "/home/tester/.cache/.bun/install/cache",
        )],
    )?;

    let live_project = bun_project(ctx, "bunlive", &["alpha", "gamma"])?;
    let env: Vec<(&str, &str)> = BUN_ENV.to_vec();
    let bun_path = bun(ctx);
    let mut install = InFlight::start(
        ctx,
        "bun-install",
        &live_project,
        &env,
        bun_path.to_str().ok_or("bun")?,
        &["install"],
    )?;
    registry.wait_gated(&mut install)?;
    let reclaimed = reclaim_with(
        ctx,
        binary,
        "bun-cache",
        &vec![(cache.clone(), Some("in_use")), (second.clone(), None)],
    )?;
    expect_in_use_by(
        &reclaimed,
        &cache,
        &install,
        &["fd"],
        "cache directory",
        &|p| p == cache,
    )?;
    release(ctx)?;
    install.expect_success()?;
    node_loads(ctx, &live_project, "gamma")?;
    Ok(())
}

// ---- node-gyp-headers ------------------------------------------------------

/// A Node.js headers dist (`dist/vVERSION/node-vVERSION-headers.tar.gz` and
/// its `SHASUMS256.txt`, as nodejs.org serves them) for each version, made
/// from the image's Node headers (symlinks resolved; the real tarball has
/// none).
const MKHEADERS_PY: &str = r#"
import hashlib, os, sys, tarfile
include, dist, versions = sys.argv[1], sys.argv[2], sys.argv[3:]
for v in versions:
    d = os.path.join(dist, f"v{v}")
    os.makedirs(d, exist_ok=True)
    name = f"node-v{v}-headers.tar.gz"
    with tarfile.open(os.path.join(d, name), "w:gz", dereference=True) as t:
        t.add(include, arcname=f"node-v{v}/include/node")
    digest = hashlib.sha256(open(os.path.join(d, name), "rb").read()).hexdigest()
    open(os.path.join(d, "SHASUMS256.txt"), "w").write(f"{digest}  {name}\n")
"#;

/// Header versions the dist serves. The image's Node finds its own headers
/// by its prefix (`use_prefix_to_find_headers`), so node-gyp installs into
/// its cache only for another target, as electron-rebuild and prebuild
/// builds do.
const GYP_TARGET: &str = "24.20.0";
const GYP_OTHER: &str = "24.19.0";

fn gyp_dist() -> String {
    format!("--dist-url={REGISTRY}/dist")
}

/// node-gyp's environment: the C compiler, then `make` and `python3` from
/// the under-test bundle, after busybox (the Makefile's `sed`, `printf`).
fn gyp_env(ctx: &Ctx) -> Result<Vec<(String, String)>, String> {
    Ok(vec![(
        "PATH".to_owned(),
        format!("/bin:{CC_BIN}:{}", bin_dir(ctx)?),
    )])
}

fn node_gyp_run(
    ctx: &Ctx,
    cwd: &Path,
    args: &[&str],
    extra: &[(&str, &str)],
) -> Result<String, String> {
    let env = gyp_env(ctx)?;
    let mut env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    env.extend_from_slice(extra);
    let script = node_gyp(ctx);
    let mut argv = vec![script.to_str().ok_or("node-gyp path")?];
    argv.extend_from_slice(args);
    tool(ctx, cwd, &env, NODE, &argv)
}

/// The headers dist and a two-source N-API addon `src/addon`; the gated
/// compiler stops before compiling `src/b.c` until the gate opens.
fn gyp_fixture(ctx: &Ctx) -> TestResult {
    let node = std::fs::canonicalize(NODE).map_err(|e| format!("resolve {NODE}: {e}"))?;
    let include = node
        .parent()
        .and_then(Path::parent)
        .map(|prefix| prefix.join("include/node"))
        .ok_or("no Node prefix")?;
    ensure!(
        include.join("node_api.h").is_file(),
        "no Node headers at {}",
        include.display()
    );
    write(ctx, &format!("{CTL}/mkheaders.py"), MKHEADERS_PY.as_bytes())?;
    tool(
        ctx,
        Path::new(HOME),
        &[],
        python(ctx)?,
        &[
            &format!("{HOME}/{CTL}/mkheaders.py"),
            include.to_str().ok_or("include path")?,
            &format!("{HOME}/{REG}/dist"),
            GYP_TARGET,
            GYP_OTHER,
        ],
    )?;
    write(
        ctx,
        &format!("{SRC}/addon/binding.gyp"),
        br#"{ "targets": [ { "target_name": "addon", "sources": [ "src/a.c", "src/b.c" ] } ] }"#,
    )?;
    write(
        ctx,
        &format!("{SRC}/addon/package.json"),
        br#"{"name":"addon","version":"1.0.0"}"#,
    )?;
    write(
        ctx,
        &format!("{SRC}/addon/src/a.c"),
        b"#include <node_api.h>\nint a(void) { return 1; }\n",
    )?;
    write(
        ctx,
        &format!("{SRC}/addon/src/b.c"),
        b"#include <node_api.h>\nstatic napi_value init(napi_env env, napi_value exports) { return exports; }\nNAPI_MODULE(NODE_GYP_MODULE_NAME, init)\n",
    )?;
    write_script(
        ctx,
        "gated-gyp/gcc",
        format!(
            "#!/bin/sh\ncase \"$*\" in *src/b.c*) : > /home/tester/ctl/gyp.hit\n  while [ ! -e /home/tester/ctl/gate.go ]; do sleep 0.1; done;; esac\nexec {CC_BIN}/gcc \"$@\"\n"
        )
        .as_bytes(),
    )
}

fn gyp_rebuild(ctx: &Ctx) -> TestResult {
    let addon = home(&format!("{SRC}/addon"));
    node_gyp_run(
        ctx,
        &addon,
        &["rebuild", &format!("--target={GYP_TARGET}"), &gyp_dist()],
        &[],
    )?;
    ensure!(
        addon.join("build/Release/addon.node").is_file(),
        "node-gyp built no addon"
    );
    Ok(())
}

fn gyp_headers(version: &str) -> PathBuf {
    home(&format!(".cache/node-gyp/{version}"))
}

/// docs/testing.md, Admission evidence (`node-gyp-headers`): npm's own
/// node-gyp installs two header versions from a local dist into
/// `~/.cache/node-gyp` and builds a native addon against one. A real
/// install at another `--devdir` and a hand-made `.node-gyp` tree are never
/// found, and a hand-made version directory without `installVersion` is
/// never planned (the profile's `has`). Fresh headers are `not_idle` (the one-hour floor); past it,
/// both versions are reclaimed and the next build installs its version
/// again. A build in flight between two compiles (the gated compiler holds
/// the second) holds nothing in the cache: the reclaim deletes the headers
/// under it and the second compile fails (`node_api.h: No such file or
/// directory`), the row's residual (1); the retried build installs the
/// headers again and succeeds.
pub fn node_gyp_headers_real_node_gyp_install_reclaim_then_reinstall_and_a_build_between_compiles_fails_once(
    ctx: &Ctx,
) -> TestResult {
    versions(
        ctx,
        &[(
            NODE,
            &[node_gyp(ctx).to_str().ok_or("node-gyp")?, "--version"],
            "v12.4.0",
        )],
    )?;
    control_dir(ctx)?;
    let registry = Registry::start(ctx, "")?;
    gyp_fixture(ctx)?;
    for version in [GYP_TARGET, GYP_OTHER] {
        node_gyp_run(
            ctx,
            Path::new(HOME),
            &["install", &format!("--target={version}"), &gyp_dist()],
            &[],
        )?;
        ensure!(
            gyp_headers(version).join("installVersion").is_file()
                && gyp_headers(version)
                    .join("include/node/node_api.h")
                    .is_file(),
            "node-gyp installed no {version} headers"
        );
    }
    gyp_rebuild(ctx)?;
    node_gyp_run(
        ctx,
        Path::new(HOME),
        &[
            "install",
            &format!("--target={GYP_TARGET}"),
            &gyp_dist(),
            "--devdir=/home/tester/alt/node-gyp",
        ],
        &[],
    )?;
    let alt = home(&format!("alt/node-gyp/{GYP_TARGET}/installVersion"));
    ensure!(alt.is_file(), "node-gyp installed nothing at --devdir");
    write(
        ctx,
        ".cache/node-gyp/99.0.0/include/node/node_api.h",
        b"/* by hand */\n",
    )?;
    let decoy = lookalike(ctx, &format!(".node-gyp/{GYP_TARGET}"))?;
    let handmade = home(".cache/node-gyp/99.0.0");

    let fresh: Expected = vec![
        (gyp_headers(GYP_TARGET), Some("not_idle")),
        (gyp_headers(GYP_OTHER), Some("not_idle")),
    ];
    reclaim(ctx, "node-gyp-headers", &fresh)?;
    let expected: Expected = vec![
        (gyp_headers(GYP_TARGET), None),
        (gyp_headers(GYP_OTHER), None),
    ];
    {
        let _clock = SandboxClockOffset::install(PAST_THE_HOUR.as_secs())?;
        reclaim(ctx, "node-gyp-headers", &expected)?;
    }
    expect_kept(&[&alt, &decoy, &handmade])?;
    let tarball = format!("/dist/v{GYP_TARGET}/node-v{GYP_TARGET}-headers.tar.gz");
    let before = registry.served(&tarball)?;
    gyp_rebuild(ctx)?;
    ensure!(
        registry.served(&tarball)? > before
            && gyp_headers(GYP_TARGET).join("installVersion").is_file(),
        "the next build did not install the headers again"
    );

    let env = gyp_env(ctx)?;
    let mut env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    env.push(("CC", "/home/tester/gated-gyp/gcc"));
    let target = format!("--target={GYP_TARGET}");
    let dist = gyp_dist();
    let script = node_gyp(ctx);
    let mut build = InFlight::start(
        ctx,
        "node-gyp-rebuild",
        &home(&format!("{SRC}/addon")),
        &env,
        NODE,
        &[
            script.to_str().ok_or("node-gyp")?,
            "rebuild",
            &target,
            &dist,
        ],
    )?;
    wait_file(&mut build, &home(&format!("{CTL}/gyp.hit")))?;
    let headers = gyp_headers(GYP_TARGET);
    let held = held_by(&build, &headers)?;
    let cwds = cwds_below(&build, &headers)?;
    info!("the build between compiles holds {held:?}, working directories {cwds:?} there");
    ensure!(
        held.is_empty() && cwds.is_empty(),
        "precondition: the build between compiles uses the headers ({held:?}, {cwds:?})"
    );
    {
        let _clock = SandboxClockOffset::install(PAST_THE_HOUR.as_secs())?;
        reclaim(ctx, "node-gyp-headers", &vec![(headers.clone(), None)])?;
    }
    release(ctx)?;
    let (status, output) = build.finish()?;
    ensure!(
        !status.success() && output.contains("node_api.h: No such file or directory"),
        "the build between compiles survived the reclaim of its headers ({status}); update the \
         node-gyp-headers liveness row:\n{output}"
    );
    gyp_rebuild(ctx)?;
    Ok(())
}

// ---- electron-download-cache -----------------------------------------------

/// Electron release zips and their `SHASUMS256.txt` in the mirror layout
/// `electron/vVERSION/` (a stand-in `electron` binary and a payload; what
/// is cached is the download, whatever it holds).
const MKELECTRON_PY: &str = r##"
import hashlib, os, sys, zipfile
root, versions = sys.argv[1], sys.argv[2:]
for v in versions:
    d = os.path.join(root, f"v{v}")
    os.makedirs(d, exist_ok=True)
    name = f"electron-v{v}-linux-x64.zip"
    with zipfile.ZipFile(os.path.join(d, name), "w") as z:
        z.writestr("electron", "#!/bin/sh\necho electron\n")
        z.writestr("version", v)
        z.writestr("payload.bin", bytes(range(256)) * 1200)
    digest = hashlib.sha256(open(os.path.join(d, name), "rb").read()).hexdigest()
    open(os.path.join(d, "SHASUMS256.txt"), "w").write(f"{digest} *{name}\n")
"##;

/// What the electron npm package's install script does: get the zip
/// through @electron/get (its cache; the script passes
/// `electron_config_cache` as the cache root, the library reads no
/// variable), then extract it with extract-zip.
/// `PAUSE=during` stops at the first entry, the zip open; `PAUSE=before`
/// stops between the cache hit and opening the zip; each writes
/// `ctl/electron-PAUSE.ready` and waits for `ctl/electron-PAUSE.go`.
/// Prints the zip's path.
const ELECTRON_INSTALL_JS: &str = r#"
const { downloadArtifact } = require('@electron/get');
const extract = require('extract-zip');
const fs = require('fs');
const [version, dist] = process.argv.slice(2);
const ctl = '/home/tester/ctl';
const pause = new Int32Array(new SharedArrayBuffer(4));
function hold(name) {
  fs.writeFileSync(`${ctl}/${name}.ready`, String(process.pid));
  while (!fs.existsSync(`${ctl}/${name}.go`)) Atomics.wait(pause, 0, 0, 50);
}
(async () => {
  const zip = await downloadArtifact({ version, artifactName: 'electron', platform: 'linux',
    arch: 'x64', mirrorOptions: { mirror: process.env.MIRROR },
    cacheRoot: process.env.electron_config_cache });
  console.log(zip);
  if (!dist) return;
  if (process.env.PAUSE === 'before') hold('electron-before');
  let first = true;
  await extract(zip, { dir: dist, onEntry: () => {
    if (first && process.env.PAUSE === 'during') hold('electron-during');
    first = false;
  } });
  console.log(fs.readFileSync(`${dist}/version`, 'utf8'));
})().catch((e) => { console.error(String(e)); process.exit(1); });
"#;

const ELECTRON: &str = "33.4.5";
const ELECTRON_OTHER: &str = "32.3.3";

fn electron_env(ctx: &Ctx) -> Result<Vec<(String, String)>, String> {
    Ok(vec![
        (
            "NODE_PATH".to_owned(),
            format!("{}/electron-get-modules", bin_dir(ctx)?),
        ),
        ("MIRROR".to_owned(), format!("{REGISTRY}/electron/")),
    ])
}

fn electron_fixture(ctx: &Ctx) -> TestResult {
    write(
        ctx,
        &format!("{CTL}/mkelectron.py"),
        MKELECTRON_PY.as_bytes(),
    )?;
    tool(
        ctx,
        Path::new(HOME),
        &[],
        python(ctx)?,
        &[
            &format!("{HOME}/{CTL}/mkelectron.py"),
            &format!("{HOME}/{REG}/electron"),
            ELECTRON,
            ELECTRON_OTHER,
        ],
    )?;
    write(
        ctx,
        &format!("{SRC}/elapp/install.js"),
        ELECTRON_INSTALL_JS.as_bytes(),
    )
}

/// The install script for `version` (extracting into `dist` when given),
/// with `extra` environment; returns the cached zip's path.
fn electron_install(
    ctx: &Ctx,
    version: &str,
    dist: Option<&str>,
    extra: &[(&str, &str)],
) -> Result<PathBuf, String> {
    let env = electron_env(ctx)?;
    let mut env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    env.extend_from_slice(extra);
    let mut args = vec!["install.js", version];
    args.extend(dist);
    let out = tool(ctx, &home(&format!("{SRC}/elapp")), &env, NODE, &args)?;
    let zip = PathBuf::from(out.lines().next().unwrap_or_default());
    ensure!(
        zip.is_file(),
        "@electron/get returned no cached zip: {out:?}"
    );
    Ok(zip)
}

/// The cache entry (the URL-hash directory) a cached zip is in.
fn electron_entry(zip: &Path) -> Result<PathBuf, String> {
    zip.parent()
        .map(Path::to_owned)
        .ok_or_else(|| format!("{} has no parent", zip.display()))
}

/// docs/testing.md, Admission evidence (`electron-download-cache`): the
/// electron package's install steps with the real @electron/get 5.1.0 and
/// extract-zip 2.0.1 download two releases from a local mirror into
/// `~/.cache/electron/<hash of the URL directory>`. A real cache at
/// `electron_config_cache` and lookalikes outside `.cache/electron` are
/// never found; a hand-made directory directly in `.cache/electron` is
/// deleted with the downloads (the row's residual (4): nothing marks a
/// directory as @electron/get's). Past the one-hour floor both downloads
/// are reclaimed and the next install downloads again. An extraction in
/// flight holds the zip open (`in_use`, `fd`) and then completes; an install
/// stopped between its cache hit and opening the zip holds nothing, so its
/// download is reclaimed under it and the extraction fails (`ENOENT`), the
/// row's residual (1); the retried install downloads again.
pub fn electron_download_cache_real_electron_get_reclaim_then_redownload_and_an_extraction_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    let modules = ctx.bin_dir.join("electron-get-modules");
    for (package, want) in [("@electron/get", "\"5.1.0\""), ("extract-zip", "\"2.0.1\"")] {
        let manifest = std::fs::read_to_string(modules.join(package).join("package.json"))
            .map_err(|e| format!("read {package}'s package.json: {e}"))?;
        ensure!(
            manifest.contains(&format!("\"version\": {want}")),
            "{package} is not the verified {want}"
        );
        info!("tool version: {package} {want}");
    }
    control_dir(ctx)?;
    let registry = Registry::start(ctx, "")?;
    electron_fixture(ctx)?;
    let zip = electron_install(ctx, ELECTRON, Some("/home/tester/src/elapp/dist"), &[])?;
    let entry = electron_entry(&zip)?;
    let other = electron_entry(&electron_install(ctx, ELECTRON_OTHER, None, &[])?)?;
    ensure!(
        entry.parent() == Some(home(".cache/electron").as_path()) && entry != other,
        "unexpected cache entries {} and {}",
        entry.display(),
        other.display()
    );
    let alt_zip = electron_install(
        ctx,
        ELECTRON,
        None,
        &[("electron_config_cache", "/home/tester/alt/electron")],
    )?;
    ensure!(
        alt_zip.starts_with(home("alt/electron")),
        "electron_config_cache was not used: {}",
        alt_zip.display()
    );
    write(ctx, ".cache/electron/handmade/notes.txt", b"by hand\n")?;
    let handmade = home(".cache/electron/handmade");
    let decoys = [
        lookalike(ctx, ".cache/electron-builder/winCodeSign")?,
        lookalike(ctx, &format!("{SRC}/.cache/electron/0123"))?,
    ];

    {
        let _clock = SandboxClockOffset::install(PAST_THE_HOUR.as_secs())?;
        reclaim(
            ctx,
            "electron-download-cache",
            &vec![
                (entry.clone(), None),
                (other.clone(), None),
                (handmade, None),
            ],
        )?;
    }
    expect_kept(&[&alt_zip, &decoys[0], &decoys[1]])?;
    let zip_path = format!("/electron/v{ELECTRON}/electron-v{ELECTRON}-linux-x64.zip");
    let before = registry.served(&zip_path)?;
    electron_install(ctx, ELECTRON, None, &[])?;
    ensure!(
        registry.served(&zip_path)? > before && zip.is_file(),
        "the next install did not download again"
    );

    let env = electron_env(ctx)?;
    let mut env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    env.push(("PAUSE", "during"));
    let mut extraction = InFlight::start(
        ctx,
        "electron-extract",
        &home(&format!("{SRC}/elapp")),
        &env,
        NODE,
        &["install.js", ELECTRON, "/home/tester/src/elapp/dist2"],
    )?;
    wait_file(
        &mut extraction,
        &home(&format!("{CTL}/electron-during.ready")),
    )?;
    let reclaimed = {
        let _clock = SandboxClockOffset::install(PAST_THE_HOUR.as_secs())?;
        reclaim(
            ctx,
            "electron-download-cache",
            &vec![(entry.clone(), Some("in_use"))],
        )?
    };
    expect_in_use_by(&reclaimed, &entry, &extraction, &["fd"], "zip", &|p| {
        p == zip
    })?;
    open_gate(ctx, "electron-during")?;
    let out = extraction.expect_success()?;
    ensure!(out.contains(ELECTRON), "the extraction printed {out:?}");

    let env: Vec<(&str, &str)> = env
        .iter()
        .map(|(k, v)| {
            if *k == "PAUSE" {
                (*k, "before")
            } else {
                (*k, *v)
            }
        })
        .collect();
    let mut install = InFlight::start(
        ctx,
        "electron-install",
        &home(&format!("{SRC}/elapp")),
        &env,
        NODE,
        &["install.js", ELECTRON, "/home/tester/src/elapp/dist3"],
    )?;
    wait_file(&mut install, &home(&format!("{CTL}/electron-before.ready")))?;
    {
        let _clock = SandboxClockOffset::install(PAST_THE_HOUR.as_secs())?;
        reclaim(ctx, "electron-download-cache", &vec![(entry.clone(), None)])?;
    }
    open_gate(ctx, "electron-before")?;
    let (status, output) = install.finish()?;
    ensure!(
        !status.success()
            && output.contains("ENOENT")
            && output.contains(&format!("electron-v{ELECTRON}-linux-x64.zip")),
        "the install between its cache hit and the extraction survived the reclaim ({status}); \
         update the electron-download-cache liveness row:\n{output}"
    );
    electron_install(ctx, ELECTRON, Some("/home/tester/src/elapp/dist3"), &[])?;
    Ok(())
}

// ---- huggingface-hub-repos -------------------------------------------------

/// A Hugging Face hub endpoint for `hf_hub_download`: `HEAD` and `GET` of
/// `[/TYPE]/ORG/NAME/resolve/REVISION/FILE` answer with the commit and ETag
/// headers the library reads (a SHA-256 ETag for the LFS-style
/// `.safetensors`, a SHA-1 for the rest, as the hub does). A `GET` whose
/// path holds the gate text writes `ctl/gate.hit` and waits for
/// `ctl/gate.go`. Logs `200 PATH` per answered `GET`.
const HF_SERVER_PY: &str = r#"
import hashlib, http.server, os, sys, time
port, ctl, gate = int(sys.argv[1]), sys.argv[2], sys.argv[3]
COMMIT = "c0ffee" + "0" * 34
def body(name):
    if name.endswith(".safetensors"):
        return bytes(range(256)) * 1200
    return ('{"name": "%s"}\n' % name).encode()
class Hub(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def answer(self):
        parts = self.path.split("?")[0].strip("/").split("/")
        if "resolve" not in parts:
            self.send_response(404); self.end_headers(); return None
        name = "/".join(parts[parts.index("resolve") + 2:])
        data = body(name)
        tag = (hashlib.sha256 if name.endswith(".safetensors") else hashlib.sha1)(data).hexdigest()
        self.send_response(200)
        self.send_header("X-Repo-Commit", COMMIT)
        self.send_header("ETag", f'"{tag}"')
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        return data
    def do_HEAD(self):
        self.answer()
    def do_GET(self):
        if gate and gate in self.path:
            open(os.path.join(ctl, "gate.hit"), "w").write(self.path)
            while not os.path.exists(os.path.join(ctl, "gate.go")):
                time.sleep(0.05)
        data = self.answer()
        if data is not None:
            self.wfile.write(data)
            with open(os.path.join(ctl, "hub.log"), "a") as log:
                log.write("200 " + self.path + "\n")
server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Hub)
open(os.path.join(ctl, "hub.port"), "w").write(str(port))
server.serve_forever()
"#;

const HF_PORT: &str = "4874";

/// The hub endpoint, killed when dropped.
struct Hub {
    _server: InFlight,
}

impl Hub {
    fn start(ctx: &Ctx, gate: &str) -> Result<Self, String> {
        write(ctx, &format!("{CTL}/hub.py"), HF_SERVER_PY.as_bytes())?;
        let python = hf_python(ctx);
        let mut server = InFlight::start(
            ctx,
            "hub",
            Path::new(HOME),
            &[],
            python.to_str().ok_or("hf python")?,
            &[
                &format!("{HOME}/{CTL}/hub.py"),
                HF_PORT,
                &format!("{HOME}/{CTL}"),
                gate,
            ],
        )?;
        wait_file(&mut server, &home(&format!("{CTL}/hub.port")))?;
        Ok(Self { _server: server })
    }

    fn served(&self, fragment: &str) -> Result<usize, String> {
        let path = home(&format!("{CTL}/hub.log"));
        let log = match std::fs::read_to_string(&path) {
            Ok(log) => log,
            // Nothing served yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        Ok(log.lines().filter(|l| l.contains(fragment)).count())
    }
}

const HF_ENV: &[(&str, &str)] = &[
    ("HF_ENDPOINT", "http://127.0.0.1:4874"),
    ("HF_HUB_DISABLE_TELEMETRY", "1"),
];

/// A `hf_hub_download` script: `REPO TYPE FILE...`; prints each path.
const HF_DOWNLOAD_PY: &str = r#"
import sys
from huggingface_hub import hf_hub_download
repo, kind, files = sys.argv[1], sys.argv[2], sys.argv[3:]
for f in files:
    print(hf_hub_download(repo, f, repo_type=kind))
"#;

/// Maps a cached snapshot file through the library's own lookup
/// (`local_files_only`) and closes its descriptor, as safetensors (Rust's
/// memmap2) loads a model (Python's own `mmap` would keep a duplicate
/// descriptor); reports its pid, waits for the gate, then reads the
/// mapping.
const HF_MAP_PY: &str = r#"
import ctypes, os, time
from huggingface_hub import hf_hub_download
path = hf_hub_download("org/tiny", "model.safetensors", local_files_only=True)
libc = ctypes.CDLL(None, use_errno=True)
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
fd = os.open(path, os.O_RDONLY)
size = os.fstat(fd).st_size
view = libc.mmap(None, size, 1, 1, fd, 0)  # PROT_READ, MAP_SHARED
assert view not in (None, ctypes.c_void_p(-1).value), os.strerror(ctypes.get_errno())
os.close(fd)
open("/home/tester/ctl/hf-map.ready", "w").write(str(os.getpid()))
while not os.path.exists("/home/tester/ctl/gate.go"):
    time.sleep(0.05)
assert ctypes.string_at(view, 2) == bytes([0, 1])
"#;

fn hf_download(
    ctx: &Ctx,
    repo: &str,
    kind: &str,
    files: &[&str],
    extra: &[(&str, &str)],
) -> Result<Vec<PathBuf>, String> {
    let env: Vec<(&str, &str)> = HF_ENV.iter().chain(extra).copied().collect();
    let mut args = vec!["/home/tester/ctl/hf_download.py", repo, kind];
    args.extend_from_slice(files);
    let out = tool(ctx, Path::new(HOME), &env, hf_python(ctx), &args)?;
    Ok(out.lines().map(PathBuf::from).collect())
}

fn hub(rel: &str) -> PathBuf {
    home(&format!(".cache/huggingface/hub/{rel}"))
}

/// docs/testing.md, Admission evidence (`huggingface-hub-repos`): the real
/// huggingface_hub downloads a model's two files and a dataset's file from
/// a local hub endpoint into `~/.cache/huggingface/hub` (`models--*`,
/// `datasets--*`, blobs, snapshot links, `refs`, and its `.locks`). A real
/// cache at `HF_HUB_CACHE` and lookalikes outside the anchor are never
/// found. Past the one-hour floor both repositories are reclaimed; their
/// `.locks` directories stay. With `HF_HUB_OFFLINE` the next download fails
/// (the row's residual (3)), online it downloads again. Two runs in flight
/// keep their repositories `in_use`: a Python that mapped a snapshot file
/// through the library's lookup and closed its descriptor (`map`, as
/// safetensors loads), and a
/// download held at the endpoint with its `.incomplete` blob open (`fd`);
/// both then complete.
pub fn huggingface_hub_repos_real_hub_downloads_reclaim_then_redownload_and_runs_in_flight_are_kept(
    ctx: &Ctx,
) -> TestResult {
    let python = hf_python(ctx);
    versions(
        ctx,
        &[(
            python.to_str().ok_or("hf python")?,
            &[
                "-c",
                "import huggingface_hub; print(huggingface_hub.__version__)",
            ],
            "1.30.0",
        )],
    )?;
    control_dir(ctx)?;
    write(
        ctx,
        &format!("{CTL}/hf_download.py"),
        HF_DOWNLOAD_PY.as_bytes(),
    )?;
    write(ctx, &format!("{CTL}/hf_map.py"), HF_MAP_PY.as_bytes())?;
    let endpoint = Hub::start(ctx, "org/live/")?;
    hf_download(
        ctx,
        "org/tiny",
        "model",
        &["config.json", "model.safetensors"],
        &[],
    )?;
    hf_download(ctx, "org/set", "dataset", &["data.json"], &[])?;
    hf_download(
        ctx,
        "org/tiny",
        "model",
        &["config.json"],
        &[("HF_HUB_CACHE", "/home/tester/alt/hub")],
    )?;
    let tiny = hub("models--org--tiny");
    let set = hub("datasets--org--set");
    for repo in [&tiny, &set] {
        ensure!(
            repo.join("blobs").is_dir() && repo.join("refs/main").is_file(),
            "huggingface_hub made no repository at {}",
            repo.display()
        );
    }
    let alt = home("alt/hub/models--org--tiny/refs/main");
    ensure!(alt.is_file(), "HF_HUB_CACHE was not used");
    let decoys = [
        lookalike(ctx, &format!("{SRC}/.cache/huggingface/hub/models--org--m"))?,
        lookalike(ctx, ".cache/huggingface/datasets/org___set")?,
    ];

    {
        let _clock = SandboxClockOffset::install(PAST_THE_HOUR.as_secs())?;
        reclaim(
            ctx,
            "huggingface-hub-repos",
            &vec![(tiny.clone(), None), (set.clone(), None)],
        )?;
    }
    expect_kept(&[
        &alt,
        &decoys[0],
        &decoys[1],
        &hub(".locks/models--org--tiny"),
    ])?;
    let offline = hf_download(
        ctx,
        "org/tiny",
        "model",
        &["model.safetensors"],
        &[("HF_HUB_OFFLINE", "1")],
    );
    info!("an offline download after the reclaim: {offline:?}");
    ensure!(
        offline.is_err_and(|e| e.contains("OfflineModeIsEnabled")),
        "an offline download found the reclaimed repository"
    );
    let before = endpoint.served("model.safetensors")?;
    hf_download(
        ctx,
        "org/tiny",
        "model",
        &["config.json", "model.safetensors"],
        &[],
    )?;
    ensure!(
        endpoint.served("model.safetensors")? > before,
        "huggingface_hub did not download again"
    );

    let env: Vec<(&str, &str)> = HF_ENV
        .iter()
        .copied()
        .chain([("HF_HUB_OFFLINE", "1")])
        .collect();
    let python = python.to_str().ok_or("hf python")?;
    let mut mapper = InFlight::start(
        ctx,
        "hf-map",
        Path::new(HOME),
        &env,
        python,
        &["/home/tester/ctl/hf_map.py"],
    )?;
    wait_file(&mut mapper, &home(&format!("{CTL}/hf-map.ready")))?;
    let mut download = InFlight::start(
        ctx,
        "hf-download",
        Path::new(HOME),
        HF_ENV,
        python,
        &[
            "/home/tester/ctl/hf_download.py",
            "org/live",
            "model",
            "model.safetensors",
        ],
    )?;
    wait_file(&mut download, &home(&format!("{CTL}/gate.hit")))?;
    let live = hub("models--org--live");
    wait_for_held(&mut download, &live, ".incomplete")?;
    let reclaimed = {
        let _clock = SandboxClockOffset::install(PAST_THE_HOUR.as_secs())?;
        reclaim(
            ctx,
            "huggingface-hub-repos",
            &vec![
                (tiny.clone(), Some("in_use")),
                (live.clone(), Some("in_use")),
            ],
        )?
    };
    expect_in_use_by(&reclaimed, &tiny, &mapper, &["map"], "mapped blob", &|p| {
        p.starts_with(tiny.join("blobs"))
    })?;
    expect_in_use_by(
        &reclaimed,
        &live,
        &download,
        &["fd"],
        "incomplete blob",
        &|p| p.to_string_lossy().ends_with(".incomplete"),
    )?;
    release(ctx)?;
    mapper.expect_success()?;
    download.expect_success()?;
    ensure!(
        live.join("refs/main").is_file(),
        "the download in flight did not complete its repository"
    );
    Ok(())
}

/// Waits until `run` holds a file below `dir` whose path ends with `suffix`.
fn wait_for_held(run: &mut InFlight, dir: &Path, suffix: &str) -> TestResult {
    crate::owner_pid_suite::wait_for(
        crate::admission::TOOL_TIMEOUT,
        &format!("a held {suffix} below {}", dir.display()),
        || {
            run.still_running()?;
            Ok(held_by(run, dir)?
                .iter()
                .any(|p| p.to_string_lossy().ends_with(suffix))
                .then_some(()))
        },
    )
}

// ---- cargo-registry-src ----------------------------------------------------

/// A sparse Cargo registry: `index/config.json` (its `dl` template) and one
/// index line per `NAME:VERSION:DEPS:CRATE` argument (the real `.crate`
/// that `cargo package` made, copied to `dl/` with its SHA-256).
const MKINDEX_PY: &str = r#"
import hashlib, json, os, shutil, sys
reg, url, specs = sys.argv[1], sys.argv[2], sys.argv[3:]
os.makedirs(f"{reg}/index", exist_ok=True)
os.makedirs(f"{reg}/dl", exist_ok=True)
json.dump({"dl": url + "/dl/{crate}-{version}.crate"}, open(f"{reg}/index/config.json", "w"))
def index_path(n):
    return {1: f"1/{n}", 2: f"2/{n}", 3: f"3/{n[0]}/{n}"}.get(len(n), f"{n[:2]}/{n[2:4]}/{n}")
for spec in specs:
    name, vers, deps, crate = spec.split(":")
    data = open(crate, "rb").read()
    shutil.copy(crate, f"{reg}/dl/{name}-{vers}.crate")
    deps = [{"name": d, "req": "^1.0", "features": [], "optional": False, "default_features": True,
             "target": None, "kind": "normal"} for d in deps.split(",") if d]
    line = {"name": name, "vers": vers, "deps": deps, "cksum": hashlib.sha256(data).hexdigest(),
            "features": {}, "yanked": False}
    path = f"{reg}/index/{index_path(name)}"
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "a") as f:
        f.write(json.dumps(line) + "\n")
"#;

/// Cargo's configuration for the local registry (served at
/// `REGISTRY/cargo`), in place of crates.io.
fn cargo_config() -> String {
    format!(
        "[source.crates-io]\nreplace-with = \"local\"\n[source.local]\nregistry = \"sparse+{REGISTRY}/cargo/index/\"\n[net]\nretry = 0\n"
    )
}

/// The registry's crates: `top` depends on `mid`, `mid` on `leaf`.
const CRATES: &[(&str, &str, &str)] = &[
    ("leaf", "", "pub fn leaf() -> u32 { 40 }\n"),
    ("mid", "leaf", "pub fn mid() -> u32 { leaf::leaf() + 1 }\n"),
    ("top", "mid", "pub fn top() -> u32 { mid::mid() + 1 }\n"),
];

/// The environment of a Cargo under test with `cargo_home`, online (to the
/// local registry) unless `offline`.
fn cargo_env(toolchain: &Toolchain, cargo_home: &str, offline: bool) -> Vec<(String, String)> {
    let mut env = toolchain.env();
    env.retain(|(k, _)| k != "CARGO_NET_OFFLINE");
    env.push(("CARGO_HOME".to_owned(), cargo_home.to_owned()));
    env.push(("CARGO_NET_OFFLINE".to_owned(), offline.to_string()));
    env
}

fn cargo(
    ctx: &Ctx,
    toolchain: &Toolchain,
    cwd: &Path,
    env: &[(String, String)],
    args: &[&str],
) -> Result<String, String> {
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    tool(ctx, cwd, &env, toolchain.cargo(), args)
}

/// The three crates packaged by the real `cargo package` (with a Cargo
/// home of their own) into the registry, and the project `src/rsapp`
/// depending on `top`, with Cargo's configuration in the measured
/// `~/.cargo` and the alternative Cargo home.
fn cargo_fixture(ctx: &Ctx, toolchain: &Toolchain) -> TestResult {
    write(ctx, &format!("{CTL}/mkindex.py"), MKINDEX_PY.as_bytes())?;
    for cargo_home in [".cargo", "pack-cargo", "alt/cargo"] {
        write(
            ctx,
            &format!("{cargo_home}/config.toml"),
            cargo_config().as_bytes(),
        )?;
    }
    let pack = cargo_env(toolchain, "/home/tester/pack-cargo", false);
    for (name, dep, body) in CRATES {
        let dir = home(&format!("crates/{name}"));
        let dependency = if dep.is_empty() {
            String::new()
        } else {
            format!("{dep} = \"1.0\"\n")
        };
        write(
            ctx,
            &format!("crates/{name}/Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"1.0.0\"\nedition = \"2021\"\ndescription = \"x\"\nlicense = \"MIT\"\n\n[dependencies]\n{dependency}"
            )
            .as_bytes(),
        )?;
        write(ctx, &format!("crates/{name}/src/lib.rs"), body.as_bytes())?;
        cargo(
            ctx,
            toolchain,
            &dir,
            &pack,
            &["package", "--no-verify", "--allow-dirty"],
        )?;
        let crate_file = dir.join(format!("target/package/{name}-1.0.0.crate"));
        tool(
            ctx,
            Path::new(HOME),
            &[],
            python(ctx)?,
            &[
                &format!("{HOME}/{CTL}/mkindex.py"),
                &format!("{HOME}/{REG}/cargo"),
                &format!("{REGISTRY}/cargo"),
                &format!("{name}:1.0.0:{dep}:{}", crate_file.display()),
            ],
        )?;
    }
    write(
        ctx,
        &format!("{SRC}/rsapp/Cargo.toml"),
        b"[package]\nname = \"rsapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ntop = \"1.0\"\n",
    )?;
    write(
        ctx,
        &format!("{SRC}/rsapp/src/main.rs"),
        b"fn main() { println!(\"{}\", top::top()); }\n",
    )?;
    // Holds the compile of `mid` (its working directory is mid's source,
    // as Cargo runs rustc for a registry package) until the gate opens.
    write_script(
        ctx,
        "gated-cargo/rustc",
        b"#!/bin/sh\ncase \"$*\" in *\"--crate-name mid \"*) : > /home/tester/ctl/cargo.hit\n  while [ ! -e /home/tester/ctl/gate.go ]; do sleep 0.1; done;; esac\nexec \"$@\"\n",
    )
}

/// `cargo build` of the project, then runs it (prints 42) from its target
/// directory (`CARGO_TARGET_DIR` in `env`, or the project's `target`).
fn cargo_run(ctx: &Ctx, toolchain: &Toolchain, env: &[(String, String)]) -> TestResult {
    let project = home(&format!("{SRC}/rsapp"));
    cargo(ctx, toolchain, &project, env, &["build"])?;
    let target = env
        .iter()
        .find(|(k, _)| k == "CARGO_TARGET_DIR")
        .map_or_else(|| project.join("target"), |(_, v)| PathBuf::from(v));
    let out = tool(ctx, &project, &[], target.join("debug/rsapp"), &[])?;
    ensure!(out.trim() == "42", "the Cargo program printed {out:?}");
    Ok(())
}

/// The crate sources of the registry directory in `~/.cargo/registry/src`.
fn crate_sources() -> Result<(PathBuf, Vec<PathBuf>), String> {
    let registry = only_entry(&home(".cargo/registry/src"))?;
    let sources = CRATES
        .iter()
        .map(|(name, _, _)| registry.join(format!("{name}-1.0.0")))
        .collect();
    Ok((registry, sources))
}

/// docs/testing.md, Admission evidence (`cargo-registry-src`): Cargo
/// 1.99.1 builds a project against a local sparse registry of three crates
/// that the real `cargo package` made, extracting them into
/// `~/.cargo/registry/src/<registry>/` with their `.cargo-ok`. A real
/// extraction under another `CARGO_HOME`, a hand-made source without
/// `.cargo-ok` (never planned: the profile's `has`) and a lookalike
/// outside the anchor are kept.
/// Past the one-hour floor the three sources are reclaimed, and the next
/// `cargo build --offline` extracts them again from the `.crate` files in
/// `registry/cache` without a download (the row's residual (2), its offline
/// side). TASK-329's case: a build in flight, its compile of `mid` held, has
/// its working directory in mid's source (`in_use`, `cwd`) and holds Cargo's
/// package-cache lock shared, which diskgc does not take; `top`, compiled
/// after `mid`, is reclaimed under the build, whose compile of `top` then
/// fails: the row's residual (1), one failed build. The retried offline
/// build extracts it again and succeeds.
pub fn cargo_registry_src_real_cargo_extracts_reclaim_then_reextract_and_a_build_in_flight_fails_once(
    ctx: &Ctx,
) -> TestResult {
    let toolchain = Toolchain::fast()?;
    let cargo_path = toolchain.cargo();
    versions(
        ctx,
        &[(
            cargo_path.to_str().ok_or("cargo")?,
            &["--version"],
            "cargo 1.99.1 ",
        )],
    )?;
    control_dir(ctx)?;
    let registry = Registry::start(ctx, "")?;
    cargo_fixture(ctx, &toolchain)?;
    let online = cargo_env(&toolchain, "/home/tester/.cargo", false);
    let offline = cargo_env(&toolchain, "/home/tester/.cargo", true);
    cargo_run(ctx, &toolchain, &online)?;
    let (registry_dir, sources) = crate_sources()?;
    for source in &sources {
        ensure!(
            source.join(".cargo-ok").is_file(),
            "Cargo extracted no {}",
            source.display()
        );
    }
    let mut alt = cargo_env(&toolchain, "/home/tester/alt/cargo", false);
    alt.push((
        "CARGO_TARGET_DIR".to_owned(),
        "/home/tester/alt/target".to_owned(),
    ));
    cargo_run(ctx, &toolchain, &alt)?;
    let alt_files = count_files(&home("alt/cargo/registry/src"), &|n| n == ".cargo-ok")?;
    ensure!(
        alt_files == CRATES.len(),
        "the other CARGO_HOME holds {alt_files} sources"
    );
    write(
        ctx,
        &format!(
            "{}/fake-1.0.0/src/lib.rs",
            registry_dir
                .strip_prefix(HOME)
                .map_err(|e| e.to_string())?
                .display()
        ),
        b"// by hand\n",
    )?;
    let fake = registry_dir.join("fake-1.0.0");
    let decoy = lookalike(ctx, &format!("{SRC}/.cargo/registry/src/x/y-1.0.0"))?;

    let expected: Expected = sources.iter().map(|s| (s.clone(), None)).collect();
    {
        let _clock = SandboxClockOffset::install(PAST_THE_HOUR.as_secs())?;
        reclaim(ctx, "cargo-registry-src", &expected)?;
    }
    expect_kept(&[&decoy, &fake])?;
    ensure!(
        count_files(&home("alt/cargo/registry/src"), &|n| n == ".cargo-ok")? == alt_files
            && count_files(&home(".cargo/registry/cache"), &|n| n.ends_with(".crate"))?
                == CRATES.len(),
        "the other Cargo home or the measured .crate files changed"
    );
    let downloads = registry.served("/cargo/dl/top-1.0.0.crate")?;
    cargo_run(ctx, &toolchain, &offline)?;
    ensure!(
        sources.iter().all(|s| s.join(".cargo-ok").is_file())
            && registry.served("/cargo/dl/top-1.0.0.crate")? == downloads,
        "the offline build did not extract the sources again from registry/cache"
    );

    let project = home(&format!("{SRC}/rsapp"));
    cargo(ctx, &toolchain, &project, &offline, &["clean"])?;
    let mut env = offline.clone();
    env.push((
        "RUSTC_WRAPPER".to_owned(),
        "/home/tester/gated-cargo/rustc".to_owned(),
    ));
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut build = InFlight::start(
        ctx,
        "cargo-build",
        &project,
        &env,
        cargo_path.to_str().ok_or("cargo")?,
        &["build"],
    )?;
    wait_file(&mut build, &home(&format!("{CTL}/cargo.hit")))?;
    let mutate = home(".cargo/.package-cache-mutate");
    let watched = Watched::files(std::slice::from_ref(&mutate))?;
    watched.wait_for(
        build.pid(),
        crate::admission::TOOL_TIMEOUT,
        "holding Cargo's package-cache lock shared",
        |u| {
            u.holds
                .iter()
                .any(|(p, mode)| *p == mutate && mode == "READ")
        },
    )?;
    info!("the build in flight holds {} shared", mutate.display());
    let [leaf, mid, top] = [&sources[0], &sources[1], &sources[2]];
    let reclaimed = {
        let _clock = SandboxClockOffset::install(PAST_THE_HOUR.as_secs())?;
        reclaim(
            ctx,
            "cargo-registry-src",
            &vec![
                (leaf.clone(), None),
                (mid.clone(), Some("in_use")),
                (top.clone(), None),
            ],
        )?
    };
    let mid_source = mid.display().to_string();
    expect_used_by(&reclaimed, mid, &build, &["cwd"], &|u| u == mid_source)?;
    release(ctx)?;
    let (status, output) = build.finish()?;
    ensure!(
        !status.success() && output.contains("top") && output.contains("No such file or directory"),
        "the build in flight survived the reclaim of a source it had yet to compile ({status}); \
         update the cargo-registry-src liveness row (TASK-329):\n{output}"
    );
    cargo_run(ctx, &toolchain, &offline)?;
    Ok(())
}

// ---- nix-user-cache --------------------------------------------------------

/// A tarball `nix/src.tar.gz` of `src/default.nix` (NIX's `fetchTarball`
/// unpacks it into its tarball cache).
const MKTARBALL_PY: &str = r#"
import io, sys, tarfile
out, text = sys.argv[1], sys.argv[2].encode()
with tarfile.open(out, "w:gz") as t:
    info = tarfile.TarInfo("src/default.nix")
    info.size = len(text)
    t.addfile(info, io.BytesIO(text))
"#;

/// A pure flake whose evaluation Nix caches in `eval-cache-v*`.
const NIX_FLAKE: &str = "{ outputs = { self }: { value = \"v\" + toString (builtins.foldl' (a: _: a + 1) 0 (builtins.genList (x: x) 1000)); }; }\n";

/// Nix with flakes, a store of its own below the home (no daemon, no
/// builds) and Git (for `fetchGit`) on `PATH`; `tarball-ttl = 0` so every
/// fetch asks the server again instead of trusting its fetcher cache.
fn nix_env(ctx: &Ctx) -> Result<Vec<(String, String)>, String> {
    Ok(vec![
        ("PATH".to_owned(), format!("/bin:{}/git-bin", bin_dir(ctx)?)),
        (
            "NIX_CONFIG".to_owned(),
            "experimental-features = nix-command flakes\ntarball-ttl = 0".to_owned(),
        ),
    ])
}

const NIX_STORE: &str = "/home/tester/nix-root";

fn nix(ctx: &Ctx, extra: &[(&str, &str)], args: &[&str]) -> Result<String, String> {
    let env = nix_env(ctx)?;
    let mut env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    env.extend_from_slice(extra);
    let mut argv = vec!["--store", NIX_STORE];
    argv.extend_from_slice(args);
    tool(ctx, Path::new(HOME), &env, NIX, &argv)
}

fn fetch_git_expr() -> String {
    format!("(builtins.fetchGit {{ url = \"{REGISTRY}/nix/repo.git\"; }}).outPath")
}

fn fetch_tarball_expr() -> String {
    format!("builtins.fetchTarball \"{REGISTRY}/nix/src.tar.gz\"")
}

/// The three caches: Git (`fetchGit` over dumb HTTP), tarball
/// (`fetchTarball`) and evaluation (a flake's attribute), with `extra`
/// environment.
fn nix_fill(ctx: &Ctx, extra: &[(&str, &str)]) -> TestResult {
    nix(
        ctx,
        extra,
        &["eval", "--impure", "--raw", "--expr", &fetch_git_expr()],
    )?;
    nix(
        ctx,
        extra,
        &["eval", "--impure", "--raw", "--expr", &fetch_tarball_expr()],
    )?;
    let out = nix(
        ctx,
        extra,
        &["eval", "--raw", "path:/home/tester/nixflake#value"],
    )?;
    ensure!(out == "v1000", "the flake evaluated to {out:?}");
    Ok(())
}

/// Git as tester in `cwd`, with a fixed identity for the fixture commits.
fn git(ctx: &Ctx, cwd: &Path, args: &[&str]) -> Result<String, String> {
    let git = ctx.bin_dir.join("git-bin/git");
    let mut argv = vec![
        "-c",
        "user.name=tester",
        "-c",
        "user.email=tester@example.invalid",
    ];
    argv.extend_from_slice(args);
    tool(ctx, cwd, &[], git, &argv)
}

/// Commits `name` to the fixture repository and republishes it as the
/// served bare repository.
fn nix_commit(ctx: &Ctx, name: &str) -> TestResult {
    let repo = home("nixrepo");
    write(ctx, &format!("nixrepo/{name}"), name.as_bytes())?;
    git(ctx, &repo, &["add", name])?;
    git(ctx, &repo, &["commit", "-q", "-m", name])?;
    let bare = home(&format!("{REG}/nix/repo.git"));
    if exists(&bare) {
        git(
            ctx,
            &bare,
            &["fetch", "-q", "/home/tester/nixrepo", "+master:master"],
        )?;
    } else {
        git(
            ctx,
            Path::new(HOME),
            &[
                "clone",
                "-q",
                "--bare",
                "nixrepo",
                &format!("{REG}/nix/repo.git"),
            ],
        )?;
    }
    git(ctx, &bare, &["update-server-info"])?;
    Ok(())
}

fn nix_fixture(ctx: &Ctx) -> TestResult {
    ctx.home.mkdirs("nixrepo", 0o755, crate::TESTER)?;
    git(ctx, &home("nixrepo"), &["init", "-q", "-b", "master"])?;
    nix_commit(ctx, "one")?;
    write(ctx, &format!("{CTL}/mktarball.py"), MKTARBALL_PY.as_bytes())?;
    tool(
        ctx,
        Path::new(HOME),
        &[],
        python(ctx)?,
        &[
            &format!("{HOME}/{CTL}/mktarball.py"),
            &format!("{HOME}/{REG}/nix/src.tar.gz"),
            "{ x = 1; }\n",
        ],
    )?;
    write(ctx, "nixflake/flake.nix", NIX_FLAKE.as_bytes())
}

/// The cache entries of a Nix user cache directory that the profile's
/// `find` names: `gitv3`, `tarball-cache-v2` and `eval-cache-v*`.
fn nix_caches(cache: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(cache).map_err(|e| format!("list {}: {e}", cache.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if path.is_dir()
            && (name == "gitv3" || name == "tarball-cache-v2" || name.starts_with("eval-cache-v"))
        {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

/// docs/testing.md, Admission evidence (`nix-user-cache`): the real Nix
/// 2.34.9 (a store of its own below the home, no daemon) fills its Git
/// cache (`fetchGit` of a dumb-HTTP repository), its tarball cache
/// (`fetchTarball`) and its evaluation cache (a flake attribute) in
/// `~/.cache/nix`. A real cache at another `XDG_CACHE_HOME`, Nix's
/// `fetcher-cache-v4.sqlite` and hand-made `gitv2` and `eval-cache` are
/// never found. All three are reclaimed (no floor) and Nix fills them
/// again. A `fetchGit` in flight, held at the server while Git fetches into
/// the cache, holds the cache's lock file and Git's working directory
/// there, so diskgc keeps `gitv3` `in_use` and reclaims the other two; the
/// fetch then completes with the new commit.
pub fn nix_user_cache_real_nix_fetches_reclaim_then_refetch_and_a_fetch_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    nix_user_scenario(ctx, &ctx.diskgc())
}

/// TASK-370: [`nix_user_cache_real_nix_fetches_reclaim_then_refetch_and_a_fetch_in_flight_is_kept`] through the gate-lifted release-candidate
/// package, whose 0.2 catalog holds `nix-user-cache`.
pub fn release_candidate_nix_user_cache_reclaim_then_refetch_and_a_fetch_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "nix-user-cache")?;
    nix_user_scenario(ctx, &binary)
}

fn nix_user_scenario(ctx: &Ctx, binary: &Path) -> TestResult {
    versions(ctx, &[(NIX, &["--version"], "2.34.9")])?;
    control_dir(ctx)?;
    let _registry = Registry::start(ctx, "nix/live.git/objects/")?;
    nix_fixture(ctx)?;
    nix_fill(ctx, &[])?;
    let cache = home(".cache/nix");
    let caches = nix_caches(&cache)?;
    let names: Vec<String> = caches
        .iter()
        .map(|c| {
            c.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    ensure!(
        names.len() == 3
            && names.contains(&"gitv3".to_owned())
            && names.contains(&"tarball-cache-v2".to_owned()),
        "Nix made the caches {names:?}"
    );
    nix_fill(ctx, &[("XDG_CACHE_HOME", "/home/tester/alt/cache")])?;
    let alt = home("alt/cache/nix");
    let alt_caches = nix_caches(&alt)?;
    ensure!(
        alt_caches.len() == 3,
        "the other XDG_CACHE_HOME holds {alt_caches:?}"
    );
    let fetcher = cache.join("fetcher-cache-v4.sqlite");
    ensure!(fetcher.is_file(), "Nix made no {}", fetcher.display());
    let decoys = [
        lookalike(ctx, ".cache/nix/gitv2")?,
        lookalike(ctx, ".cache/nix/eval-cache")?,
    ];

    let expected: Expected = caches.iter().map(|c| (c.clone(), None)).collect();
    reclaim_with(ctx, binary, "nix-user-cache", &expected)?;
    ensure!(
        nix_caches(&alt)? == alt_caches,
        "the other XDG_CACHE_HOME's caches changed"
    );
    expect_kept(&[&fetcher, &decoys[0], &decoys[1]])?;
    nix_fill(ctx, &[])?;
    ensure!(
        nix_caches(&cache)? == caches,
        "Nix did not fill its caches again"
    );

    // The fetch in flight reads its own repository, the only one whose
    // objects the registry holds at its gate.
    nix_commit(ctx, "two")?;
    git(
        ctx,
        Path::new(HOME),
        &[
            "clone",
            "-q",
            "--bare",
            "nixrepo",
            &format!("{REG}/nix/live.git"),
        ],
    )?;
    git(
        ctx,
        &home(&format!("{REG}/nix/live.git")),
        &["update-server-info"],
    )?;
    let env = nix_env(ctx)?;
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let expr = format!("(builtins.fetchGit {{ url = \"{REGISTRY}/nix/live.git\"; }}).outPath");
    let mut fetch = InFlight::start(
        ctx,
        "nix-fetch",
        Path::new(HOME),
        &env,
        NIX,
        &[
            "--store", NIX_STORE, "eval", "--impure", "--raw", "--expr", &expr,
        ],
    )?;
    wait_file(&mut fetch, &home(&format!("{CTL}/gate.hit")))?;
    let gitv3 = cache.join("gitv3");
    let reclaimed = reclaim_with(
        ctx,
        binary,
        "nix-user-cache",
        &caches
            .iter()
            .map(|c| (c.clone(), (*c == gitv3).then_some("in_use")))
            .collect::<Expected>(),
    )?;
    expect_used_by(&reclaimed, &gitv3, &fetch, &["fd", "cwd", "map"], &|u| {
        u.ends_with(".lock")
    })?;
    release(ctx)?;
    let out = fetch.expect_success()?;
    ensure!(
        out.contains("-source"),
        "the fetch in flight printed {out:?}"
    );
    Ok(())
}

// ---- the candidate catalog -------------------------------------------------

/// docs/testing.md, Admission evidence (candidate catalog): the release
/// build's catalog code path with this family in its catalog (the members
/// not yet admitted through the test-only candidate list). Over one
/// fixture of every member made by the real tools, with a Python mapping a
/// hub snapshot (the live counterexample), its dry run plans exactly what
/// the development build's does, and its reclaim deletes exactly the
/// reclaimable set (past the one-hour floor, through the sandbox clock
/// offset that the gated candidate build reads); the tools then fill their
/// caches again.
pub fn candidate_catalog_runtime_cache_family_matches_the_development_build(
    ctx: &Ctx,
) -> TestResult {
    let candidate = check_candidate_catalog(ctx, FAMILY)?;
    let toolchain = Toolchain::fast()?;
    let (bun_path, gyp, cargo_path, hf) =
        (bun(ctx), node_gyp(ctx), toolchain.cargo(), hf_python(ctx));
    versions(
        ctx,
        &[
            (GO, &["version"], "go1.26.8 "),
            (bun_path.to_str().ok_or("bun")?, &["--version"], "1.4.2"),
            (
                NODE,
                &[gyp.to_str().ok_or("node-gyp")?, "--version"],
                "v12.4.0",
            ),
            (
                hf.to_str().ok_or("hf python")?,
                &[
                    "-c",
                    "import huggingface_hub; print(huggingface_hub.__version__)",
                ],
                "1.30.0",
            ),
            (
                cargo_path.to_str().ok_or("cargo")?,
                &["--version"],
                "cargo 1.99.1 ",
            ),
            (NIX, &["--version"], "2.34.9"),
        ],
    )?;
    control_dir(ctx)?;
    let _registry = Registry::start(ctx, "")?;
    write(
        ctx,
        &format!("{CTL}/hf_download.py"),
        HF_DOWNLOAD_PY.as_bytes(),
    )?;
    write(ctx, &format!("{CTL}/hf_map.py"), HF_MAP_PY.as_bytes())?;
    let _hub = Hub::start(ctx, "")?;

    go_fixture(ctx)?;
    go_build(ctx, &[])?;
    npm_packages(ctx, &["alpha"])?;
    let bun_app = bun_project(ctx, "bunapp", &["alpha"])?;
    bun_install(ctx, &bun_app, &[])?;
    gyp_fixture(ctx)?;
    node_gyp_run(
        ctx,
        Path::new(HOME),
        &["install", &format!("--target={GYP_TARGET}"), &gyp_dist()],
        &[],
    )?;
    electron_fixture(ctx)?;
    let electron = electron_entry(&electron_install(ctx, ELECTRON, None, &[])?)?;
    hf_download(ctx, "org/tiny", "model", &["model.safetensors"], &[])?;
    hf_download(ctx, "org/set", "dataset", &["data.json"], &[])?;
    cargo_fixture(ctx, &toolchain)?;
    cargo_run(
        ctx,
        &toolchain,
        &cargo_env(&toolchain, "/home/tester/.cargo", false),
    )?;
    let (_, sources) = crate_sources()?;
    nix_fixture(ctx)?;
    nix_fill(ctx, &[])?;

    let tiny = hub("models--org--tiny");
    let go_cache: Expected = vec![(home("go/pkg/mod"), None)];
    let mut expected: Expected = vec![
        (home(".bun/install/cache"), None),
        (gyp_headers(GYP_TARGET), None),
        (electron, None),
        (tiny.clone(), Some("in_use")),
        (hub("datasets--org--set"), None),
    ];
    expected.extend(sources.iter().map(|s| (s.clone(), None)));
    expected.extend(
        nix_caches(&home(".cache/nix"))?
            .into_iter()
            .map(|c| (c, None)),
    );

    let env: Vec<(&str, &str)> = HF_ENV
        .iter()
        .copied()
        .chain([("HF_HUB_OFFLINE", "1")])
        .collect();
    let python = hf_python(ctx);
    let mut mapper = InFlight::start(
        ctx,
        "hf-map",
        Path::new(HOME),
        &env,
        python.to_str().ok_or("hf python")?,
        &["/home/tester/ctl/hf_map.py"],
    )?;
    wait_file(&mut mapper, &home(&format!("{CTL}/hf-map.ready")))?;

    // `go-mod-cache` deletes with `make_writable`, the others plainly: one
    // selection's sources share one action, so it has a selection of its own.
    let _clock = SandboxClockOffset::install(PAST_THE_HOUR.as_secs())?;
    let others = FAMILY
        .iter()
        .filter(|name| **name != "go-mod-cache")
        .map(|name| format!("profile:{name}"))
        .collect::<Vec<_>>()
        .join(" || ");
    for (select, expected) in [
        (others.as_str(), &expected),
        ("profile:go-mod-cache", &go_cache),
    ] {
        let oracle = Oracle::arm(Fixture::new())?;
        let development = run_ensure(ctx, &ctx.diskgc(), true, select, Some(HOME), HOME)?;
        let candidate_plan = run_ensure(ctx, &candidate, true, select, Some(HOME), HOME)?;
        oracle
            .judge_runs(&[&development, &candidate_plan])?
            .assert_clean()?;
        development.expect_code(0)?;
        candidate_plan.expect_code(0)?;
        let development = plan_of(&report(&development)?)?;
        ensure!(
            development == sorted(expected),
            "{select}: the development build planned {development:?}, expected {:?}",
            sorted(expected)
        );
        ensure!(
            plan_of(&report(&candidate_plan)?)? == development,
            "{select}: the candidate catalog planned {:?}, the development build {development:?}",
            plan_of(&report(&candidate_plan)?)?
        );
        let reclaimed = dry_run_then_reclaim(ctx, &candidate, select, Some(HOME), HOME, expected)?;
        if select == others {
            expect_in_use_by(&reclaimed, &tiny, &mapper, &["map"], "mapped blob", &|p| {
                p.starts_with(tiny.join("blobs"))
            })?;
        }
    }
    drop(_clock);
    release(ctx)?;
    mapper.expect_success()?;

    go_build(ctx, &[])?;
    bun_install(ctx, &bun_project(ctx, "bunapp2", &["alpha"])?, &[])?;
    gyp_rebuild(ctx)?;
    electron_install(ctx, ELECTRON, None, &[])?;
    hf_download(ctx, "org/set", "dataset", &["data.json"], &[])?;
    cargo_run(
        ctx,
        &toolchain,
        &cargo_env(&toolchain, "/home/tester/.cargo", true),
    )?;
    nix_fill(ctx, &[])?;
    for rebuilt in [
        home("go/pkg/mod"),
        home(".bun/install/cache"),
        gyp_headers(GYP_TARGET).join("installVersion"),
        hub("datasets--org--set/refs/main"),
        sources[2].join(".cargo-ok"),
        home(".cache/nix/gitv3"),
    ] {
        ensure!(
            exists(&rebuilt),
            "the tool did not rebuild {}",
            rebuilt.display()
        );
    }
    Ok(())
}
