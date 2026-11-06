//! TASK-350: admission evidence for the project-tree built-ins
//! (`node-modules`, `python-venv`, `python-pycache`, `mypy-cache`,
//! `pytest-cache`, `node-compile-cache`), docs/testing.md, Admission
//! evidence.
//!
//! Every artifact here is made by the real pinned tool in the container
//! (npm, CPython's `venv`, uv, CPython imports, mypy, pytest, Node), never
//! by hand; hand-made trees appear only as the lookalike decoys a profile
//! must not take. Per profile: a dry run plans exactly the expected set, a
//! reclaim under the deletion oracle deletes exactly its reclaimable part,
//! and the tool then makes the artifact again (rebuildable). The live
//! counterexamples run the tool in or against the artifact during the
//! reclaim. The containers have no network, so npm installs a local
//! package that the real npm packed.
//!
//! `node-modules` and `python-venv` keep a project for 30 days after its
//! last change, read or visible use; every project here is made now, so
//! those scenarios advance the sandboxed binary's clock past the floor
//! ([`PAST_THE_FLOOR`]) and make "recent" activity by setting a file's
//! mtime or atime to an hour before that clock's now (a kernel ctime
//! cannot be set back).
//!
//! TASK-359 admitted five of the six to the 0.2 catalog: the
//! `release_candidate_*` tests run the `pytest-cache`, `python-pycache`,
//! `mypy-cache` and `node-compile-cache` scenarios through the gate-lifted
//! release-candidate package, and `python-venv` only on the keep side of
//! its floor (that build reads no clock offset). `node-modules` is held
//! out (Mark, 2026-11-06, TASK-365; successor TASK-357): it runs through
//! the release build's catalog path only as a candidate
//! ([`candidate_catalog_project_tree_family_matches_the_development_build`]).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Stdio};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::admission::{
    CTL, Expected, HOME, NODE, NPM, UV, check_candidate_catalog, control_dir, dry_run_then_reclaim,
    exists, expect_blocker, held_in, home, plan_of, python, run_ensure, skipped, sorted,
    tester_command, tool, write,
};
use crate::differential_suite::SandboxClockOffset;
use crate::ensure_suite::report;
use crate::oracle::{Fixture, Oracle};
use crate::owner_pid_suite::{KillOnDrop, wait_for};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{TESTER, ensure, info};

/// The supplied project root (on the development machine: `~/topics`).
const SRC: &str = "/home/tester/src";

/// The pinned tools of the Nix image (flake.nix: `cacheTools`, the
/// pytest link) and of the under-test bundle (`agent-python3`, `mypy`).
const PYTEST: &str = "/opt/pytest-under-test";

/// npm without a registry: the containers have no network.
const NPM_ENV: &[(&str, &str)] = &[
    ("npm_config_cache", "/home/tester/.npm"),
    ("npm_config_offline", "true"),
    ("npm_config_audit", "false"),
    ("npm_config_fund", "false"),
    ("npm_config_update_notifier", "false"),
];
/// The other tools' runs must not add `__pycache__` directories that only
/// the `python-pycache` scenario expects.
const NO_BYTECODE: &[(&str, &str)] = &[
    ("PYTHONDONTWRITEBYTECODE", "1"),
    ("PYTEST_DISABLE_PLUGIN_AUTOLOAD", "1"),
];

/// Past the 30-day `min_idle_for` of `node-modules` and `python-venv` by
/// a day; the sandbox clock offset allows up to 366 days.
const PAST_THE_FLOOR: Duration = Duration::from_secs(31 * 86_400);
/// "Recent" activity: an hour before the advanced clock's now.
const RECENT: Duration = Duration::from_secs(3600);
/// Bound for a fixture process to report ready, or to finish.
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(60);

const SELECT_NODE_MODULES: &str = "profile:node-modules";
const SELECT_VENV: &str = "profile:python-venv";
const SELECT_PYCACHE: &str = "profile:python-pycache";
const SELECT_MYPY: &str = "profile:mypy-cache";
const SELECT_PYTEST: &str = "profile:pytest-cache";
const SELECT_NODE_COMPILE: &str = "profile:node-compile-cache";

// ---- the harness -----------------------------------------------------------

fn mypy(ctx: &Ctx) -> PathBuf {
    ctx.bin_dir.join("mypy")
}

/// A long-running fixture process started as tester. Killed when dropped.
fn start(
    ctx: &Ctx,
    cwd: &Path,
    env: &[(&str, &str)],
    program: impl AsRef<Path>,
    args: &[&str],
) -> Result<KillOnDrop, String> {
    let program = program.as_ref();
    let child = tester_command(ctx, cwd, env, program, args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("start {}: {e}", program.display()))?;
    Ok(KillOnDrop(child))
}

/// Waits for the ready file a fixture process writes once it is in place
/// (its own pid, as text) and returns that pid.
fn wait_ready(process: &mut KillOnDrop, ready: &Path) -> Result<u32, String> {
    wait_for(FIXTURE_TIMEOUT, &format!("{}", ready.display()), || {
        if let Some(status) = process.0.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "the fixture process exited before {}: {status}",
                ready.display()
            ));
        }
        Ok(std::fs::read_to_string(ready)
            .ok()
            .and_then(|text| text.trim().parse().ok()))
    })
}

/// Waits for a fixture process to end and requires exit status 0.
fn wait_success(process: &mut KillOnDrop, what: &str) -> TestResult {
    let status = wait_for(FIXTURE_TIMEOUT, what, || {
        process.0.try_wait().map_err(|e| e.to_string())
    })?;
    ensure!(status.success(), "{what} ended {status}");
    Ok(())
}

/// Logs the version of each named tool as it runs in this container (the
/// evidence names what ran, not only the flake's pin).
fn versions(ctx: &Ctx, names: &[&str]) -> TestResult {
    for name in names {
        let (program, args): (PathBuf, &[&str]) = match *name {
            "node" => (NODE.into(), &["--version"]),
            "npm" => (NPM.into(), &["--version"]),
            "python" => (python(ctx)?, &["--version"]),
            "uv" => (UV.into(), &["--version"]),
            "mypy" => (mypy(ctx), &["--version"]),
            "pytest" => (PYTEST.into(), &["--version"]),
            other => return Err(format!("no version probe for {other}")),
        };
        let env = if *name == "npm" { NPM_ENV } else { NO_BYTECODE };
        let out = tool(ctx, Path::new("/"), env, &program, args)?;
        info!("tool version: {name}: {}", out.trim());
    }
    Ok(())
}

/// Lets a fixture process blocked on its go file carry on. Written only
/// after the oracle judged the reclaim: it is not part of the run.
fn go(ctx: &Ctx, name: &str) -> TestResult {
    ctx.home
        .write_file(&format!("{CTL}/{name}.go"), b"go\n", 0o644, TESTER)?;
    Ok(())
}

/// Sets `path`'s mtime (`change`) or atime to an hour before the advanced
/// clock's now ([`PAST_THE_FLOOR`]): a change or a read inside every
/// 30-day floor, as `diskgc` sees it.
fn recent(path: &Path, change: bool) -> TestResult {
    let at = SystemTime::now() + PAST_THE_FLOOR - RECENT;
    let times = if change {
        std::fs::FileTimes::new().set_modified(at)
    } else {
        std::fs::FileTimes::new().set_accessed(at)
    };
    std::fs::File::open(path)
        .and_then(|file| file.set_times(times))
        .map_err(|e| format!("set the times of {}: {e}", path.display()))
}

/// The skipped entry's detail contains `fragment` (the deciding rule).
fn expect_detail(report: &Value, candidate: &Path, fragment: &str) -> TestResult {
    let entry = skipped(report, candidate)?;
    ensure!(
        entry["detail"]
            .as_str()
            .is_some_and(|d| d.contains(fragment)),
        "{} detail {}, expected to contain {fragment:?}",
        candidate.display(),
        entry["detail"]
    );
    Ok(())
}

// ---- real-tool fixtures ----------------------------------------------------

/// The local package every npm project depends on, packed into a tarball
/// by the real npm. Returns its `file:` dependency spec.
fn local_package(ctx: &Ctx) -> Result<String, String> {
    write(
        ctx,
        "pkgs/leftpad-local/package.json",
        br#"{"name":"leftpad-local","version":"1.0.0","main":"index.js"}"#,
    )?;
    write(
        ctx,
        "pkgs/leftpad-local/index.js",
        b"module.exports = (s, n) => String(s).padStart(n);\n",
    )?;
    tool(ctx, &home("pkgs/leftpad-local"), NPM_ENV, NPM, &["pack"])?;
    Ok(format!(
        "file:{HOME}/pkgs/leftpad-local/leftpad-local-1.0.0.tgz"
    ))
}

/// An npm project at `src/REL` depending on the local package, with a
/// `server.js` that loads it, writes its pid to `ctl/NAME.ready` (NAME:
/// REL's last component, also its package name) and then waits;
/// `npm install` makes its `node_modules` and lock file.
fn npm_project(ctx: &Ctx, rel: &str, spec: &str) -> Result<PathBuf, String> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let manifest = format!(
        r#"{{"name":"{name}","version":"1.0.0","private":true,"dependencies":{{"leftpad-local":"{spec}"}}}}"#
    );
    write(ctx, &format!("src/{rel}/package.json"), manifest.as_bytes())?;
    let server = format!(
        "const pad = require('leftpad-local');\n\
         require('fs').writeFileSync('{HOME}/{CTL}/{name}.ready', String(process.pid));\n\
         console.log(pad('{name}', 12));\n\
         setInterval(() => {{}}, 1000);\n"
    );
    write(ctx, &format!("src/{rel}/server.js"), server.as_bytes())?;
    let project = home(&format!("src/{rel}"));
    tool(ctx, &project, NPM_ENV, NPM, &["install"])?;
    let modules = project.join("node_modules");
    ensure!(
        project.join("package-lock.json").is_file()
            && modules.join("leftpad-local/index.js").is_file(),
        "npm install made no node_modules or lock file in {}",
        project.display()
    );
    Ok(modules)
}

/// An npm workspace at `src/NAME`: its root declares `packages/*`, member
/// `a` depends on the local package; one `npm install` at the root makes
/// the root's `node_modules` (the member linked, the package hoisted).
fn npm_workspace(ctx: &Ctx, name: &str, spec: &str) -> Result<PathBuf, String> {
    write(
        ctx,
        &format!("src/{name}/package.json"),
        format!(r#"{{"name":"{name}","private":true,"workspaces":["packages/*"]}}"#).as_bytes(),
    )?;
    write(
        ctx,
        &format!("src/{name}/packages/a/package.json"),
        format!(r#"{{"name":"a","version":"1.0.0","dependencies":{{"leftpad-local":"{spec}"}}}}"#)
            .as_bytes(),
    )?;
    let project = home(&format!("src/{name}"));
    tool(ctx, &project, NPM_ENV, NPM, &["install"])?;
    let modules = project.join("node_modules");
    ensure!(
        modules.join("leftpad-local/index.js").is_file()
            && std::fs::symlink_metadata(modules.join("a"))
                .is_ok_and(|m| m.file_type().is_symlink()),
        "npm install did not hoist the package and link the member in {}",
        modules.display()
    );
    Ok(modules)
}

/// The project-tree verdicts of the npm scenario. Real npm installs:
/// `app` and the workspace `mono` idle, reclaimable; `recent` (a source
/// changed an hour ago) and `read-recently` (a dependency file read an
/// hour ago) not idle. Lookalikes: a hand-made `handmade/node_modules`
/// with no `package.json` beside it (filtered: not a project's
/// dependencies); a hand-made nested
/// `app/node_modules/leftpad-local/node_modules`, never a candidate of its
/// own (discovery does not descend into a found candidate: it goes with
/// `app/node_modules`); a real install below a hidden directory
/// (`.tools/cli`), which `find.exclude` never finds.
fn node_projects(ctx: &Ctx, spec: &str) -> Result<Expected, String> {
    let app = npm_project(ctx, "app", spec)?;
    let mono = npm_workspace(ctx, "mono", spec)?;
    let recent_project = npm_project(ctx, "recent", spec)?;
    recent(&home("src/recent/server.js"), true)?;
    let read_recently = npm_project(ctx, "read-recently", spec)?;
    recent(&read_recently.join("leftpad-local/index.js"), false)?;
    write(ctx, "src/handmade/node_modules/x/index.js", b"// by hand\n")?;
    write(
        ctx,
        "src/app/node_modules/leftpad-local/node_modules/inner/index.js",
        b"// by hand\n",
    )?;
    npm_project(ctx, ".tools/cli", spec)?;
    Ok(vec![
        (app.clone(), None),
        (mono, None),
        (recent_project, Some("not_idle")),
        (read_recently, Some("not_idle")),
        (home("src/handmade/node_modules"), Some("filtered")),
    ])
}

/// `python -m venv` (with pip, as users make them) or, `fast`, without.
fn std_venv(ctx: &Ctx, project: &Path, fast: bool) -> TestResult {
    let mut args = vec!["-m", "venv"];
    if fast {
        args.push("--without-pip");
    }
    args.push(".venv");
    tool(ctx, project, NO_BYTECODE, python(ctx)?, &args)?;
    ensure!(
        project.join(".venv/pyvenv.cfg").is_file(),
        "venv made no pyvenv.cfg in {}",
        project.display()
    );
    Ok(())
}

fn uv_venv(ctx: &Ctx, project: &Path) -> TestResult {
    let interpreter = python(ctx)?;
    let interpreter = interpreter.to_str().ok_or("non-UTF-8 interpreter path")?;
    tool(
        ctx,
        project,
        &[
            ("UV_CACHE_DIR", "/home/tester/.cache/uv"),
            ("UV_OFFLINE", "1"),
        ],
        UV,
        &["venv", "--python", interpreter],
    )?;
    ensure!(
        project.join(".venv/pyvenv.cfg").is_file(),
        "uv venv made no pyvenv.cfg in {}",
        project.display()
    );
    Ok(())
}

/// The development package's dry run `development` past the 30-day floor
/// planned exactly `expected` (the positive control of the release
/// project-tree tests). If not, the error names what may have kept them
/// (TASK-363): a dry run's plan names no process, so the processes naming
/// [`SRC`] are listed, and the same reclaim runs (this container is
/// disposable) for its skipped rows' details and blocking processes.
/// Best effort: both come after the dry run, so a process that blocked it
/// and ended since is not seen; processes outside the container show
/// their command line only; a failed diagnostic is reported after the
/// mismatch, never instead of it.
fn expect_positive_control(
    ctx: &Ctx,
    development_package: &Path,
    development: &Run,
    select: &str,
    expected: &Expected,
) -> TestResult {
    let planned = plan_of(&report(development)?)?;
    if planned == sorted(expected) {
        return Ok(());
    }
    let naming = processes_naming(SRC);
    let skipped = run_ensure(ctx, development_package, false, select, Some(SRC), HOME)
        .and_then(|reclaim| report(&reclaim))
        .map_or_else(
            |e| format!("(the diagnostic reclaim failed: {e})"),
            |report| report["skipped"].to_string(),
        );
    Err(format!(
        "positive control: the development build past the floor planned {planned:?}, \
         expected {:?}; processes naming {SRC} then:\n{naming}\nthe same reclaim's skipped \
         rows: {skipped}",
        sorted(expected),
    ))
}

/// Every process whose command line, working directory or environment
/// names a path below `dir`, one line each with its cgroup (which
/// container it is in): a diagnostic for a project kept `in_use` by a
/// process the test did not start (TASK-363).
fn processes_naming(dir: &str) -> String {
    let mut lines = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return "(cannot list /proc)".to_owned();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let read = |what: &str| std::fs::read(format!("/proc/{pid}/{what}")).unwrap_or_default();
        let text = |bytes: Vec<u8>| -> Vec<String> {
            bytes
                .split(|b| *b == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect()
        };
        let cmdline = text(read("cmdline"));
        let environ: Vec<String> = text(read("environ"))
            .into_iter()
            .filter(|var| var.contains(dir))
            .collect();
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        if cmdline.iter().any(|a| a.contains(dir)) || !environ.is_empty() || cwd.starts_with(dir) {
            let cgroup = String::from_utf8_lossy(&read("cgroup")).trim().to_owned();
            lines.push(format!(
                "pid {pid} cgroup {cgroup:?} cwd {cwd:?} cmdline {cmdline:?} environ {environ:?}"
            ));
        }
    }
    if lines.is_empty() {
        "(none)".to_owned()
    } else {
        lines.join("\n")
    }
}

fn pyproject(ctx: &Ctx, name: &str) -> TestResult {
    write(
        ctx,
        &format!("src/{name}/pyproject.toml"),
        format!("[project]\nname = \"{name}\"\nversion = \"0\"\n").as_bytes(),
    )
}

/// The site-packages directory of a virtual environment.
fn site_packages(venv: &Path) -> Result<PathBuf, String> {
    let lib = venv.join("lib");
    let entries = std::fs::read_dir(&lib).map_err(|e| format!("list {}: {e}", lib.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| e.to_string())?
            .path()
            .join("site-packages");
        if path.is_dir() {
            return Ok(path);
        }
    }
    Err(format!("no site-packages below {}", lib.display()))
}

/// The project-tree verdicts of the virtual-environment scenario. Real
/// environments: `pyapp` (`python -m venv`, with pip), `uvapp` (`uv venv`)
/// and `reqapp` (a `requirements.txt` as its manifest) idle, reclaimable;
/// `recentpy` (a source changed an hour ago) not idle; `editable`, whose
/// site-packages names the project's `src` in a `.pth` line as an editable
/// install does, not idle because a project source outside `.venv` was
/// read an hour ago (reads count project-wide then). Lookalikes: a real
/// environment with no manifest beside it (`noman`) and a hand-made
/// `.venv` without `pyvenv.cfg` (`fakevenv`), both filtered.
fn venv_projects(ctx: &Ctx, fast: bool) -> Result<Expected, String> {
    for name in ["pyapp", "uvapp", "recentpy", "editable", "fakevenv"] {
        pyproject(ctx, name)?;
    }
    write(ctx, "src/reqapp/requirements.txt", b"# no dependencies\n")?;
    write(ctx, "src/recentpy/main.py", b"print('recent')\n")?;
    write(ctx, "src/editable/src/editable/__init__.py", b"VALUE = 1\n")?;
    write(ctx, "src/fakevenv/.venv/bin/python", b"#!/bin/sh\n")?;
    ctx.home.mkdirs("src/noman", 0o755, TESTER)?;
    std_venv(ctx, &home("src/pyapp"), fast)?;
    uv_venv(ctx, &home("src/uvapp"))?;
    for name in ["reqapp", "recentpy", "editable", "noman"] {
        std_venv(ctx, &home(&format!("src/{name}")), true)?;
    }
    recent(&home("src/recentpy/main.py"), true)?;
    let site = site_packages(&home("src/editable/.venv"))?;
    let rel = site
        .strip_prefix(HOME)
        .map_err(|e| e.to_string())?
        .join("__editable__.editable-0.pth");
    write(
        ctx,
        rel.to_str().ok_or("non-UTF-8 path")?,
        format!("{SRC}/editable/src\n").as_bytes(),
    )?;
    recent(&home("src/editable/src/editable/__init__.py"), false)?;
    Ok(vec![
        (home("src/pyapp/.venv"), None),
        (home("src/uvapp/.venv"), None),
        (home("src/reqapp/.venv"), None),
        (home("src/recentpy/.venv"), Some("not_idle")),
        (home("src/editable/.venv"), Some("not_idle")),
        (home("src/noman/.venv"), Some("filtered")),
        (home("src/fakevenv/.venv"), Some("filtered")),
    ])
}

/// A Python package `src/pyproj/pkg` (with `pkg.sub` and a lazily
/// imported `pkg.lazy`) imported by the real interpreter, which writes
/// `pkg/__pycache__` and `pkg/sub/__pycache__`; and a pytest project whose
/// assertion rewriting writes `tests/__pycache__`. Lookalikes: a hand-made
/// `__pycache__` at relative depth six and a `__pycache__.old` directory,
/// which `find` never reaches.
fn pycache_projects(ctx: &Ctx) -> Result<Expected, String> {
    pyproject(ctx, "pyproj")?;
    write(ctx, "src/pyproj/pkg/__init__.py", b"")?;
    write(ctx, "src/pyproj/pkg/sub/__init__.py", b"")?;
    write(ctx, "src/pyproj/pkg/sub/mod.py", b"VALUE = 'mod'\n")?;
    write(ctx, "src/pyproj/pkg/lazy.py", b"VALUE = 'lazy'\n")?;
    tool(
        ctx,
        &home("src/pyproj"),
        &[],
        python(ctx)?,
        &["-c", "import pkg.sub.mod"],
    )?;
    pyproject(ctx, "pytested")?;
    write(
        ctx,
        "src/pytested/tests/test_ok.py",
        b"def test_ok():\n    assert 1 + 1 == 2\n",
    )?;
    tool(
        ctx,
        &home("src/pytested"),
        &[("PYTEST_DISABLE_PLUGIN_AUTOLOAD", "1")],
        PYTEST,
        &["-q", "-p", "no:cacheprovider"],
    )?;
    write(ctx, "src/pyproj/a/b/c/d/e/__pycache__/deep.pyc", b"by hand")?;
    write(ctx, "src/pyproj/pkg/__pycache__.old/x.pyc", b"by hand")?;
    let expected = vec![
        (home("src/pyproj/pkg/__pycache__"), None),
        (home("src/pyproj/pkg/sub/__pycache__"), None),
        (home("src/pytested/tests/__pycache__"), None),
    ];
    for (dir, _) in &expected {
        ensure!(dir.is_dir(), "the real tools wrote no {}", dir.display());
    }
    Ok(expected)
}

/// A mypy-checked project `src/NAME`: `main.py` importing a typed
/// package; with `blocker`, a mypy plugin that, when mypy parses a module
/// named `second` (new to its cache, so parsed even when the cache is
/// warm), writes its pid to `ctl/NAME.ready` and waits for `ctl/NAME.go`
/// (by then mypy has opened its cache databases).
fn mypy_project(ctx: &Ctx, name: &str, blocker: bool) -> TestResult {
    let config = if blocker {
        format!(
            "[project]\nname = \"{name}\"\nversion = \"0\"\n[tool.mypy]\nplugins = [\"{SRC}/{name}/blocker.py\"]\n"
        )
    } else {
        format!("[project]\nname = \"{name}\"\nversion = \"0\"\n")
    };
    write(
        ctx,
        &format!("src/{name}/pyproject.toml"),
        config.as_bytes(),
    )?;
    write(
        ctx,
        &format!("src/{name}/typedpkg/__init__.py"),
        b"def double(x: int) -> int:\n    return 2 * x\n",
    )?;
    write(
        ctx,
        &format!("src/{name}/main.py"),
        b"from typedpkg import double\n\nprint(double(2))\n",
    )?;
    if blocker {
        let plugin = format!(
            "import os, time\n\
             from mypy.plugin import Plugin\n\
             \n\
             class Blocker(Plugin):\n    \
                 def get_additional_deps(self, file):\n        \
                     if file.fullname == 'second':\n            \
                         with open('{HOME}/{CTL}/{name}.ready', 'w') as f:\n                \
                             f.write(str(os.getpid()))\n            \
                         while not os.path.exists('{HOME}/{CTL}/{name}.go'):\n                \
                             time.sleep(0.05)\n        \
                     return []\n\
             \n\
             def plugin(version):\n    \
                 return Blocker\n"
        );
        write(ctx, &format!("src/{name}/blocker.py"), plugin.as_bytes())?;
    }
    Ok(())
}

fn run_mypy(ctx: &Ctx, name: &str) -> TestResult {
    let out = tool(
        ctx,
        &home(&format!("src/{name}")),
        NO_BYTECODE,
        mypy(ctx),
        &["main.py"],
    )?;
    ensure!(
        out.contains("Success: no issues found"),
        "mypy in {name}: {out}"
    );
    ensure!(
        home(&format!("src/{name}/.mypy_cache/CACHEDIR.TAG")).is_file(),
        "mypy wrote no tagged .mypy_cache in {name}"
    );
    Ok(())
}

/// A pytest project `src/NAME` with a passing test and one that sleeps
/// while `ctl/NAME.ready` exists without `ctl/NAME.go` (a session in
/// flight); without `SLEEP` in its environment it returns at once.
fn pytest_project(ctx: &Ctx, name: &str) -> TestResult {
    pyproject(ctx, name)?;
    let tests = format!(
        "import os, time\n\
         \n\
         def test_ok():\n    \
             assert 1 + 1 == 2\n\
         \n\
         def test_in_flight():\n    \
             if os.environ.get('SLEEP'):\n        \
                 with open('{HOME}/{CTL}/{name}.ready', 'w') as f:\n            \
                     f.write(str(os.getpid()))\n        \
                 while not os.path.exists('{HOME}/{CTL}/{name}.go'):\n            \
                     time.sleep(0.05)\n"
    );
    write(ctx, &format!("src/{name}/test_x.py"), tests.as_bytes())
}

fn run_pytest(ctx: &Ctx, name: &str, args: &[&str]) -> Result<String, String> {
    tool(
        ctx,
        &home(&format!("src/{name}")),
        NO_BYTECODE,
        PYTEST,
        args,
    )
}

/// A Node script that enables the compile cache in `os.tmpdir()` (as
/// Node's `module.enableCompileCache()` without a directory does) and
/// loads a module, below tester's home.
fn node_scripts(ctx: &Ctx) -> TestResult {
    write(
        ctx,
        "ncc/main.js",
        b"const m = require('module');\n\
          const r = m.enableCompileCache();\n\
          const lib = require('./lib.js');\n\
          console.log(JSON.stringify(r.directory), lib());\n",
    )?;
    write(
        ctx,
        "ncc/lib.js",
        b"module.exports = () => [1, 2, 3].map((x) => x * 2).join(',');\n",
    )?;
    write(ctx, "ncc/lazy.js", b"module.exports = () => 'lazy';\n")?;
    write(
        ctx,
        "ncc/live.js",
        format!(
            "const m = require('module');\n\
             m.enableCompileCache();\n\
             const fs = require('fs');\n\
             require('./lib.js');\n\
             fs.writeFileSync('{HOME}/{CTL}/ncc-live.ready', String(process.pid));\n\
             const wait = setInterval(() => {{\n  \
                 if (!fs.existsSync('{HOME}/{CTL}/ncc-live.go')) return;\n  \
                 clearInterval(wait);\n  \
                 console.log(require('./lazy.js')());\n\
             }}, 50);\n"
        )
        .as_bytes(),
    )
}

fn run_node_with_cache(ctx: &Ctx, tmpdir: &str) -> Result<String, String> {
    tool(ctx, &home("ncc"), &[("TMPDIR", tmpdir)], NODE, &["main.js"])
}

/// The compile caches Node makes for `TMPDIR=/tmp` and for a shell's
/// `TMPDIR` below `/tmp` (a `nix-shell.*` directory of the temp-prefix
/// preset). Lookalikes: a cache Node made where `NODE_COMPILE_CACHE`
/// pointed outside the preset (`/tmp/x/node-compile-cache`) and a
/// hand-made `/tmp/node-compile-cache-old`, which `find` never reaches.
fn node_compile_caches(ctx: &Ctx) -> Result<Expected, String> {
    node_scripts(ctx)?;
    ctx.scratch.mkdirs("nix-shell.Ab12Cd", 0o700, TESTER)?;
    ctx.scratch.mkdirs("x", 0o755, TESTER)?;
    run_node_with_cache(ctx, "/tmp")?;
    run_node_with_cache(ctx, "/tmp/nix-shell.Ab12Cd")?;
    tool(
        ctx,
        &home("ncc"),
        &[("NODE_COMPILE_CACHE", "/tmp/x/node-compile-cache")],
        NODE,
        &["lib.js"],
    )?;
    ctx.scratch
        .mkdirs("node-compile-cache-old/v0", 0o755, TESTER)?;
    let expected = vec![
        (PathBuf::from("/tmp/node-compile-cache"), None),
        (
            PathBuf::from("/tmp/nix-shell.Ab12Cd/node-compile-cache"),
            None,
        ),
    ];
    for dir in [
        "/tmp/node-compile-cache",
        "/tmp/nix-shell.Ab12Cd/node-compile-cache",
        "/tmp/x/node-compile-cache",
    ] {
        ensure!(
            has_entries(Path::new(dir))?,
            "Node wrote no compile cache in {dir}"
        );
    }
    Ok(expected)
}

/// Whether `dir` holds any entry below it (a cache Node actually wrote).
fn has_entries(dir: &Path) -> Result<bool, String> {
    let mut pending = vec![dir.to_owned()];
    while let Some(path) = pending.pop() {
        let entries =
            std::fs::read_dir(&path).map_err(|e| format!("list {}: {e}", path.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

// ---- node-modules ----------------------------------------------------------

/// docs/testing.md, Admission evidence (`node-modules`): real npm
/// projects, dry run and reclaim of exactly the idle ones, then `npm ci`
/// restores them and Node loads the dependency again.
pub fn node_modules_real_npm_projects_reclaim_only_idle_projects_then_npm_ci_restores(
    ctx: &Ctx,
) -> TestResult {
    versions(ctx, &["node", "npm"])?;
    control_dir(ctx)?;
    let spec = local_package(ctx)?;
    let expected = node_projects(ctx, &spec)?;
    let _clock = SandboxClockOffset::install(PAST_THE_FLOOR.as_secs())?;
    let reclaimed = dry_run_then_reclaim(
        ctx,
        &ctx.diskgc(),
        SELECT_NODE_MODULES,
        Some(SRC),
        HOME,
        &expected,
    )?;
    expect_detail(&reclaimed, &home("src/recent/node_modules"), "server.js")?;
    expect_detail(
        &reclaimed,
        &home("src/read-recently/node_modules"),
        "rule 2 (no read)",
    )?;
    expect_detail(
        &reclaimed,
        &home("src/handmade/node_modules"),
        "no regular package.json beside it",
    )?;
    ensure!(
        exists(&home("src/.tools/cli/node_modules/leftpad-local")),
        "the install below a hidden directory was touched"
    );
    for name in ["app", "mono"] {
        let project = home(&format!("src/{name}"));
        tool(ctx, &project, NPM_ENV, NPM, &["ci"])?;
        ensure!(
            project
                .join("node_modules/leftpad-local/index.js")
                .is_file(),
            "npm ci did not restore {name}'s node_modules"
        );
    }
    let out = tool(
        ctx,
        &home("src/app"),
        &[],
        NODE,
        &["-e", "console.log(require('leftpad-local')('ok', 4))"],
    )?;
    ensure!(out == "  ok\n", "node after npm ci printed {out:?}");
    Ok(())
}

/// docs/testing.md, Admission evidence (`node-modules`, live): idle
/// projects in use by a running Node are kept as `in_use` naming the
/// process and the evidence: one with its working directory in the
/// project (the contract's held-path test), one started from `/` naming
/// the project's script (the contract's command-line test), and one
/// holding a file of `node_modules` open (In use itself). Once they end,
/// the same reclaim deletes all three (the positive control).
pub fn node_modules_live_node_processes_keep_their_projects(ctx: &Ctx) -> TestResult {
    versions(ctx, &["node", "npm"])?;
    control_dir(ctx)?;
    let spec = local_package(ctx)?;
    let served = npm_project(ctx, "served", &spec)?;
    let named = npm_project(ctx, "named", &spec)?;
    let held = npm_project(ctx, "held", &spec)?;
    let mut by_cwd = start(ctx, &home("src/served"), &[], NODE, &["server.js"])?;
    let mut by_name = start(
        ctx,
        Path::new("/"),
        &[],
        NODE,
        &["/home/tester/src/named/server.js"],
    )?;
    let holder = format!(
        "const fs = require('fs');\n\
         globalThis.held = fs.openSync('{SRC}/held/node_modules/leftpad-local/index.js', 'r');\n\
         fs.writeFileSync('{HOME}/{CTL}/held.ready', String(process.pid));\n\
         setInterval(() => {{}}, 1000);\n"
    );
    let mut by_fd = start(ctx, Path::new("/"), &[], NODE, &["-e", &holder])?;
    let cwd_pid = wait_ready(&mut by_cwd, &home("ctl/served.ready"))?;
    let name_pid = wait_ready(&mut by_name, &home("ctl/named.ready"))?;
    let fd_pid = wait_ready(&mut by_fd, &home("ctl/held.ready"))?;
    let _clock = SandboxClockOffset::install(PAST_THE_FLOOR.as_secs())?;
    let expected: Expected = vec![
        (served.clone(), Some("in_use")),
        (named.clone(), Some("in_use")),
        (held.clone(), Some("in_use")),
    ];
    let reclaimed = dry_run_then_reclaim(
        ctx,
        &ctx.diskgc(),
        SELECT_NODE_MODULES,
        Some(SRC),
        HOME,
        &expected,
    )?;
    let contract = |pid: u32| format!("liveness_contract node-project: pid {pid} (");
    expect_blocker(&reclaimed, &served, cwd_pid, "cwd", &contract(cwd_pid))?;
    expect_blocker(
        &reclaimed,
        &named,
        name_pid,
        "command_line",
        &contract(name_pid),
    )?;
    expect_blocker(&reclaimed, &held, fd_pid, "fd", "")?;
    drop((by_cwd, by_name, by_fd));
    let released: Expected = vec![(served, None), (named, None), (held, None)];
    dry_run_then_reclaim(
        ctx,
        &ctx.diskgc(),
        SELECT_NODE_MODULES,
        Some(SRC),
        HOME,
        &released,
    )?;
    Ok(())
}

/// docs/testing.md, Admission evidence (`node-modules`, home install): a
/// real `npm install` in tester's home (`~/package.json` and
/// `~/node_modules`, as on the development machine) keeps every project's
/// `node_modules` below it `filtered`, idle or not: Node would resolve the
/// home's copies once a project's own are gone. Nothing is deleted.
pub fn node_modules_home_level_install_filters_every_project_below_it(ctx: &Ctx) -> TestResult {
    versions(ctx, &["node", "npm"])?;
    control_dir(ctx)?;
    let spec = local_package(ctx)?;
    let app = npm_project(ctx, "app", &spec)?;
    let mono = npm_workspace(ctx, "mono", &spec)?;
    write(
        ctx,
        "package.json",
        format!(r#"{{"dependencies":{{"leftpad-local":"{spec}"}}}}"#).as_bytes(),
    )?;
    tool(ctx, Path::new(HOME), NPM_ENV, NPM, &["install"])?;
    ensure!(
        home("node_modules/leftpad-local").is_dir(),
        "npm install made no ~/node_modules"
    );
    let _clock = SandboxClockOffset::install(PAST_THE_FLOOR.as_secs())?;
    let expected: Expected = vec![(app.clone(), Some("filtered")), (mono, Some("filtered"))];
    let reclaimed = dry_run_then_reclaim(
        ctx,
        &ctx.diskgc(),
        SELECT_NODE_MODULES,
        Some(SRC),
        HOME,
        &expected,
    )?;
    expect_detail(&reclaimed, &app, "/home/tester/node_modules")?;
    Ok(())
}

// ---- python-venv -----------------------------------------------------------

/// docs/testing.md, Admission evidence (`python-venv`): real `venv` and
/// `uv venv` environments, dry run and reclaim of exactly the idle ones;
/// then `python -m venv` and `uv venv` make them again and run.
pub fn python_venv_real_venv_and_uv_projects_reclaim_only_idle_projects_then_recreate(
    ctx: &Ctx,
) -> TestResult {
    versions(ctx, &["python", "uv"])?;
    let expected = venv_projects(ctx, false)?;
    let _clock = SandboxClockOffset::install(PAST_THE_FLOOR.as_secs())?;
    let reclaimed =
        dry_run_then_reclaim(ctx, &ctx.diskgc(), SELECT_VENV, Some(SRC), HOME, &expected)?;
    expect_detail(&reclaimed, &home("src/recentpy/.venv"), "main.py")?;
    expect_detail(
        &reclaimed,
        &home("src/editable/.venv"),
        "reads counted project-wide",
    )?;
    expect_detail(
        &reclaimed,
        &home("src/fakevenv/.venv"),
        "no regular pyvenv.cfg in it",
    )?;
    std_venv(ctx, &home("src/pyapp"), false)?;
    uv_venv(ctx, &home("src/uvapp"))?;
    std_venv(ctx, &home("src/reqapp"), true)?;
    for name in ["pyapp", "uvapp", "reqapp"] {
        let interpreter = home(&format!("src/{name}/.venv/bin/python"));
        let out = tool(
            ctx,
            Path::new("/"),
            NO_BYTECODE,
            &interpreter,
            &["-c", "import sys; print(sys.prefix)"],
        )?;
        ensure!(
            out.trim() == home(&format!("src/{name}/.venv")).display().to_string(),
            "the re-created {name} environment's prefix is {out:?}"
        );
    }
    Ok(())
}

/// The shell script an "activated" interactive shell runs, read from its
/// standard input (`sh -s`, so its command line names nothing): it
/// sources the environment's own `activate`, optionally leaves the
/// project, reports its pid, then waits for one line and runs `python`.
fn activated_shell_script(name: &str, leave: bool) -> String {
    format!(
        ". {SRC}/{name}/.venv/bin/activate\n\
         {}\
         echo $$ > {HOME}/{CTL}/{name}.ready\n\
         read line\n\
         python -c 'import sys; print(sys.prefix)' > {HOME}/{CTL}/{name}.out 2>&1\n\
         echo $? >> {HOME}/{CTL}/{name}.out\n",
        if leave { "cd /\n" } else { "" }
    )
}

/// Starts `sh -s` as tester in `cwd`, feeds it `script` and keeps its
/// standard input open (the shell then waits in `read`).
fn start_shell(ctx: &Ctx, cwd: &Path, script: &str) -> Result<(KillOnDrop, ChildStdin), String> {
    let mut child = tester_command(ctx, cwd, &[], Path::new("/bin/sh"), &["-s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("start sh -s: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("sh -s has no stdin")?;
    stdin
        .write_all(script.as_bytes())
        .map_err(|e| format!("feed sh -s: {e}"))?;
    Ok((KillOnDrop(child), stdin))
}

/// docs/testing.md, Admission evidence (`python-venv`, live): idle
/// projects in use are kept as `in_use`: one by a Python started from `/`
/// as the environment's `bin/python` (the contract's command-line test)
/// and one by an interactive shell that activated the environment and
/// stays in the project (its working directory). The activated shell
/// then runs the environment's `python`.
pub fn python_venv_live_interpreter_and_activated_shell_keep_their_projects(
    ctx: &Ctx,
) -> TestResult {
    versions(ctx, &["python"])?;
    control_dir(ctx)?;
    for name in ["interp", "shellin"] {
        pyproject(ctx, name)?;
        std_venv(ctx, &home(&format!("src/{name}")), true)?;
    }
    let script = format!(
        "import os, time\n\
         open('{HOME}/{CTL}/interp.ready', 'w').write(str(os.getpid()))\n\
         time.sleep(600)\n"
    );
    let mut interpreter = start(
        ctx,
        Path::new("/"),
        NO_BYTECODE,
        "/home/tester/src/interp/.venv/bin/python",
        &["-c", &script],
    )?;
    let (mut shell, mut stdin) = start_shell(
        ctx,
        &home("src/shellin"),
        &activated_shell_script("shellin", false),
    )?;
    let interp_pid = wait_ready(&mut interpreter, &home("ctl/interp.ready"))?;
    let shell_pid = wait_ready(&mut shell, &home("ctl/shellin.ready"))?;
    let _clock = SandboxClockOffset::install(PAST_THE_FLOOR.as_secs())?;
    let interp = home("src/interp/.venv");
    let shellin = home("src/shellin/.venv");
    let expected: Expected = vec![
        (interp.clone(), Some("in_use")),
        (shellin.clone(), Some("in_use")),
    ];
    let reclaimed =
        dry_run_then_reclaim(ctx, &ctx.diskgc(), SELECT_VENV, Some(SRC), HOME, &expected)?;
    let contract = |pid: u32| format!("liveness_contract python-project: pid {pid} (");
    expect_blocker(
        &reclaimed,
        &interp,
        interp_pid,
        "command_line",
        &contract(interp_pid),
    )?;
    expect_blocker(&reclaimed, &shellin, shell_pid, "cwd", &contract(shell_pid))?;
    stdin
        .write_all(b"go\n")
        .map_err(|e| format!("resume the shell: {e}"))?;
    drop(stdin);
    wait_success(&mut shell, "the activated shell")?;
    let out = std::fs::read_to_string(home("ctl/shellin.out"))
        .map_err(|e| format!("read the shell's output: {e}"))?;
    ensure!(
        out == format!("{}\n0\n", shellin.display()),
        "the activated shell's python after the reclaim: {out:?}"
    );
    Ok(())
}

/// docs/testing.md, Admission evidence (`python-venv`, residual): an
/// interactive shell that activated a project's environment and then left
/// the project holds nothing the kernel shows and names nothing in its
/// initial environment or command line (`VIRTUAL_ENV` is exported after
/// `exec`). The idle environment is reclaimed: the named residual of
/// docs/safety.md (Project dependency directories, a long-running
/// consumer that names nothing in the project). Its next `python` finds
/// no interpreter, and `python -m venv` makes the environment again.
pub fn python_venv_activated_shell_that_left_its_project_is_a_named_residual(
    ctx: &Ctx,
) -> TestResult {
    versions(ctx, &["python"])?;
    control_dir(ctx)?;
    pyproject(ctx, "left")?;
    let project = home("src/left");
    std_venv(ctx, &project, true)?;
    let (mut shell, mut stdin) = start_shell(ctx, &project, &activated_shell_script("left", true))?;
    wait_ready(&mut shell, &home("ctl/left.ready"))?;
    let _clock = SandboxClockOffset::install(PAST_THE_FLOOR.as_secs())?;
    let expected: Expected = vec![(project.join(".venv"), None)];
    dry_run_then_reclaim(ctx, &ctx.diskgc(), SELECT_VENV, Some(SRC), HOME, &expected)?;
    stdin
        .write_all(b"go\n")
        .map_err(|e| format!("resume the shell: {e}"))?;
    drop(stdin);
    wait_success(&mut shell, "the activated shell")?;
    let out = std::fs::read_to_string(home("ctl/left.out"))
        .map_err(|e| format!("read the shell's output: {e}"))?;
    info!("the activated shell's python after the reclaim: {out:?}");
    ensure!(
        out.lines().last() == Some("127"),
        "the shell's python after its environment was reclaimed did not exit 127 \
         (not found): {out:?}"
    );
    std_venv(ctx, &project, true)?;
    Ok(())
}

/// TASK-359 (docs/testing.md, Admission evidence): `python-venv` through
/// the gate-lifted release-candidate package, whose 0.2 catalog holds it.
/// That build has no sandbox clock offset, so the reclaim side of the
/// 30-day floor cannot run in a container; this shows the keep side on
/// the real clock. Two environments made now by the real tools (`python
/// -m venv`, `uv venv`) are what the development package (gated, so it
/// reads the offset; not the e2e `diskgc`, which the release runner
/// replaces with the release candidate), its clock advanced past the
/// floor, plans to reclaim: the positive control, nothing but the floor
/// keeps them. With the same offset installed, which the release build
/// does not read, the release candidate keeps both `not_idle`, and its
/// reclaim deletes nothing.
pub fn release_candidate_python_venv_keeps_projects_inside_the_30_day_floor(
    ctx: &Ctx,
) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "python-venv")?;
    versions(ctx, &["python", "uv"])?;
    // Project names of this test alone (TASK-363), defensive: the project
    // contract reads other processes' command lines and working
    // directories as text, so no other test's process names these paths.
    // (Host-PID-namespace tests never overlap, and a container's
    // processes end with it, probed: no route is known.)
    for name in ["rc-venv-pyapp", "rc-venv-uvapp"] {
        pyproject(ctx, name)?;
    }
    std_venv(ctx, &home("src/rc-venv-pyapp"), true)?;
    uv_venv(ctx, &home("src/rc-venv-uvapp"))?;
    let venvs = [
        home("src/rc-venv-pyapp/.venv"),
        home("src/rc-venv-uvapp/.venv"),
    ];
    let _clock = SandboxClockOffset::install(PAST_THE_FLOOR.as_secs())?;
    let oracle = Oracle::arm(Fixture::new())?;
    let development_package = crate::release_suite::binary_path(ctx, "diskgc-development");
    let development = run_ensure(
        ctx,
        &development_package,
        true,
        SELECT_VENV,
        Some(SRC),
        HOME,
    )?;
    oracle.judge(&development)?.assert_clean()?;
    development.expect_code(0)?;
    let past_the_floor: Expected = venvs.iter().map(|v| (v.clone(), None)).collect();
    expect_positive_control(
        ctx,
        &development_package,
        &development,
        SELECT_VENV,
        &past_the_floor,
    )?;
    let inside_the_floor: Expected = venvs
        .iter()
        .map(|v| (v.clone(), Some("not_idle")))
        .collect();
    dry_run_then_reclaim(
        ctx,
        &binary,
        SELECT_VENV,
        Some(SRC),
        HOME,
        &inside_the_floor,
    )?;
    Ok(())
}

// ---- python-pycache --------------------------------------------------------

/// docs/testing.md, Admission evidence (`python-pycache`): bytecode the
/// real interpreter and pytest wrote, dry run and reclaim of exactly
/// those directories while a Python that imported from them still runs
/// in the project (tolerant: it holds nothing there, so nothing keeps
/// them); that process then imports another module, which the
/// interpreter compiles and caches again, and a fresh import rewrites
/// every cache.
pub fn python_pycache_real_imports_reclaim_then_a_live_importer_recompiles(
    ctx: &Ctx,
) -> TestResult {
    pycache_scenario(ctx, &ctx.diskgc())
}

/// TASK-359: [`python_pycache_real_imports_reclaim_then_a_live_importer_recompiles`]
/// through the gate-lifted release-candidate package, whose 0.2 catalog
/// holds `python-pycache`.
pub fn release_candidate_python_pycache_reclaim_then_a_live_importer_recompiles(
    ctx: &Ctx,
) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "python-pycache")?;
    pycache_scenario(ctx, &binary)
}

fn pycache_scenario(ctx: &Ctx, binary: &Path) -> TestResult {
    versions(ctx, &["python", "pytest"])?;
    control_dir(ctx)?;
    let expected = pycache_projects(ctx)?;
    let script = format!(
        "import os, time, pkg.sub.mod\n\
         open('{HOME}/{CTL}/importer.ready', 'w').write(str(os.getpid()))\n\
         while not os.path.exists('{HOME}/{CTL}/importer.go'):\n    \
             time.sleep(0.05)\n\
         import pkg.lazy\n\
         assert pkg.lazy.VALUE == 'lazy'\n"
    );
    let mut importer = start(
        ctx,
        &home("src/pyproj"),
        &[],
        python(ctx)?,
        &["-c", &script],
    )?;
    wait_ready(&mut importer, &home("ctl/importer.ready"))?;
    dry_run_then_reclaim(ctx, binary, SELECT_PYCACHE, Some(SRC), HOME, &expected)?;
    ensure!(
        exists(&home("src/pyproj/a/b/c/d/e/__pycache__/deep.pyc"))
            && exists(&home("src/pyproj/pkg/__pycache__.old/x.pyc")),
        "a lookalike outside the profile's find was touched"
    );
    go(ctx, "importer")?;
    wait_success(&mut importer, "the live importer")?;
    ensure!(
        cached(&home("src/pyproj/pkg/__pycache__"), "lazy.")?,
        "the live importer did not cache pkg.lazy again"
    );
    tool(
        ctx,
        &home("src/pyproj"),
        &[],
        python(ctx)?,
        &["-c", "import pkg.sub.mod"],
    )?;
    ensure!(
        cached(&home("src/pyproj/pkg/sub/__pycache__"), "mod.")?,
        "a fresh import did not rewrite pkg/sub/__pycache__"
    );
    Ok(())
}

/// Whether `dir` holds a `.pyc` whose name starts with `stem`.
fn cached(dir: &Path, stem: &str) -> Result<bool, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("list {}: {e}", dir.display()))?;
    for entry in entries {
        let name = entry.map_err(|e| e.to_string())?.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(stem) && name.ends_with(".pyc") {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---- mypy-cache ------------------------------------------------------------

/// The SQLite shards `<python version>/cache.N.db` of a mypy cache.
fn cache_shards(cache: &Path) -> Result<Vec<PathBuf>, String> {
    let mut shards = Vec::new();
    for version in std::fs::read_dir(cache).map_err(|e| format!("list {}: {e}", cache.display()))? {
        let version = version.map_err(|e| e.to_string())?.path();
        if !version.is_dir() {
            continue;
        }
        for entry in
            std::fs::read_dir(&version).map_err(|e| format!("list {}: {e}", version.display()))?
        {
            let path = entry.map_err(|e| e.to_string())?.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with("cache.") && name.ends_with(".db") {
                shards.push(path);
            }
        }
    }
    shards.sort();
    Ok(shards)
}

/// docs/testing.md, Admission evidence (`mypy-cache`): the cache the real
/// mypy writes (SQLite shards behind a `CACHEDIR.TAG`), dry run and
/// reclaim of exactly it while a second mypy run is in flight in another
/// project (tagged by an earlier run, re-run after a module was added),
/// paused by a plugin with its cache databases open: that cache is kept
/// `in_use` (descriptor), and the run then completes. The deleted
/// cache is rebuilt by the next run. Lookalikes: a hand-made
/// `.mypy_cache` without `CACHEDIR.TAG` and a copy named `.mypy_cache.bak`,
/// which `find` never reaches.
pub fn mypy_cache_real_mypy_reclaim_then_recheck_and_a_run_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    mypy_scenario(ctx, &ctx.diskgc())
}

/// TASK-359: [`mypy_cache_real_mypy_reclaim_then_recheck_and_a_run_in_flight_is_kept`]
/// through the gate-lifted release-candidate package, whose 0.2 catalog
/// holds `mypy-cache`.
pub fn release_candidate_mypy_cache_reclaim_and_a_run_in_flight_is_kept(ctx: &Ctx) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "mypy-cache")?;
    mypy_scenario(ctx, &binary)
}

fn mypy_scenario(ctx: &Ctx, binary: &Path) -> TestResult {
    versions(ctx, &["mypy"])?;
    control_dir(ctx)?;
    mypy_project(ctx, "typed", false)?;
    run_mypy(ctx, "typed")?;
    mypy_project(ctx, "inflight", true)?;
    run_mypy(ctx, "inflight")?;
    write(
        ctx,
        "src/inflight/second.py",
        b"from typedpkg import double\n\nX: int = double(3)\n",
    )?;
    write(
        ctx,
        "src/inflight/sub/.mypy_cache/3.14/cache.0.db",
        b"by hand",
    )?;
    write(ctx, "src/typed/.mypy_cache.bak/CACHEDIR.TAG", b"by hand")?;
    let mut run = start(
        ctx,
        &home("src/inflight"),
        NO_BYTECODE,
        mypy(ctx),
        &["main.py", "second.py"],
    )?;
    let pid = wait_ready(&mut run, &home("ctl/inflight.ready"))?;
    let cache = home("src/inflight/.mypy_cache");
    let held = held_in(pid, &cache)?;
    let shards = cache_shards(&cache)?;
    ensure!(
        !shards.is_empty() && shards.iter().all(|shard| held.contains(shard)),
        "precondition: the paused mypy (pid {pid}) holds {held:?}, not every cache shard \
         {shards:?}"
    );
    info!("the paused mypy holds every cache shard: {held:?}");
    let typed = home("src/typed/.mypy_cache");
    let expected: Expected = vec![(typed.clone(), None), (cache.clone(), Some("in_use"))];
    let reclaimed = dry_run_then_reclaim(ctx, binary, SELECT_MYPY, Some(SRC), HOME, &expected)?;
    expect_blocker(&reclaimed, &cache, pid, "fd", "")?;
    go(ctx, "inflight")?;
    wait_success(&mut run, "the mypy run in flight")?;
    run_mypy(ctx, "typed")?;
    ensure!(
        exists(&typed.join("CACHEDIR.TAG")),
        "mypy did not rebuild the reclaimed cache"
    );
    Ok(())
}

// ---- pytest-cache ----------------------------------------------------------

/// docs/testing.md, Admission evidence (`pytest-cache`): the caches the
/// real pytest writes, dry run and reclaim of exactly them while a pytest
/// session is in flight in one of the projects (tolerant: the session
/// holds nothing there). The session then ends with exit 0 and writes its
/// cache again, and `pytest --lf` in the other project, with no record of
/// the earlier failure, runs every test and rewrites its cache.
/// Lookalikes: a hand-made `.pytest_cache` at relative depth five and a
/// `.pytest_cache.bak`, which `find` never reaches.
pub fn pytest_cache_real_pytest_reclaim_then_a_live_session_rewrites_it(ctx: &Ctx) -> TestResult {
    pytest_cache_scenario(ctx, &ctx.diskgc())
}

/// TASK-359: [`pytest_cache_real_pytest_reclaim_then_a_live_session_rewrites_it`]
/// through the gate-lifted release-candidate package, whose 0.2 catalog
/// holds `pytest-cache`.
pub fn release_candidate_pytest_cache_reclaim_then_a_live_session_rewrites_it(
    ctx: &Ctx,
) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "pytest-cache")?;
    pytest_cache_scenario(ctx, &binary)
}

fn pytest_cache_scenario(ctx: &Ctx, binary: &Path) -> TestResult {
    versions(ctx, &["pytest"])?;
    control_dir(ctx)?;
    pytest_project(ctx, "tested")?;
    write(
        ctx,
        "src/tested/test_fails.py",
        b"import os\n\ndef test_fails_once():\n    assert not os.environ.get('FAIL')\n",
    )?;
    let failed = tester_command(
        ctx,
        &home("src/tested"),
        &[NO_BYTECODE, &[("FAIL", "1")]].concat(),
        Path::new(PYTEST),
        &["-q"],
    )
    .stdin(Stdio::null())
    .output()
    .map_err(|e| format!("start pytest: {e}"))?;
    ensure!(
        failed.status.code() == Some(1),
        "the failing pytest run ended {}",
        failed.status
    );
    ensure!(
        lastfailed(&home("src/tested"))?.contains("test_fails_once"),
        "pytest recorded no failure"
    );
    pytest_project(ctx, "live")?;
    run_pytest(ctx, "live", &["-q"])?;
    write(ctx, "src/a/b/c/d/.pytest_cache/v/cache/nodeids", b"[]")?;
    write(ctx, "src/tested/.pytest_cache.bak/README.md", b"by hand")?;
    let mut session = start(
        ctx,
        &home("src/live"),
        &[NO_BYTECODE, &[("SLEEP", "1")]].concat(),
        PYTEST,
        &["-q"],
    )?;
    wait_ready(&mut session, &home("ctl/live.ready"))?;
    let expected: Expected = vec![
        (home("src/tested/.pytest_cache"), None),
        (home("src/live/.pytest_cache"), None),
    ];
    dry_run_then_reclaim(ctx, binary, SELECT_PYTEST, Some(SRC), HOME, &expected)?;
    go(ctx, "live")?;
    wait_success(&mut session, "the pytest session in flight")?;
    ensure!(
        home("src/live/.pytest_cache/v/cache/nodeids").is_file(),
        "the session in flight did not write its cache again"
    );
    let rerun = run_pytest(ctx, "tested", &["-q", "--lf"])?;
    info!("pytest --lf after the reclaim: {rerun}");
    ensure!(
        rerun.contains("3 passed"),
        "pytest --lf without a record did not run every test: {rerun}"
    );
    ensure!(
        home("src/tested/.pytest_cache/v/cache/nodeids").is_file(),
        "pytest --lf did not rewrite the reclaimed cache"
    );
    Ok(())
}

/// pytest's record of the last failures in `project`.
fn lastfailed(project: &Path) -> Result<String, String> {
    let path = project.join(".pytest_cache/v/cache/lastfailed");
    std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))
}

// ---- node-compile-cache ----------------------------------------------------

/// docs/testing.md, Admission evidence (`node-compile-cache`): the caches
/// the real Node writes under `/tmp` and a shell's `TMPDIR`, dry run and
/// reclaim of exactly them while a Node that enabled one still runs
/// (tolerant: it holds nothing there). That Node then loads another
/// module and exits 0, its cache writes failing silently; the next start
/// makes the cache again.
pub fn node_compile_cache_real_node_reclaim_then_a_live_process_recompiles(
    ctx: &Ctx,
) -> TestResult {
    node_compile_cache_scenario(ctx, &ctx.diskgc())
}

/// TASK-359: [`node_compile_cache_real_node_reclaim_then_a_live_process_recompiles`]
/// through the gate-lifted release-candidate package, whose 0.2 catalog
/// holds `node-compile-cache`.
pub fn release_candidate_node_compile_cache_reclaim_then_a_live_process_recompiles(
    ctx: &Ctx,
) -> TestResult {
    let binary = crate::release_suite::release_candidate_with(ctx, "node-compile-cache")?;
    node_compile_cache_scenario(ctx, &binary)
}

fn node_compile_cache_scenario(ctx: &Ctx, binary: &Path) -> TestResult {
    versions(ctx, &["node"])?;
    control_dir(ctx)?;
    let expected = node_compile_caches(ctx)?;
    let mut live = start(
        ctx,
        &home("ncc"),
        &[("TMPDIR", "/tmp/nix-shell.Ab12Cd")],
        NODE,
        &["live.js"],
    )?;
    wait_ready(&mut live, &home("ctl/ncc-live.ready"))?;
    dry_run_then_reclaim(ctx, binary, SELECT_NODE_COMPILE, None, "/tmp", &expected)?;
    ensure!(
        exists(Path::new("/tmp/x/node-compile-cache"))
            && exists(Path::new("/tmp/node-compile-cache-old/v0")),
        "a lookalike outside the profile's find was touched"
    );
    go(ctx, "ncc-live")?;
    wait_success(&mut live, "the live Node")?;
    run_node_with_cache(ctx, "/tmp")?;
    ensure!(
        has_entries(Path::new("/tmp/node-compile-cache"))?,
        "the next Node start did not make the compile cache again"
    );
    Ok(())
}

// ---- the candidate catalog -------------------------------------------------

/// The family this suite builds fixtures for: TASK-350's six, five of them
/// in the 0.2 catalog since TASK-359, `node-modules` on the candidate list
/// (held, TASK-365).
pub(crate) const FAMILY: &[&str] = &[
    "node-modules",
    "python-venv",
    "python-pycache",
    "mypy-cache",
    "pytest-cache",
    "node-compile-cache",
];

/// docs/testing.md, Admission evidence (candidate catalog): the release
/// build's catalog code path with the family in its catalog (five in the
/// real 0.2 catalog since TASK-359, `node-modules` through the test-only
/// candidate list since TASK-365); its catalog is checked by
/// [`check_candidate_catalog`]. Over one fixture of
/// every family member (real tools), its dry run plans exactly what the
/// development build's does, a live Node keeps its project with the same
/// verdict, and its reclaims delete exactly the reclaimable set; the
/// tools then rebuild.
pub fn candidate_catalog_project_tree_family_matches_the_development_build(
    ctx: &Ctx,
) -> TestResult {
    versions(ctx, &["node", "npm", "python", "uv", "mypy", "pytest"])?;
    let candidate = check_candidate_catalog(ctx, FAMILY)?;

    control_dir(ctx)?;
    let spec = local_package(ctx)?;
    let mut expected = node_projects(ctx, &spec)?;
    let served = npm_project(ctx, "served", &spec)?;
    expected.push((served.clone(), Some("in_use")));
    expected.extend(venv_projects(ctx, true)?);
    expected.extend(pycache_projects(ctx)?);
    mypy_project(ctx, "typed", false)?;
    run_mypy(ctx, "typed")?;
    expected.push((home("src/typed/.mypy_cache"), None));
    pytest_project(ctx, "tested")?;
    run_pytest(ctx, "tested", &["-q"])?;
    expected.push((home("src/tested/.pytest_cache"), None));
    let compile_caches = node_compile_caches(ctx)?;
    let mut server = start(ctx, &home("src/served"), &[], NODE, &["server.js"])?;
    let pid = wait_ready(&mut server, &home("ctl/served.ready"))?;

    let _clock = SandboxClockOffset::install(PAST_THE_FLOOR.as_secs())?;
    // node-compile-cache's root is /tmp, another filesystem: its own runs.
    let select = FAMILY
        .iter()
        .filter(|name| **name != "node-compile-cache")
        .map(|name| format!("profile:{name}"))
        .collect::<Vec<_>>()
        .join(" || ");
    let select = select.as_str();
    let oracle = Oracle::arm(Fixture::new())?;
    let development = run_ensure(ctx, &ctx.diskgc(), true, select, Some(SRC), HOME)?;
    let candidate_plan = run_ensure(ctx, &candidate, true, select, Some(SRC), HOME)?;
    let development_tmp = run_ensure(ctx, &ctx.diskgc(), true, SELECT_NODE_COMPILE, None, "/tmp")?;
    oracle
        .judge_runs(&[&development, &candidate_plan, &development_tmp])?
        .assert_clean()?;
    for run in [&development, &candidate_plan, &development_tmp] {
        run.expect_code(0)?;
    }
    let development = plan_of(&report(&development)?)?;
    ensure!(
        development == sorted(&expected),
        "the development build planned {development:?}, expected {:?}",
        sorted(&expected)
    );
    ensure!(
        plan_of(&report(&candidate_plan)?)? == development,
        "the candidate catalog planned {:?}, the development build {development:?}",
        plan_of(&report(&candidate_plan)?)?
    );
    ensure!(
        plan_of(&report(&development_tmp)?)? == sorted(&compile_caches),
        "the development build planned {:?} in /tmp",
        plan_of(&report(&development_tmp)?)?
    );

    let reclaimed = dry_run_then_reclaim(ctx, &candidate, select, Some(SRC), HOME, &expected)?;
    expect_blocker(
        &reclaimed,
        &served,
        pid,
        "cwd",
        &format!("liveness_contract node-project: pid {pid} ("),
    )?;
    dry_run_then_reclaim(
        ctx,
        &candidate,
        SELECT_NODE_COMPILE,
        None,
        "/tmp",
        &compile_caches,
    )?;
    drop(server);

    tool(ctx, &home("src/app"), NPM_ENV, NPM, &["ci"])?;
    std_venv(ctx, &home("src/pyapp"), true)?;
    run_mypy(ctx, "typed")?;
    run_pytest(ctx, "tested", &["-q"])?;
    run_node_with_cache(ctx, "/tmp")?;
    for rebuilt in [
        home("src/app/node_modules/leftpad-local/index.js"),
        home("src/pyapp/.venv/pyvenv.cfg"),
        home("src/typed/.mypy_cache/CACHEDIR.TAG"),
        home("src/tested/.pytest_cache/v/cache/nodeids"),
    ] {
        ensure!(
            rebuilt.is_file(),
            "the tool did not rebuild {}",
            rebuilt.display()
        );
    }
    Ok(())
}
