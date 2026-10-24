//! A live process can change TMPDIR after exec. Linux exposes only its
//! initial environment through /proc, so an idle temporary directory with
//! no open descriptor is not recognizable as active. This container test
//! keeps the unsafe pattern as a user-configured dry-run probe and requires
//! the broad built-ins to stay out of the shipped library.

use crate::ensure_suite::{ensure_run, report};
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

/// A real pytest invocation in the disposable Nix image demonstrates the
/// supported `tmp_path_retention_policy=none` case. It creates an ordinary
/// numbered session and test directory, but no `.lock`; while the test sleeps
/// it holds no descriptor inside the test directory. A user-configured
/// deletion probe must classify that live directory as eligible, which is
/// why a plain path rule cannot ship as a built-in; the shipped
/// `pytest-tmp`, one session per candidate under the pytest contract,
/// keeps it (TASK-112 step 5).
pub fn live_pytest_without_session_lock_is_unsafe_to_reclaim(ctx: &Ctx) -> TestResult {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    struct StopOnDrop(Child);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let s = &ctx.scratch;
    s.write_file(
        "test_active.py",
        b"from pathlib import Path\nimport time\n\ndef test_active(tmp_path):\n    (tmp_path / 'payload').write_text('live')\n    Path('/tmp/pytest-ready').write_text(str(tmp_path))\n    time.sleep(60)\n    assert (tmp_path / 'payload').read_text() == 'live'\n",
        0o600,
        TESTER,
    )?;
    ctx.home.write_file(
        "pytest-unsafe-probe.yaml",
        b"version: 1\nprofiles:\n  - name: pytest-unsafe-probe\n    find: {roots: [/tmp], paths: ['pytest-of-*/pytest-*/*'], type: dir}\n    action: delete\n    examples: {match: [/tmp/pytest-of-tester/pytest-0/test_active0]}\n",
        0o600,
        TESTER,
    )?;
    let child = Command::new(&ctx.self_exe)
        .args([
            "as-tester",
            "--",
            "/opt/pytest-under-test",
            "-q",
            "-o",
            "tmp_path_retention_policy=none",
            "/tmp/test_active.py",
        ])
        .env_clear()
        .env("PATH", "/bin")
        .env("PYTEST_DISABLE_PLUGIN_AUTOLOAD", "1")
        .current_dir("/")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("start pinned pytest: {e}"))?;
    let mut pytest = StopOnDrop(child);
    let deadline = Instant::now() + Duration::from_secs(15);
    let test_dir = loop {
        if let Ok(path) = std::fs::read_to_string(s.path("pytest-ready")) {
            break path.into();
        }
        if let Some(status) = pytest
            .0
            .try_wait()
            .map_err(|e| format!("poll pinned pytest: {e}"))?
        {
            return Err(format!(
                "pinned pytest exited before active fixture: {status}"
            ));
        }
        ensure!(
            Instant::now() < deadline,
            "pinned pytest did not reach tmp_path fixture"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let test_dir: std::path::PathBuf = test_dir;
    ensure!(
        test_dir == s.path("pytest-of-tester/pytest-0/test_active0"),
        "unexpected pytest test directory: {}",
        test_dir.display()
    );
    ensure!(
        !s.path("pytest-of-tester/pytest-0/.lock").exists(),
        "retention_policy=none unexpectedly made a session lock"
    );
    let fd_dir = format!("/proc/{}/fd", pytest.0.id());
    for entry in std::fs::read_dir(&fd_dir).map_err(|e| format!("read {fd_dir}: {e}"))? {
        let entry = entry.map_err(|e| format!("read pytest fd: {e}"))?;
        let target = std::fs::read_link(entry.path())
            .map_err(|e| format!("read pytest fd {}: {e}", entry.path().display()))?;
        ensure!(
            !target.starts_with(&test_dir),
            "pytest unexpectedly holds a descriptor in {}: {}",
            test_dir.display(),
            target.display()
        );
    }
    let oracle = Oracle::arm(Fixture::new())?;
    let list = ctx.run_as_tester(&ctx.diskgc(), &["profiles", "list", "--json"])?;
    let dry = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--no-index",
            "--config",
            "/home/tester/pytest-unsafe-probe.yaml",
            "--min-free",
            "100%",
            "--select",
            "profile:pytest-unsafe-probe",
            "/tmp",
        ],
    )?;
    let verdict = oracle.judge_runs(&[&list, &dry])?;
    list.expect_code(0)?;
    dry.expect_code(0)?;
    let listed = report(&list)?;
    let names: Vec<&str> = listed["profiles"]
        .as_array()
        .ok_or("profiles list has no profiles array")?
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    // pytest-tmp ships (TASK-112 step 5) only with the pytest contract, one
    // session per candidate: the built-in keeps this live session. In this
    // container diskgc has a PID namespace of its own, so its process list
    // is not complete and the session above the rank is producer_unknown
    // (row 8); the contract's other rows are the host-PID-namespace tests'
    // (pytest_contract_suite). Its 1h floor needs the clock advanced.
    ensure!(names.contains(&"pytest-tmp"), "pytest-tmp is not shipped");
    let session = s.path("pytest-of-tester/pytest-0");
    let clock = crate::differential_suite::SandboxClockOffset::install(2 * 3600)?;
    let builtin = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:pytest-tmp",
            "/tmp",
        ],
    )?;
    drop(clock);
    oracle.judge_runs(&[&builtin])?.assert_clean()?;
    builtin.expect_code(0)?;
    let builtin_plan = report(&builtin)?;
    ensure!(
        builtin_plan["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"].as_str() == session.to_str()
                    && entry["reason"] == "producer_unknown"
            })),
        "the pytest-tmp built-in did not keep the live session: {builtin_plan}"
    );
    let plan = report(&dry)?;
    ensure!(
        plan["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"].as_str() == test_dir.to_str() && entry["reason"].is_null()
            })),
        "live no-lock pytest test dir was not eligible in unsafe probe: {plan}"
    );
    ensure!(
        test_dir.join("payload").exists(),
        "dry-run touched pytest payload"
    );
    verdict.assert_clean()
}

pub fn dormant_exported_tmpdir_is_invisible_and_broad_builtins_are_absent(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("tmp.active", 0o700, TESTER)?;
    s.write_file("tmp.active/scratch", b"live session data", 0o600, TESTER)?;
    let holder = Holder::tester(ctx, &["hold-export", "TMPDIR", "/tmp/tmp.active"], &[])?;
    s.mkdirs("claude-1000/nix-shell.active", 0o700, TESTER)?;
    s.write_file(
        "claude-1000/nix-shell.active/scratch",
        b"live shell data",
        0o600,
        TESTER,
    )?;
    let nested_holder = Holder::tester(
        ctx,
        &["hold-export", "TMPDIR", "/tmp/claude-1000/nix-shell.active"],
        &[],
    )?;
    let environ = std::fs::read(format!("/proc/{}/environ", holder.pid()))
        .map_err(|e| format!("read holder's initial environment: {e}"))?;
    ensure!(
        !environ.windows(b"TMPDIR=".len()).any(|w| w == b"TMPDIR="),
        "fixture's TMPDIR unexpectedly appeared in /proc environ"
    );

    ctx.home.write_file(
        "temp-probe.yaml",
        b"version: 1\nprofiles:\n  - name: shell-temp-probe\n    find: {roots: [/tmp], paths: ['tmp.*'], type: dir}\n    action: delete\n    examples: {match: [/tmp/tmp.active]}\n  - name: nested-shell-temp-probe\n    find: {roots: [/tmp], paths: ['claude-1000/nix-shell.*'], type: dir}\n    action: delete\n    examples: {match: [/tmp/claude-1000/nix-shell.active]}\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let list = ctx.run_as_tester(&ctx.diskgc(), &["profiles", "list", "--json"])?;
    let dry = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--no-index",
            "--config",
            "/home/tester/temp-probe.yaml",
            "--min-free",
            "100%",
            "--select",
            "profile:shell-temp-probe",
            "/tmp",
        ],
    )?;
    let nested_dry = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--no-index",
            "--config",
            "/home/tester/temp-probe.yaml",
            "--min-free",
            "100%",
            "--select",
            "profile:nested-shell-temp-probe",
            "/tmp",
        ],
    )?;
    let verdict = oracle.judge_runs(&[&list, &dry, &nested_dry])?;
    list.expect_code(0)?;
    dry.expect_code(0)?;
    nested_dry.expect_code(0)?;
    let plan = report(&dry)?;
    ensure!(
        plan["plan"].as_array().is_some_and(|entries| entries
            .iter()
            .any(|entry| { entry["candidate"] == "/tmp/tmp.active" && entry["reason"].is_null() })),
        "post-exec TMPDIR should expose the known false negative: {plan}"
    );
    let nested_plan = report(&nested_dry)?;
    ensure!(
        nested_plan["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"] == "/tmp/claude-1000/nix-shell.active"
                    && entry["reason"].is_null()
            })),
        "Claude's nested shell TMPDIR should expose the same false negative: {nested_plan}"
    );
    let listed = report(&list)?;
    let names: Vec<&str> = listed["profiles"]
        .as_array()
        .ok_or("profiles list has no profiles array")?
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    for unsafe_name in [
        "mktemp-dir",
        "nix-shell-tmpdir",
        "tmp-entry",
        // Withdrawn (Mark, 2026-10-23, TASK-112 D2); TASK-125 re-admits.
        "containers-image-tmp",
    ] {
        ensure!(
            !names.contains(&unsafe_name),
            "unsafe broad temp profile {unsafe_name} is shipped"
        );
    }
    ensure!(
        s.path("tmp.active/scratch").exists(),
        "dry-run touched the live scratch file"
    );
    ensure!(
        s.path("claude-1000/nix-shell.active/scratch").exists(),
        "dry-run touched the live nested shell"
    );
    holder.finish()?;
    nested_holder.finish()?;
    verdict.assert_clean()
}

/// The shipped exact cache profile reaches an entry in Claude's nested
/// TMPDIR while leaving the enclosing shell and neighboring data intact.
pub fn nested_claude_shell_cache_reclaims_only_its_exact_entry(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs(
        "claude-1000/nix-shell.Ab1/node-compile-cache",
        0o700,
        TESTER,
    )?;
    s.write_file(
        "claude-1000/nix-shell.Ab1/node-compile-cache/payload",
        &[0x71; 8192],
        0o600,
        TESTER,
    )?;
    s.write_file(
        "claude-1000/nix-shell.Ab1/important",
        b"keep this shell data",
        0o600,
        TESTER,
    )?;
    let candidate = s.path("claude-1000/nix-shell.Ab1/node-compile-cache");
    let parent = s.path("claude-1000/nix-shell.Ab1");
    let protected = s.path("claude-1000/nix-shell.Ab1/important");
    let oracle = Oracle::arm(Fixture::new().eligible(candidate.clone()))?;
    let args = [
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        "profile:node-compile-cache",
        "/tmp",
    ];
    let mut dry_args = vec!["--dry-run"];
    dry_args.extend(args);
    let dry = ensure_run(ctx, &dry_args)?;
    let run = ensure_run(ctx, &args)?;
    let verdict = oracle.judge_runs(&[&dry, &run])?;
    dry.expect_code(0)?;
    run.expect_code(1)?;
    let plan = report(&dry)?;
    ensure!(
        plan["plan"]
            .as_array()
            .is_some_and(|entries| entries.len() == 1
                && entries[0]["candidate"].as_str() == candidate.to_str()
                && entries[0]["reason"].is_null()),
        "nested cache plan selected the wrong entry: {plan}"
    );
    let result = report(&run)?;
    ensure!(
        result["actions"]
            .as_array()
            .is_some_and(|actions| actions.len() == 1
                && actions[0]["candidate"].as_str() == candidate.to_str()),
        "nested cache reclaim acted on the wrong entry: {result}"
    );
    ensure!(!candidate.exists(), "exact nested cache survived reclaim");
    ensure!(
        parent.is_dir() && protected.exists(),
        "enclosing shell changed"
    );
    verdict.assert_clean()
}
