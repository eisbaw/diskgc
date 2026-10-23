//! Producer lifecycle evidence (TASK-112 step 1). These tests run the
//! pinned producers, never diskgc: they pin the facts the liveness
//! contracts of docs/safety.md (Producer liveness) rest on, so a bump of
//! the pinned Nix or pytest that changes a lifecycle fails here before any
//! contract can act on the new behaviour.
//!
//! Each scenario is a shell script run as tester that prints
//! `FACT key=value` lines; the test requires exactly the expected set, so
//! a fact that changes, disappears or appears is a failure.

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::suite::{Ctx, TestResult};
use crate::under_test::TESTER_ENV;
use crate::{ensure, info};

const NIX_SHELL_RC_SCRIPT: &str = include_str!("lifecycle/nix_shell_rc.sh");
const PYTEST_SCRIPT: &str = include_str!("lifecycle/pytest.sh");
/// The pinned interactive shell for both Nix commands (flake.nix bakes the
/// link; `nix develop` would otherwise fetch `bashInteractive`).
const NIX_SHELL_BASH: &str = "/opt/nix-eval/bash";
/// Generous for about a dozen shell starts and pytest runs; a hung
/// producer must fail the test, not the suite's budget.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(180);

/// What docs/safety.md (nix-shell rc directories) relies on, observed:
/// the pid in the name is the shell's own for both commands, interactive
/// and `-c`; `nix develop`'s directory is empty after start, referenced by
/// no shell or child, and left after every exit; legacy `nix-shell`'s
/// directory holds `rc`, is the `TMPDIR` of the shell and of every child
/// (so a surviving child names it in its initial environment), and its rc
/// trap removes it at a normal exit or `SIGHUP`, even while a `setsid`
/// child still lives: only `SIGKILL` leaves it.
const NIX_SHELL_RC_FACTS: &[(&str, &str)] = &[
    ("nix_version", "2.34.9"),
    ("legacy.c.pid_equal", "yes"),
    ("legacy.c.contents", "rc,"),
    ("legacy.c.after_exit", "removed"),
    ("legacy.i.pid_equal", "yes"),
    ("legacy.i.contents", "rc,"),
    ("legacy.i.shell_refs", "environ"),
    ("legacy.i.child_refs", "environ"),
    ("legacy.i.child_tmpdir_is_rcdir", "yes"),
    ("legacy.i.shell_tmpdir_is_rcdir", "yes"),
    ("legacy.i.after_exit", "removed"),
    ("legacy.i.child_alive_after_exit", "yes"),
    ("legacy.KILL.after", "kept:rc,"),
    ("legacy.HUP.after", "removed"),
    ("develop.c.pid_equal", "yes"),
    ("develop.c.contents", ""),
    ("develop.c.after_exit", "kept"),
    ("develop.i.pid_equal", "yes"),
    ("develop.i.contents", ""),
    ("develop.i.shell_refs", "none"),
    ("develop.i.child_refs", "none"),
    ("develop.i.child_tmpdir_is_rcdir", "no"),
    ("develop.i.shell_tmpdir_is_rcdir", "no"),
    ("develop.i.after_exit", "kept"),
    ("develop.i.child_alive_after_exit", "yes"),
    ("develop.KILL.after", "kept:"),
    ("develop.HUP.after", "kept:"),
    ("done", "yes"),
];

/// What docs/safety.md (pytest sessions) relies on, observed with the
/// pinned pytest: the lock holds the pid as text without a newline and is
/// removed at exit; exit cleanup keeps the newest three numbers; a killed
/// session's lock below the rank is kept while young and removed once
/// older than three days; retention policy `none` makes no lock and its
/// exit deletes a concurrent lockless live session; `--basetemp` makes no
/// lock or number, and a later session cleans such a directory shaped like
/// a session with `N <= M - 3`.
const PYTEST_FACTS: &[(&str, &str)] = &[
    ("pytest_version", "9.1.1"),
    ("default.lock_is_pid_text", "yes"),
    ("default.lock_after_exit", "absent"),
    (
        "default.sessions_after_5",
        "pytest-2,pytest-3,pytest-4,pytest-current,",
    ),
    ("killed.lock_left", "yes"),
    ("killed.young_lock_below_rank", "kept"),
    ("killed.old_lock_below_rank", "removed"),
    ("keep0.lock", "none"),
    ("keep0.after", ""),
    ("keep0.live_session_survived", "no"),
    ("keep0.live_still_running", "yes"),
    (
        "basetemp.before",
        "pytest-1,pytest-2,pytest-3,pytest-current,",
    ),
    ("basetemp.lock", "none"),
    (
        "basetemp.after_run",
        "pytest-0,pytest-1,pytest-2,pytest-3,pytest-current,",
    ),
    (
        "basetemp.after_next_session",
        "pytest-2,pytest-3,pytest-4,pytest-current,",
    ),
    ("done", "yes"),
];

pub fn nix_shell_rc_lifecycle_is_pinned(ctx: &Ctx) -> TestResult {
    let facts = run_scenario(ctx, NIX_SHELL_RC_SCRIPT, &[NIX_SHELL_BASH])?;
    require_facts("nix-shell rc", &facts, NIX_SHELL_RC_FACTS)
}

pub fn pytest_lifecycle_is_pinned(ctx: &Ctx) -> TestResult {
    let facts = run_scenario(ctx, PYTEST_SCRIPT, &[])?;
    require_facts("pytest", &facts, PYTEST_FACTS)
}

/// Runs `script` as tester under `/bin/sh` with `args`, bounded by
/// [`SCENARIO_TIMEOUT`], and returns its `FACT` lines. A repeated key is an
/// error: a scenario states each fact once.
fn run_scenario(
    ctx: &Ctx,
    script: &str,
    args: &[&str],
) -> Result<BTreeMap<String, String>, String> {
    let mut child = Command::new(&ctx.self_exe)
        .args(["as-tester", "--", "/bin/sh", "-c", script, "scenario"])
        .args(args)
        .env_clear()
        .envs(TESTER_ENV.iter().copied())
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start lifecycle scenario: {e}"))?;
    // Drain both pipes while the scenario runs, so a chatty producer cannot
    // block on a full pipe.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout_reader = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr_reader = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("poll lifecycle scenario: {e}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "lifecycle scenario did not finish within {SCENARIO_TIMEOUT:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "scenario stdout reader panicked".to_owned())?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "scenario stderr reader panicked".to_owned())?;
    info!("lifecycle scenario stdout:\n{stdout}");
    ensure!(
        status.success(),
        "lifecycle scenario exited {status}; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let mut facts = BTreeMap::new();
    for line in stdout.lines() {
        let Some(fact) = line.strip_prefix("FACT ") else {
            continue;
        };
        let (key, value) = fact
            .split_once('=')
            .ok_or_else(|| format!("malformed fact line {line:?}"))?;
        if facts.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(format!("fact {key} stated twice; stdout:\n{stdout}"));
        }
    }
    Ok(facts)
}

fn require_facts(
    producer: &str,
    observed: &BTreeMap<String, String>,
    expected: &[(&str, &str)],
) -> TestResult {
    let expected: BTreeMap<String, String> = expected
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    ensure!(
        *observed == expected,
        "{producer} lifecycle differs from the design (docs/safety.md, Producer liveness):\nobserved {observed:#?}\nexpected {expected:#?}"
    );
    Ok(())
}
