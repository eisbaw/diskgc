//! The pytest contract in the kernel's view (TASK-112 step 5;
//! docs/safety.md, Producer liveness, pytest sessions): the `pytest-tmp`
//! built-in judged by the real binary while the pinned pytest (and
//! pytest-xdist) runs in the same container.
//!
//! These containers share the host's PID namespace, as the Owner PID tests
//! do ([`crate::suite::HOST_PID_NAMESPACE_TESTS`]): in a PID namespace of
//! its own diskgc's process list is not complete, so the contract keeps
//! every session above the rank as `producer_unknown` (row 8) and nothing
//! here could be reclaimed. The harness signals only processes it started.
//!
//! Every assertion of a kept session names the deciding row of the pure
//! verdict (the report's detail starts with it), not only the outcome, and
//! every run is judged by the deletion oracle.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::differential_suite::SandboxClockOffset;
use crate::ensure_suite::{ensure_run, report};
use crate::oracle::{Fixture, Oracle};
use crate::owner_pid_suite::{
    KillOnDrop, below_the_goal, expect_action, expect_plan, expect_skipped,
    require_shifted_uid_map, wait_for,
};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{TESTER, ensure, info};

/// The pinned pytest (`flake.nix`, the Nix image).
const PYTEST: &str = "/opt/pytest-under-test";
/// The same pytest with pytest-xdist and execnet in its interpreter.
const PYTEST_XDIST: &str = "/opt/pytest-xdist-under-test";
/// The built-in's `min_idle_for` is 1h and every session here is made now:
/// the sandboxed binary's clock is advanced past it (kernel ctimes cannot
/// be set back). It stays below pytest's 3-day dead-lock age, so no
/// session here is "old" (row 7's condition while a pytest runs).
const CLOCK_OFFSET_SECONDS: u64 = 2 * 3600;
/// Bound for a pytest to reach its sleeping test.
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the test bodies sleep: far longer than the checks take.
const TEST_SLEEP_SECONDS: u32 = 300;
/// execnet 2.1.2 (`WorkerGateway._terminate_execution`) gives a worker
/// whose controller went away 5 s, then SIGINTs it, then 10 s more before
/// `os._exit(1)`. The worker test ignores SIGINT, so it outlives its
/// controller by at most about this long; the checks must fit inside.
const ORPHAN_WORKER_WINDOW: Duration = Duration::from_secs(15);
const SELECT: &str = "profile:pytest-tmp";

/// The detail a verdict of `row` starts with (crate diskgc-core,
/// fs_kind::liveness).
fn row(number: u8, rule: &str) -> String {
    format!("liveness_contract pytest row {number} ({rule}): ")
}

/// A test module whose one test records `<pid> <tmp_path>` in `ready` and
/// sleeps, holding nothing in its temp directory. With `ignore_sigint`, it
/// ignores SIGINT (an orphan xdist worker's first termination step).
fn sleeping_test(ready: &Path, ignore_sigint: bool) -> String {
    format!(
        "import os, signal, time\n\
         def test_sleeps(tmp_path):\n    \
             {}(tmp_path / 'payload').write_text('live')\n    \
             with open({:?}, 'w') as f:\n        \
                 f.write(f'{{os.getpid()}} {{tmp_path}}')\n    \
             time.sleep({TEST_SLEEP_SECONDS})\n",
        if ignore_sigint {
            "signal.signal(signal.SIGINT, signal.SIG_IGN)\n    "
        } else {
            ""
        },
        ready.display().to_string(),
    )
}

/// Starts the pinned pytest as tester (`as-tester` execs it: the child's pid
/// is pytest's), its environment cleared but for `env`, in `/`.
fn start_pytest(
    ctx: &Ctx,
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<KillOnDrop, String> {
    let child = Command::new(&ctx.self_exe)
        .args(["as-tester", "--", program])
        .args(args)
        .env_clear()
        .env("PATH", "/bin")
        .env("HOME", "/home/tester")
        .env("PYTEST_DISABLE_PLUGIN_AUTOLOAD", "1")
        .envs(env.iter().copied())
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("start {program}: {e}"))?;
    Ok(KillOnDrop(child))
}

/// Waits for the sleeping test's `ready` file: its process's pid and its
/// `tmp_path`.
fn wait_ready(ready: &Path, pytest: &mut KillOnDrop) -> Result<(u32, PathBuf), String> {
    wait_for(FIXTURE_TIMEOUT, "the sleeping test to start", || {
        if let Some(status) = pytest.0.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("pytest exited before its test slept: {status}"));
        }
        let Ok(text) = std::fs::read_to_string(ready) else {
            return Ok(None);
        };
        let Some((pid, path)) = text.split_once(' ') else {
            return Ok(None);
        };
        Ok(pid.parse().ok().map(|pid| (pid, PathBuf::from(path))))
    })
}

/// No fd, working directory or root of `pid` is inside `tree`: the
/// verdict is the contract's, not In use's.
fn holds_nothing_in(pid: u32, tree: &Path) -> TestResult {
    let mut links = vec![PathBuf::from(format!("/proc/{pid}/cwd"))];
    links.push(PathBuf::from(format!("/proc/{pid}/root")));
    let fd_dir = format!("/proc/{pid}/fd");
    for entry in std::fs::read_dir(&fd_dir).map_err(|e| format!("list {fd_dir}: {e}"))? {
        links.push(entry.map_err(|e| format!("list {fd_dir}: {e}"))?.path());
    }
    for link in links {
        // An fd closed meanwhile is gone: nothing held.
        if let Ok(target) = std::fs::read_link(&link) {
            ensure!(
                !target.starts_with(tree),
                "process {pid} holds {} in {} ({})",
                target.display(),
                tree.display(),
                link.display()
            );
        }
    }
    Ok(())
}

/// Whether `pid` is a live process: listed and not a zombie (an orphan
/// that ended may wait for its new parent to reap it). The pid is one this
/// test's own pytest made, read while the test still waits on it.
fn is_live(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z'))
    })
}

fn dry_run_args() -> Vec<&'static str> {
    let mut args = vec!["--dry-run"];
    args.extend(reclaim_args());
    args
}

fn reclaim_args() -> Vec<&'static str> {
    vec![
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        SELECT,
        "/tmp",
    ]
}

/// The skipped entry of `candidate` has a detail starting with `prefix`.
fn expect_detail_prefix(run: &Run, candidate: &Path, prefix: &str) -> TestResult {
    let report = report(run)?;
    let detail = report["skipped"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["candidate"].as_str() == candidate.to_str())
        })
        .and_then(|entry| entry["detail"].as_str())
        .ok_or_else(|| format!("{} has no skip detail: {report}", candidate.display()))?;
    ensure!(
        detail.starts_with(prefix),
        "{} detail {detail:?} does not start with {prefix:?}",
        candidate.display()
    );
    Ok(())
}

/// A live `tmp_path_retention_policy=none` session (no `.lock`, nothing
/// held in it: the TASK-65 counterexample) whose temp root is a shell's
/// `TMPDIR`: the pytest names that root in its initial environment, so the
/// session is kept by row 10 (`temp_root`, the pytest's pid). Killed, the
/// pytest leaves the session (policy none deletes only at a normal exit),
/// no process can use the root any more, and the session is reclaimed by
/// row 11, judged by the deletion oracle.
pub fn pytest_live_policy_none_session_in_shell_root_is_kept(ctx: &Ctx) -> TestResult {
    require_shifted_uid_map()?;
    below_the_goal(ctx)?;
    let s = &ctx.scratch;
    let root = s.path("nix-shell.e2eLive");
    s.mkdir("nix-shell.e2eLive", 0o700, TESTER)?;
    ctx.home.mkdir("pt", 0o700, TESTER)?;
    let ready = PathBuf::from("/home/tester/pt/ready");
    ctx.home.write_file(
        "pt/test_live.py",
        sleeping_test(&ready, false).as_bytes(),
        0o600,
        TESTER,
    )?;
    let mut pytest = start_pytest(
        ctx,
        PYTEST,
        &[
            "-q",
            "-p",
            "no:cacheprovider",
            "-o",
            "tmp_path_retention_policy=none",
            "/home/tester/pt/test_live.py",
        ],
        &[("TMPDIR", root.to_str().expect("UTF-8 scratch path"))],
    )?;
    let (pid, tmp_path) = wait_ready(&ready, &mut pytest)?;
    ensure!(
        pid == pytest.0.id(),
        "the test runs in pid {pid}, not the started pytest {}",
        pytest.0.id()
    );
    let session = root.join("pytest-of-tester/pytest-0");
    ensure!(
        tmp_path.starts_with(&session),
        "tmp_path {} is not in {}",
        tmp_path.display(),
        session.display()
    );
    ensure!(
        !session.join(".lock").exists(),
        "retention policy none made a session lock"
    );
    holds_nothing_in(pid, &session)?;

    let clock = SandboxClockOffset::install(CLOCK_OFFSET_SECONDS)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let live_plan = ensure_run(ctx, &dry_run_args())?;
    let live_run = ensure_run(ctx, &reclaim_args())?;
    oracle
        .judge_runs(&[&live_plan, &live_run])?
        .assert_clean()?;
    live_plan.expect_code(0)?;
    live_run.expect_code(1)?;
    expect_plan(&live_plan, &[(&session, Some("in_use"))])?;
    expect_skipped(&live_run, &session, "in_use", &[(pid, "temp_root")], None)?;
    expect_detail_prefix(&live_run, &session, &row(10, "unused root"))?;
    ensure!(
        tmp_path.join("payload").exists(),
        "the live session's payload is gone"
    );

    pytest
        .0
        .kill()
        .map_err(|e| format!("kill pytest {pid}: {e}"))?;
    let status = pytest.0.wait().map_err(|e| format!("reap pytest: {e}"))?;
    info!("killed pytest: {status}");
    ensure!(session.exists(), "the killed pytest removed its session");
    let oracle = Oracle::arm(Fixture::new().eligible(&session))?;
    let dead_run = ensure_run(ctx, &reclaim_args())?;
    oracle.judge_runs(&[&dead_run])?.assert_clean()?;
    dead_run.expect_code(1)?;
    expect_action(&dead_run, &session)?;
    ensure!(!session.exists(), "the dead session was not reclaimed");
    drop(clock);
    Ok(())
}

/// A sleeping run whose `--basetemp` is shaped like a session (no number of
/// pytest's own, no `.lock`) in a dead shell's root, which no process uses:
/// the pytest names it on its command line, so it is kept by row 6
/// (`command_line`). A leftover session below the rank in the same
/// directory is reclaimed meanwhile (the root is unused: row 11), and once
/// the run is killed the `--basetemp` session goes too.
pub fn pytest_live_basetemp_session_is_kept(ctx: &Ctx) -> TestResult {
    require_shifted_uid_map()?;
    below_the_goal(ctx)?;
    let s = &ctx.scratch;
    let root = s.path("nix-shell.e2eBase");
    s.mkdirs("nix-shell.e2eBase/pytest-of-tester/pytest-1", 0o700, TESTER)?;
    let leftover = root.join("pytest-of-tester/pytest-1");
    let basetemp = root.join("pytest-of-tester/pytest-7");
    ctx.home.mkdir("pt", 0o700, TESTER)?;
    let ready = PathBuf::from("/home/tester/pt/ready");
    ctx.home.write_file(
        "pt/test_base.py",
        sleeping_test(&ready, false).as_bytes(),
        0o600,
        TESTER,
    )?;
    let basetemp_arg = format!("--basetemp={}", basetemp.display());
    let mut pytest = start_pytest(
        ctx,
        PYTEST,
        &[
            "-q",
            "-p",
            "no:cacheprovider",
            &basetemp_arg,
            "/home/tester/pt/test_base.py",
        ],
        &[],
    )?;
    let (pid, tmp_path) = wait_ready(&ready, &mut pytest)?;
    ensure!(
        pid == pytest.0.id(),
        "the test runs in pid {pid}, not the started pytest {}",
        pytest.0.id()
    );
    ensure!(
        tmp_path.starts_with(&basetemp),
        "tmp_path {} is not below --basetemp {}",
        tmp_path.display(),
        basetemp.display()
    );
    ensure!(
        !basetemp.join(".lock").exists(),
        "--basetemp made a session lock"
    );
    holds_nothing_in(pid, &basetemp)?;

    let clock = SandboxClockOffset::install(CLOCK_OFFSET_SECONDS)?;
    let oracle = Oracle::arm(Fixture::new().eligible(&leftover))?;
    let live_plan = ensure_run(ctx, &dry_run_args())?;
    let live_run = ensure_run(ctx, &reclaim_args())?;
    oracle
        .judge_runs(&[&live_plan, &live_run])?
        .assert_clean()?;
    live_plan.expect_code(0)?;
    live_run.expect_code(1)?;
    expect_plan(
        &live_plan,
        &[(&basetemp, Some("in_use")), (&leftover, None)],
    )?;
    expect_skipped(
        &live_run,
        &basetemp,
        "in_use",
        &[(pid, "command_line")],
        None,
    )?;
    expect_detail_prefix(&live_run, &basetemp, &row(6, "named"))?;
    expect_action(&live_run, &leftover)?;
    ensure!(!leftover.exists(), "the leftover session was not reclaimed");
    ensure!(
        tmp_path.join("payload").exists(),
        "the live --basetemp session's payload is gone"
    );

    pytest
        .0
        .kill()
        .map_err(|e| format!("kill pytest {pid}: {e}"))?;
    let status = pytest.0.wait().map_err(|e| format!("reap pytest: {e}"))?;
    info!("killed pytest: {status}");
    let oracle = Oracle::arm(Fixture::new().eligible(&basetemp))?;
    let dead_run = ensure_run(ctx, &reclaim_args())?;
    oracle.judge_runs(&[&dead_run])?.assert_clean()?;
    dead_run.expect_code(1)?;
    expect_action(&dead_run, &basetemp)?;
    ensure!(
        !basetemp.exists(),
        "the dead --basetemp session was not reclaimed"
    );
    drop(clock);
    Ok(())
}

/// pytest-xdist: the controller (whose pid the session's `.lock` holds) is
/// killed while its one worker sleeps in a test. The worker is a real
/// execnet worker: its command line is the bootstrap and its initial
/// environment holds no `PYTEST_XDIST_*` variable (xdist sets them inside
/// the process), as docs/safety.md says. While it lives, the lock's owner
/// has ended (rows 3 to 5 pass), the session is above the rank, and the
/// worker's inherited `TMPDIR` is the session's root: row 10, `temp_root`,
/// the worker's pid. An ended lock owner alone never permits.
///
/// execnet bounds the worker's life after its controller is gone
/// ([`ORPHAN_WORKER_WINDOW`]); the test checks that the worker still
/// lived after diskgc's runs, so the verdict was about a live orphan.
/// Once it is gone the session is reclaimed (row 11), judged by the
/// deletion oracle.
pub fn pytest_orphan_xdist_worker_keeps_session(ctx: &Ctx) -> TestResult {
    require_shifted_uid_map()?;
    below_the_goal(ctx)?;
    let s = &ctx.scratch;
    let root = s.path("nix-shell.e2eXdist");
    s.mkdir("nix-shell.e2eXdist", 0o700, TESTER)?;
    ctx.home.mkdir("pt", 0o700, TESTER)?;
    let ready = PathBuf::from("/home/tester/pt/ready");
    ctx.home.write_file(
        "pt/test_worker.py",
        sleeping_test(&ready, true).as_bytes(),
        0o600,
        TESTER,
    )?;
    let mut controller = start_pytest(
        ctx,
        PYTEST_XDIST,
        &[
            "-q",
            "-p",
            "no:cacheprovider",
            "-p",
            "xdist.plugin",
            "-n",
            "1",
            "/home/tester/pt/test_worker.py",
        ],
        &[("TMPDIR", root.to_str().expect("UTF-8 scratch path"))],
    )?;
    let controller_pid = controller.0.id();
    let (worker, tmp_path) = wait_ready(&ready, &mut controller)?;
    ensure!(
        worker != controller_pid,
        "the test ran in the controller, not in an xdist worker"
    );
    let session = root.join("pytest-of-tester/pytest-0");
    ensure!(
        tmp_path.starts_with(&session),
        "the worker's tmp_path {} is not in {}",
        tmp_path.display(),
        session.display()
    );
    let lock = std::fs::read_to_string(session.join(".lock"))
        .map_err(|e| format!("read the session's .lock: {e}"))?;
    ensure!(
        lock == controller_pid.to_string(),
        "the .lock holds {lock:?}, not the controller's pid {controller_pid}"
    );
    let cmdline = std::fs::read(format!("/proc/{worker}/cmdline"))
        .map_err(|e| format!("read the worker's command line: {e}"))?;
    let args: Vec<&[u8]> = cmdline
        .strip_suffix(b"\0")
        .unwrap_or(&cmdline)
        .split(|b| *b == 0)
        .collect();
    ensure!(
        args.windows(2)
            .any(|w| w[0] == b"-c" && w[1] == b"import sys;exec(eval(sys.stdin.readline()))"),
        "the worker's command line is not the execnet bootstrap: {:?}",
        String::from_utf8_lossy(&cmdline)
    );
    let environ = std::fs::read(format!("/proc/{worker}/environ"))
        .map_err(|e| format!("read the worker's environment: {e}"))?;
    ensure!(
        !environ
            .split(|b| *b == 0)
            .any(|var| var.starts_with(b"PYTEST_XDIST_")),
        "the worker's initial environment holds a PYTEST_XDIST_ variable"
    );
    holds_nothing_in(worker, &session)?;

    let clock = SandboxClockOffset::install(CLOCK_OFFSET_SECONDS)?;
    controller
        .0
        .kill()
        .map_err(|e| format!("kill the controller {controller_pid}: {e}"))?;
    let killed_at = Instant::now();
    controller
        .0
        .wait()
        .map_err(|e| format!("reap the controller: {e}"))?;
    let oracle = Oracle::arm(Fixture::new())?;
    let live_plan = ensure_run(ctx, &dry_run_args())?;
    let live_run = ensure_run(ctx, &reclaim_args())?;
    let checked_within = killed_at.elapsed();
    ensure!(
        is_live(worker),
        "the orphan worker {worker} was gone by the end of the checks \
         ({checked_within:?} after its controller was killed): the verdict was not about a \
         live orphan"
    );
    info!(
        "orphan worker {worker} alive {checked_within:?} after its controller was killed \
         (execnet window {ORPHAN_WORKER_WINDOW:?})"
    );
    oracle
        .judge_runs(&[&live_plan, &live_run])?
        .assert_clean()?;
    live_plan.expect_code(0)?;
    live_run.expect_code(1)?;
    expect_plan(&live_plan, &[(&session, Some("in_use"))])?;
    expect_skipped(
        &live_run,
        &session,
        "in_use",
        &[(worker, "temp_root")],
        None,
    )?;
    expect_detail_prefix(&live_run, &session, &row(10, "unused root"))?;

    // execnet ends the orphan on its own: wait for that (bounded), then the
    // session has no user left.
    let gone = wait_for(
        ORPHAN_WORKER_WINDOW + Duration::from_secs(30),
        "the orphan worker to end",
        || Ok((!is_live(worker)).then_some(())),
    );
    info!(
        "orphan worker {worker} {} {:?} after its controller was killed",
        if gone.is_ok() { "ended" } else { "still alive" },
        killed_at.elapsed()
    );
    gone?;
    ensure!(session.exists(), "the orphan worker removed the session");
    let oracle = Oracle::arm(Fixture::new().eligible(&session))?;
    let dead_run = ensure_run(ctx, &reclaim_args())?;
    oracle.judge_runs(&[&dead_run])?.assert_clean()?;
    dead_run.expect_code(1)?;
    expect_action(&dead_run, &session)?;
    ensure!(!session.exists(), "the dead session was not reclaimed");
    drop(clock);
    Ok(())
}
