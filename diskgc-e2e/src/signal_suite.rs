//! Real-SIGTERM reclaim check in a disposable test container. A command
//! removes one file, proves it is still running, then the harness signals
//! the diskgc process itself. The oracle and surviving sibling show that
//! interruption stops the current action and prevents the next one.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::ensure_suite::report;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

pub fn sigterm_mid_reclaim_reports_partial_action_and_stops(ctx: &Ctx) -> TestResult {
    let first = ctx.scratch.mkdir("signal-first", 0o700, TESTER)?;
    let second = ctx.scratch.mkdir("signal-second", 0o700, TESTER)?;
    let first_removed = ctx
        .scratch
        .write_file("signal-first/one", b"one", 0o600, TESTER)?;
    let first_kept = ctx
        .scratch
        .write_file("signal-first/two", b"two", 0o600, TESTER)?;
    let second_one = ctx
        .scratch
        .write_file("signal-second/one", b"one", 0o600, TESTER)?;
    let second_two = ctx
        .scratch
        .write_file("signal-second/two", b"two", 0o600, TESTER)?;
    let cleaner = ctx.scratch.write_file(
        "signal-cleaner",
        b"#!/bin/sh\nset -eu\n/bin/rm -- \"$1/one\"\nprintf '%s\\n' \"$PPID\" > /tmp/signal-ready\nwhile :; do /bin/sleep 1; done\n",
        0o755,
        TESTER,
    )?;
    let config = format!(
        "version: 1\nprofiles:\n  - name: signal-command\n    find: {{roots: [/tmp], paths: [signal-first, signal-second], type: dir}}\n    action: {{command: [{}, \"{{path}}\"]}}\n    examples: {{match: [/tmp/signal-first, /tmp/signal-second]}}\n",
        cleaner.display()
    );
    ctx.home
        .write_file("diskgc-signal.yaml", config.as_bytes(), 0o644, TESTER)?;

    // The snapshot predates the traced run. Only the first candidate is
    // permitted to lose anything, so the oracle catches a second action.
    let oracle = Oracle::arm(Fixture::new().eligible(first.clone()))?;
    let spawned = ctx.spawn_as_tester(
        &ctx.diskgc(),
        &[
            "ensure",
            "--json",
            "--config",
            "/home/tester/diskgc-signal.yaml",
            "--min-free",
            "100%",
            "--timeout",
            "15s",
            "--select",
            "profile:signal-command",
            "/tmp",
        ],
    )?;
    let marker = ctx.scratch.path("signal-ready");
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        match fs::read_to_string(&marker) {
            Ok(text) if text.trim().is_empty() => {}
            Ok(text) => {
                break text.trim().parse::<i32>().map_err(|error| {
                    format!(
                        "cleaner wrote invalid diskgc PID to {}: {error}",
                        marker.display()
                    )
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("read {}: {error}", marker.display())),
        }
        ensure!(
            Instant::now() < deadline,
            "the command did not delete its first file and write its parent PID"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    ensure!(pid > 0, "invalid diskgc PID {pid}");
    ensure!(
        same_file(Path::new(&format!("/proc/{pid}/exe")), &ctx.diskgc())?,
        "command parent {pid} is not this diskgc binary"
    );
    ensure!(
        !first_removed.exists() && first_kept.exists(),
        "SIGTERM was not sent during a partially processed first candidate"
    );
    // Only the traced diskgc receives this signal. Its command child is
    // stopped by diskgc's normal process-group cancellation path.
    let sent = unsafe { libc::kill(pid, libc::SIGTERM) };
    ensure!(
        sent == 0,
        "SIGTERM to diskgc PID {pid}: {}",
        std::io::Error::last_os_error()
    );

    let run = spawned.wait()?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(143)?;
    let json = report(&run)?;
    ensure!(json["status"] == "interrupted", "SIGTERM report: {json}");
    let actions = json["actions"]
        .as_array()
        .ok_or("actions is not an array")?;
    ensure!(
        actions.len() == 1
            && actions[0]["candidate"] == first.to_string_lossy().as_ref()
            && actions[0]["count"] == 1,
        "partial command action was not reported exactly once: {json}"
    );
    ensure!(
        !first_removed.exists()
            && first_kept.exists()
            && second_one.exists()
            && second_two.exists()
            && second.exists(),
        "SIGTERM did not preserve the unfinished candidate and the next candidate"
    );
    verdict.assert_clean()
}

fn same_file(left: &Path, right: &Path) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    let left = fs::metadata(left).map_err(|error| format!("stat {}: {error}", left.display()))?;
    let right =
        fs::metadata(right).map_err(|error| format!("stat {}: {error}", right.display()))?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

/// The command's shell handles TERM but deliberately keeps running. The
/// signal handler leaves evidence; only the KILL after diskgc's grace period
/// can stop the loop. This is a Full test because it waits about ten seconds.
pub fn sigterm_stubborn_command_child_gets_killed_after_grace(ctx: &Ctx) -> TestResult {
    let first = ctx.scratch.mkdir("stubborn-first", 0o700, TESTER)?;
    let second = ctx.scratch.mkdir("stubborn-second", 0o700, TESTER)?;
    let first_removed = ctx
        .scratch
        .write_file("stubborn-first/one", b"one", 0o600, TESTER)?;
    let first_kept = ctx
        .scratch
        .write_file("stubborn-first/two", b"two", 0o600, TESTER)?;
    let second_one = ctx
        .scratch
        .write_file("stubborn-second/one", b"one", 0o600, TESTER)?;
    let second_two = ctx
        .scratch
        .write_file("stubborn-second/two", b"two", 0o600, TESTER)?;
    let cleaner = ctx.scratch.write_file(
        "stubborn-cleaner",
        b"#!/bin/sh\ntrap 'printf TERM > /tmp/stubborn-term-seen' TERM\ntrap 'printf EXIT > /tmp/stubborn-exit-seen' EXIT\n/bin/rm -- \"$1/one\"\nprintf '%s\\n' \"$PPID\" > /tmp/stubborn-ready\nwhile :; do /bin/sleep 1 || :; done\n",
        0o755,
        TESTER,
    )?;
    let config = format!(
        "version: 1\nprofiles:\n  - name: stubborn-command\n    find: {{roots: [/tmp], paths: [stubborn-first, stubborn-second], type: dir}}\n    action: {{command: [{}, \"{{path}}\"]}}\n    examples: {{match: [/tmp/stubborn-first, /tmp/stubborn-second]}}\n",
        cleaner.display()
    );
    ctx.home
        .write_file("diskgc-stubborn.yaml", config.as_bytes(), 0o644, TESTER)?;

    let oracle = Oracle::arm(Fixture::new().eligible(first.clone()))?;
    let spawned = ctx.spawn_as_tester(
        &ctx.diskgc(),
        &[
            "ensure",
            "--json",
            "--config",
            "/home/tester/diskgc-stubborn.yaml",
            "--min-free",
            "100%",
            "--timeout",
            "60s",
            "--select",
            "profile:stubborn-command",
            "/tmp",
        ],
    )?;
    let marker = ctx.scratch.path("stubborn-ready");
    let deadline = Instant::now() + Duration::from_secs(20);
    let pid = loop {
        match fs::read_to_string(&marker) {
            Ok(text) if text.trim().is_empty() => {}
            Ok(text) => {
                break text.trim().parse::<i32>().map_err(|error| {
                    format!(
                        "cleaner wrote invalid diskgc PID to {}: {error}",
                        marker.display()
                    )
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("read {}: {error}", marker.display())),
        }
        ensure!(Instant::now() < deadline, "stubborn command never started");
        std::thread::sleep(Duration::from_millis(10));
    };
    ensure!(pid > 0, "invalid diskgc PID {pid}");
    ensure!(
        same_file(Path::new(&format!("/proc/{pid}/exe")), &ctx.diskgc())?,
        "command parent {pid} is not this diskgc binary"
    );
    ensure!(
        !first_removed.exists() && first_kept.exists(),
        "stubborn command did not reach its partial deletion before SIGTERM"
    );
    let signaled_at = Instant::now();
    let sent = unsafe { libc::kill(pid, libc::SIGTERM) };
    ensure!(
        sent == 0,
        "SIGTERM to diskgc PID {pid}: {}",
        std::io::Error::last_os_error()
    );
    let run = spawned.wait()?;
    let waited = signaled_at.elapsed();
    let verdict = oracle.judge(&run)?;
    run.expect_code(143)?;
    let json = report(&run)?;
    ensure!(json["status"] == "interrupted", "SIGTERM report: {json}");
    ensure!(
        fs::read_to_string(ctx.scratch.path("stubborn-term-seen"))
            .map_err(|error| format!("TERM handler did not run: {error}"))?
            .contains("TERM"),
        "child did not observe process-group TERM"
    );
    ensure!(
        !ctx.scratch.path("stubborn-exit-seen").exists(),
        "child exited through its shell EXIT trap instead of being killed"
    );
    ensure!(
        waited >= Duration::from_secs(9) && waited < Duration::from_secs(30),
        "TERM-ignoring child ended after {waited:?}, outside the 10 s KILL grace window"
    );
    let actions = json["actions"]
        .as_array()
        .ok_or("actions is not an array")?;
    ensure!(
        actions.len() == 1 && actions[0]["candidate"] == first.to_string_lossy().as_ref(),
        "partial stubborn command action was not reported once: {json}"
    );
    ensure!(
        !first_removed.exists()
            && first_kept.exists()
            && second_one.exists()
            && second_two.exists()
            && second.exists(),
        "SIGTERM/KILL path touched the next candidate or hid a partial action"
    );
    verdict.assert_clean()
}
