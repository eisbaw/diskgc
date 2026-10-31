//! Container tests of `diskgc ensure` (task-10; docs/reference.md,
//! `ensure`, Exit codes, Output): the real binary, run as `tester` (or, for
//! the root refusal, as container root) under strace, judged by the
//! deletion oracle, whose eligible set the harness computes from the tree,
//! never from diskgc's output. Space is measured by the harness itself
//! (`statvfs` of the scratch tmpfs), so a goal the run must reach is set
//! from the harness's own numbers.
//!
//! The Cargo tests run once per Cargo toolchain under test
//! ([`crate::cargo_suite::CargoTest`]) on targets real builds made.

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;

use crate::cargo_harness::{deletable, lock_inodes, objects_below, profile_dirs, reclaimable};
use crate::cargo_under_test::{Cargo, SMOKE, Toolchain};
use crate::holder::Holder;
use crate::lock_release::{flock_free_within_release_bound, flock_held};
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, HOST_LOCK, TestResult};
use crate::under_test::Run;
use crate::{Owner, ROOT, TESTER, ensure, info};

/// Available bytes of the filesystem holding `path`, as an unprivileged
/// writer may use them (`f_bavail × f_frsize`): the harness's own measure.
pub(crate) fn avail(path: &Path) -> Result<u64, String> {
    let s = rustix::fs::statvfs(path).map_err(|e| format!("statvfs {path:?}: {e}"))?;
    Ok(s.f_bavail * s.f_frsize)
}

/// The allocated bytes of every object below the target's profile
/// directories' entries, each inode once (Cargo hard-links binaries from
/// `deps/`): what reclaiming the target frees on tmpfs.
pub(crate) fn freed_by(target: &Path) -> Result<u64, String> {
    let mut seen = BTreeSet::new();
    let mut bytes = 0;
    for profile in profile_dirs(target)? {
        for entry in deletable(&profile)? {
            for object in objects_below(&entry)? {
                let meta = std::fs::symlink_metadata(&object)
                    .map_err(|e| format!("lstat {object:?}: {e}"))?;
                if seen.insert(meta.ino()) {
                    bytes += meta.blocks() * 512;
                }
            }
        }
    }
    Ok(bytes)
}

/// `diskgc ensure ARGS` as tester under strace.
pub(crate) fn ensure_run(ctx: &Ctx, args: &[&str]) -> Result<Run, String> {
    let mut full = vec!["ensure"];
    full.extend_from_slice(args);
    let run = ctx.run_as_tester(&ctx.diskgc(), &full)?;
    info!(
        "diskgc {} -> {:?}\n--- stdout\n{}--- stderr\n{}",
        full.join(" "),
        run.code,
        run.stdout,
        run.stderr
    );
    Ok(run)
}

/// The real CLI binds protection independently of tier roots and applies
/// raw find matches to a dry-run plan. This test only plans; it never arms
/// the deletion oracle or invokes a reclaiming ensure.
pub fn configured_protect_marks_overlapping_fs_candidates_in_dry_run(ctx: &Ctx) -> TestResult {
    let scratch = &ctx.scratch;
    scratch.mkdir("protect-parent", 0o755, TESTER)?;
    scratch.mkdir("protect-parent/inside", 0o755, TESTER)?;
    scratch.mkdir("protect-exact", 0o755, TESTER)?;
    scratch.mkdir("protect-other", 0o755, TESTER)?;
    ctx.home.write_file(
        "diskgc-protect.yaml",
        b"version: 1\nprofiles:\n  - name: reclaim-top\n    find: {roots: [/tmp], paths: [protect-parent, protect-exact, protect-other], type: dir}\n    action: delete\n    examples: {match: [/tmp/protect-parent, /tmp/protect-exact, /tmp/protect-other]}\n  - name: guard-inside\n    find: {roots: [/tmp], paths: [protect-parent/inside, protect-exact], type: dir}\n    where: {has: absent-marker}\n    action: delete\n    examples: {match: [/tmp/protect-parent/inside, /tmp/protect-exact]}\nprotect: [guard-inside]\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--config",
            "/home/tester/diskgc-protect.yaml",
            "--select",
            "profile:reclaim-top",
            "--min-free",
            "100%",
            "/tmp",
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    let result = report(&run)?;
    let plan = result["plan"].as_array().ok_or("plan is not an array")?;
    for (path, reason) in [
        (scratch.path("protect-parent"), Some("protected")),
        (scratch.path("protect-exact"), Some("protected")),
        (scratch.path("protect-other"), None),
    ] {
        let entry = plan
            .iter()
            .find(|entry| entry["candidate"] == path.to_string_lossy().as_ref())
            .ok_or_else(|| format!("missing candidate {}: {result}", path.display()))?;
        ensure!(
            entry["reason"].as_str() == reason,
            "{}: expected {reason:?}, got {entry}",
            path.display()
        );
    }
    judged.assert_clean()
}

/// The configured CLI path must execute both groups in policy order, with
/// each candidate deletion checked by the independent filesystem oracle.
pub fn configured_groups_reclaim_in_order(ctx: &Ctx) -> TestResult {
    let scratch = &ctx.scratch;
    scratch.mkdir("group-stale", 0o755, TESTER)?;
    scratch.mkdir("group-pressure", 0o755, TESTER)?;
    scratch.write_file("group-stale/payload", &[7; 8192], 0o644, TESTER)?;
    scratch.write_file("group-pressure/payload", &[9; 8192], 0o644, TESTER)?;
    let stale = scratch.path("group-stale");
    let pressure = scratch.path("group-pressure");
    ctx.home.write_file(
        "diskgc-groups.yaml",
        b"version: 1\nprofiles:\n  - name: group-stale-source\n    find: {roots: [/tmp], paths: [group-stale], type: dir}\n    action: delete\n    examples: {match: [/tmp/group-stale]}\n  - name: group-pressure-source\n    find: {roots: [/tmp], paths: [group-pressure], type: dir}\n    action: delete\n    examples: {match: [/tmp/group-pressure]}\ngroups:\n  - name: stale\n    tiers:\n      - name: stale-tier\n        select: profile:group-stale-source\n  - name: pressure\n    tiers:\n      - name: pressure-tier\n        select: profile:group-pressure-source\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(
        Fixture::new()
            .eligible(stale.clone())
            .eligible(pressure.clone()),
    )?;
    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/diskgc-groups.yaml",
            "--min-free",
            "100%",
            "--timeout",
            "10s",
            "/tmp",
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    let result = report(&run)?;
    let actions = result["actions"]
        .as_array()
        .ok_or("actions is not an array")?;
    ensure!(
        actions.len() == 2,
        "expected both configured tiers: {result}"
    );
    ensure!(
        actions[0]["tier"] == "stale-tier"
            && actions[0]["candidate"] == stale.to_string_lossy().as_ref(),
        "stale tier did not run first: {result}"
    );
    ensure!(
        actions[1]["tier"] == "pressure-tier"
            && actions[1]["candidate"] == pressure.to_string_lossy().as_ref(),
        "pressure tier did not run second: {result}"
    );
    judged.assert_clean()?;
    Ok(())
}

/// A fixed random seed orders tiers reproducibly inside one group, while
/// the next group stays after it. The dry run makes no filesystem mutation.
pub fn configured_random_group_reports_seed_and_order(ctx: &Ctx) -> TestResult {
    let scratch = &ctx.scratch;
    let mut fixture = Fixture::new();
    for name in [
        "random-first",
        "random-second",
        "random-third",
        "random-last",
    ] {
        scratch.mkdir(name, 0o755, TESTER)?;
        let path = scratch.path(name);
        fixture = fixture.eligible(path);
    }
    ctx.home.write_file(
        "diskgc-random.yaml",
        b"version: 1\nprofiles:\n  - name: first-source\n    find: {roots: [/tmp], paths: [random-first], type: dir}\n    action: delete\n    examples: {match: [/tmp/random-first]}\n  - name: second-source\n    find: {roots: [/tmp], paths: [random-second], type: dir}\n    action: delete\n    examples: {match: [/tmp/random-second]}\n  - name: third-source\n    find: {roots: [/tmp], paths: [random-third], type: dir}\n    action: delete\n    examples: {match: [/tmp/random-third]}\n  - name: last-source\n    find: {roots: [/tmp], paths: [random-last], type: dir}\n    action: delete\n    examples: {match: [/tmp/random-last]}\ngroups:\n  - name: random\n    ranking: random\n    tiers:\n      - name: first\n        select: profile:first-source\n      - name: second\n        select: profile:second-source\n      - name: third\n        select: profile:third-source\n  - name: pressure\n    tiers:\n      - name: last\n        select: profile:last-source\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(fixture)?;
    let run = crate::under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &[("DISKGC_SEED".to_owned(), "42".to_owned())],
        &ctx.diskgc(),
        &[
            "ensure",
            "--dry-run",
            "--json",
            "--config",
            "/home/tester/diskgc-random.yaml",
            "--min-free",
            "1K",
            "/tmp",
        ],
    )?;
    let invalid = crate::under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &[("DISKGC_SEED".to_owned(), "not-a-number".to_owned())],
        &ctx.diskgc(),
        &[
            "ensure",
            "--dry-run",
            "--json",
            "--config",
            "/home/tester/diskgc-random.yaml",
            "--min-free",
            "1K",
            "/tmp",
        ],
    )?;
    let judged = oracle.judge_runs(&[&run, &invalid])?;
    run.expect_code(0)?;
    invalid.expect_code(2)?;
    let invalid_report = report(&invalid)?;
    ensure!(
        invalid_report["status"] == "usage_error",
        "{invalid_report}"
    );
    ensure!(
        invalid_report["actions"] == serde_json::json!([]),
        "{invalid_report}"
    );
    let result = report(&run)?;
    ensure!(result["status"] == "dry_run", "{result}");
    ensure!(result["seed"] == 42, "{result}");
    let plan = result["plan"].as_array().ok_or("plan is not an array")?;
    let order = plan
        .iter()
        .map(|entry| entry["tier"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    ensure!(order == ["first", "third", "second", "last"], "{result}");
    judged.assert_clean()?;
    Ok(())
}

/// Claude's built-in source and a user-configured pytest probe see open
/// files through the real Linux process scan. This proves only the held-file
/// case; a live pytest session can have no descriptor in its test directory.
/// All runs are dry: the test checks classification without deleting.
pub fn ensure_claude_and_pytest_tmp_profiles_keep_open_units(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("claude-1000", 0o755, TESTER)?;
    s.mkdir("claude-1000/-home-tester-proj", 0o755, TESTER)?;
    s.mkdir("claude-1000/-home-tester-proj/session-live", 0o755, TESTER)?;
    s.write_file(
        "claude-1000/-home-tester-proj/session-live/output",
        b"live",
        0o644,
        TESTER,
    )?;
    s.mkdir("claude-1000/-home-tester-proj/session-stale", 0o755, TESTER)?;
    let claude = Holder::tester(
        ctx,
        &[
            "hold",
            "/tmp/claude-1000/-home-tester-proj/session-live/output",
            "/",
        ],
        &[],
    )?;

    s.mkdir("pytest-of-tester", 0o755, TESTER)?;
    s.mkdir("pytest-of-tester/pytest-1", 0o755, TESTER)?;
    s.mkdir("pytest-of-tester/pytest-1/test_live", 0o755, TESTER)?;
    s.write_file(
        "pytest-of-tester/pytest-1/test_live/output",
        b"live",
        0o644,
        TESTER,
    )?;
    s.mkdir("pytest-of-tester/pytest-1/test_stale", 0o755, TESTER)?;
    let pytest = Holder::tester(
        ctx,
        &[
            "hold",
            "/tmp/pytest-of-tester/pytest-1/test_live/output",
            "/",
        ],
        &[],
    )?;

    ctx.home.write_file(
        "pytest-probe.yaml",
        b"version: 1\nprofiles:\n  - name: pytest-unsafe-probe\n    find: {roots: [/tmp], paths: ['pytest-of-*/pytest-*/*'], type: dir}\n    action: delete\n    examples: {match: [/tmp/pytest-of-tester/pytest-1/test_live]}\n",
        0o600,
        TESTER,
    )?;

    // claude-code-tmp was withdrawn (TASK-122); its matching set as a user
    // probe profile keeps this evidence of what the in-use scan sees.
    ctx.home.write_file(
        "claude-probe.yaml",
        b"version: 1\nprofiles:\n  - name: claude-code-tmp-probe\n    find: {roots: [/tmp], paths: ['claude-{uid}/-*/*'], type: dir}\n    action: delete\n    examples: {match: [/tmp/claude-1000/-home-tester-proj/session-live]}\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let claude_run = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--config",
            "/home/tester/claude-probe.yaml",
            "--min-free",
            "1K",
            "--select",
            "profile:claude-code-tmp-probe",
            "/tmp",
        ],
    )?;
    let pytest_run = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--config",
            "/home/tester/pytest-probe.yaml",
            "--min-free",
            "1K",
            "--select",
            "profile:pytest-unsafe-probe",
            "/tmp",
        ],
    )?;
    let judged = oracle.judge_runs(&[&claude_run, &pytest_run])?;
    claude_run.expect_code(0)?;
    pytest_run.expect_code(0)?;
    let reasons = |run: &Run| -> Result<Vec<(String, Option<String>)>, String> {
        let r = report(run)?;
        ensure!(r["status"] == "dry_run", "{r}");
        Ok(r["plan"]
            .as_array()
            .ok_or("plan is not an array")?
            .iter()
            .map(|entry| {
                (
                    entry["candidate"].as_str().unwrap_or("").to_owned(),
                    entry["reason"].as_str().map(str::to_owned),
                )
            })
            .collect())
    };
    let claude_plan = reasons(&claude_run)?;
    let pytest_plan = reasons(&pytest_run)?;
    ensure!(
        claude_plan.iter().any(|(path, why)| path
            == "/tmp/claude-1000/-home-tester-proj/session-live"
            && why.as_deref() == Some("in_use")),
        "live Claude session was not kept: {claude_plan:?}"
    );
    ensure!(
        claude_plan.iter().any(|(path, why)| path
            == "/tmp/claude-1000/-home-tester-proj/session-stale"
            && why.is_none()),
        "stale Claude session was not eligible: {claude_plan:?}"
    );
    ensure!(
        pytest_plan.iter().any(
            |(path, why)| path == "/tmp/pytest-of-tester/pytest-1/test_live"
                && why.as_deref() == Some("in_use")
        ),
        "open pytest test directory was not kept: {pytest_plan:?}"
    );
    ensure!(
        pytest_plan.iter().any(
            |(path, why)| path == "/tmp/pytest-of-tester/pytest-1/test_stale" && why.is_none()
        ),
        "idle pytest sibling was not eligible: {pytest_plan:?}"
    );
    claude.finish()?;
    pytest.finish()?;
    judged.assert_clean()
}

/// The one JSON object a `--json` run printed.
pub(crate) fn report(run: &Run) -> Result<Value, String> {
    let lines: Vec<&str> = run.stdout.lines().collect();
    ensure!(
        lines.len() == 1,
        "--json expected one line, got {}: argv={:?}, exit={:?}, signal={:?}, stdout={:?}, stderr={:?}",
        lines.len(),
        run.argv,
        run.code,
        run.signal,
        run.stdout,
        run.stderr
    );
    serde_json::from_str(lines[0]).map_err(|error| {
        format!(
            "--json parse: {error}: argv={:?}, exit={:?}, signal={:?}, stdout={:?}, stderr={:?}",
            run.argv, run.code, run.signal, run.stdout, run.stderr
        )
    })
}

/// A configured command profile must pass every candidate safety check
/// before exec. Once the holder exits, the same real CLI path runs the
/// cleaner in the checked candidate, supplies its environment, and reports
/// a nonzero exit as an action error without contaminating JSON stdout.
pub fn command_profile_checks_candidate_and_reports_nonzero_exit(ctx: &Ctx) -> TestResult {
    let candidate = ctx.scratch.mkdir("command-product", 0o755, TESTER)?;
    let held_file = ctx
        .scratch
        .write_file("command-product/held", b"held", 0o644, TESTER)?;
    let cleaner = ctx.scratch.path("command-product-cleaner");
    ctx.scratch.write_file(
        "command-product-cleaner",
        b"#!/bin/sh\nset -eu\ntest \"$DISKGC_PATH\" = \"$1\"\ntest \"$DISKGC_PROFILE\" = command-product\ntest -n \"$DISKGC_NEED_BYTES\"\ntest \"$(/bin/stat -Lc '%d:%i' .)\" = \"$2\"\nprintf 'command-product-verified\\n'\nexit 7\n",
        0o755,
        TESTER,
    )?;
    let meta = std::fs::metadata(&candidate)
        .map_err(|error| format!("stat {}: {error}", candidate.display()))?;
    let config = format!(
        "version: 1\nprofiles:\n  - name: command-product\n    find: {{roots: [/tmp], paths: [command-product], type: dir}}\n    action: {{command: [{}, \"{{path}}\", \"{}:{}\"]}}\n    examples: {{match: [/tmp/command-product]}}\n",
        cleaner.display(),
        meta.dev(),
        meta.ino()
    );
    ctx.home
        .write_file("diskgc-command.yaml", config.as_bytes(), 0o644, TESTER)?;
    let args = [
        "--json",
        "--config",
        "/home/tester/diskgc-command.yaml",
        "--min-free",
        "100%",
        "--timeout",
        "10s",
        "--select",
        "profile:command-product",
        "/tmp",
    ];
    let holder = Holder::tester(
        ctx,
        &[
            "hold",
            held_file.to_str().ok_or("non-UTF-8 held file")?,
            "/",
        ],
        &[],
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let held = ensure_run(ctx, &args)?;
    let judged = oracle.judge(&held)?;
    held.expect_code(1)?;
    let held_report = report(&held)?;
    ensure!(
        held_report["skipped"][0]["reason"] == "in_use"
            && held_report["actions"].as_array().is_some_and(Vec::is_empty),
        "in-use candidate reached command action: {held_report}"
    );
    ensure!(
        !held.stderr.contains("command-product-verified"),
        "in-use candidate ran the cleaner"
    );
    judged.assert_clean()?;
    holder.finish()?;

    let oracle = Oracle::arm(Fixture::new())?;
    let run = ensure_run(ctx, &args)?;
    let judged = oracle.judge(&run)?;
    run.expect_code(3)?;
    let result = report(&run)?;
    ensure!(
        result["status"] == "error"
            && result["actions"][0]["candidate"] == candidate.to_string_lossy().as_ref()
            && result["errors"][0]["message"]
                .as_str()
                .is_some_and(|message| message.contains("status 7")),
        "command error was not reported: {result}"
    );
    ensure!(
        run.stderr
            .contains("diskgc: select/command-product: command-product-verified"),
        "command output was not prefixed on stderr: {}",
        run.stderr
    );
    judged.assert_clean()
}

/// A successful command action reaches the real CLI, removes only its
/// checked file, and reports the available-space change around that action.
/// The sibling with an open descriptor must never reach the cleaner.
pub fn command_profile_deletes_only_an_eligible_file(ctx: &Ctx) -> TestResult {
    let candidate = ctx.scratch.write_file(
        "command-success.bin",
        &vec![0x5a; 1024 * 1024],
        0o644,
        TESTER,
    )?;
    let held_file = ctx
        .scratch
        .write_file("command-held.bin", b"held", 0o644, TESTER)?;
    let cleaner = ctx.scratch.path("command-cleaner");
    ctx.scratch.write_file(
        "command-cleaner",
        b"#!/bin/sh\nset -eu\ntest \"$DISKGC_PATH\" = \"$1\"\ntest \"$DISKGC_PROFILE\" = command-success\ntest \"$(/bin/pwd -P)\" = /tmp\n/bin/rm -- \"$1\"\n",
        0o755,
        TESTER,
    )?;
    let config = format!(
        "version: 1\nprofiles:\n  - name: command-success\n    find: {{roots: [/tmp], paths: [command-success.bin, command-held.bin], type: file}}\n    action: {{command: [{}, \"{{path}}\"], cwd: parent}}\n    examples: {{match: [/tmp/command-success.bin, /tmp/command-held.bin]}}\n",
        cleaner.display()
    );
    ctx.home.write_file(
        "diskgc-command-success.yaml",
        config.as_bytes(),
        0o644,
        TESTER,
    )?;
    let holder = Holder::tester(
        ctx,
        &[
            "hold",
            held_file.to_str().ok_or("non-UTF-8 held file")?,
            "/",
        ],
        &[],
    )?;
    let oracle = Oracle::arm(Fixture::new().eligible(candidate.clone()))?;
    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/diskgc-command-success.yaml",
            "--min-free",
            "100%",
            "--timeout",
            "10s",
            "--select",
            "profile:command-success",
            "/tmp",
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(1)?;
    let result = report(&run)?;
    ensure!(!candidate.exists(), "successful cleaner left its candidate");
    ensure!(held_file.exists(), "in-use sibling was removed");
    ensure!(
        result["actions"]
            .as_array()
            .is_some_and(|actions| actions.len() == 1)
            && result["actions"][0]["candidate"] == candidate.to_string_lossy().as_ref()
            && result["actions"][0]["avail_delta_bytes"]
                .as_i64()
                .is_some_and(|delta| delta > 0)
            && result["errors"].as_array().is_some_and(Vec::is_empty)
            && result["skipped"].as_array().is_some_and(|skipped| {
                skipped.iter().any(|entry| {
                    entry["candidate"] == held_file.to_string_lossy().as_ref()
                        && entry["reason"] == "in_use"
                })
            }),
        "successful command action or held sibling was misreported: {result}"
    );
    holder.finish()?;
    judged.assert_clean()
}

/// The shipped Go module cache profile must carry `make_writable` through
/// `ensure` to the real kernel deletion path. Go's downloaded module trees
/// contain read-only directories; the unprivileged tester cannot empty this
/// fixture without that option. The oracle bounds every deletion to `mod`.
pub fn go_mod_cache_builtin_deletes_a_read_only_module_tree(ctx: &Ctx) -> TestResult {
    let home = &ctx.home;
    home.mkdirs("go/pkg/mod/example", 0o755, TESTER)?;
    home.write_file(
        "go/pkg/mod/example/source.go",
        b"package example\n",
        0o644,
        TESTER,
    )?;
    home.chmod("go/pkg/mod/example", 0o555)?;
    home.chmod("go/pkg/mod", 0o555)?;
    let candidate = home.path("go/pkg/mod");
    let oracle = Oracle::arm(Fixture::new().eligible(candidate.clone()))?;
    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--min-free",
            "100%",
            "--select",
            "profile:go-mod-cache",
            "--roots",
            "/home/tester",
            "/home/tester",
        ],
    )?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(1)?;
    ensure!(!candidate.exists(), "read-only Go module cache remains");
    let r = report(&run)?;
    let actions = column(&r, "actions", "candidate");
    ensure!(
        actions == [&Value::from("/home/tester/go/pkg/mod")],
        "{actions:?}"
    );
    verdict.assert_clean()
}

/// The real kernel exposes the running release binary through `/proc/exe`.
/// The oldest unprotected release goes; the old installed one, the running
/// one, and the two newest remain.
pub fn codex_version_running_binary_is_in_use(ctx: &Ctx) -> TestResult {
    let home = &ctx.home;
    home.mkdirs(".local/bin", 0o755, TESTER)?;
    home.mkdirs("bin", 0o755, TESTER)?;
    let releases = ".codex/packages/standalone/releases";
    let mut dirs = Vec::new();
    for version in ["v0", "v1", "v2", "v3", "v4"] {
        dirs.push(home.mkdirs(&format!("{releases}/{version}"), 0o755, TESTER)?);
        if version != "v2" {
            home.write_file(
                &format!("{releases}/{version}/data"),
                b"release\n",
                0o644,
                TESTER,
            )?;
        } else {
            let binary = std::fs::read(&ctx.self_exe)
                .map_err(|error| format!("read fixture executable: {error}"))?;
            home.write_file(&format!("{releases}/v2/codex"), &binary, 0o755, TESTER)?;
        }
    }
    home.symlink(
        ".codex/packages/standalone/current",
        Path::new("releases/v1"),
    )?;
    let candidate_exe = home.path(&format!("{releases}/v2/codex"));
    let running = Holder::tester_program(ctx, &candidate_exe, &["hold-export", "UNRELATED", "x"])?;
    let oracle = Oracle::arm(Fixture::new().eligible(dirs[0].clone()))?;
    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--min-free",
            "100%",
            "--no-index",
            "--select",
            "profile:codex-releases",
            "--roots",
            "/home/tester",
            "/home/tester",
        ],
    )?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(1)?;
    let r = report(&run)?;
    ensure!(
        column(&r, "actions", "candidate") == [&Value::from(dirs[0].display().to_string())],
        "unexpected actions: {r}"
    );
    ensure!(
        r["skipped"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["reason"] == "in_use" && entry["candidate"] == dirs[2].display().to_string()
            })),
        "running version did not report in_use: {r}"
    );
    ensure!(!dirs[0].exists(), "old eligible release remains");
    ensure!(
        dirs[1..].iter().all(|dir| dir.exists()),
        "protected release went"
    );
    running.finish()?;
    verdict.assert_clean()
}

/// The kernel follows `route` before applying `..`, so `current` reaches
/// inside v0. A lexical collapse would instead name the sibling v1 and
/// could expose v0 for deletion. The dry run must retain it.
pub fn codex_current_internal_dotdot_is_ambiguous(ctx: &Ctx) -> TestResult {
    let home = &ctx.home;
    home.mkdirs(".local/bin", 0o755, TESTER)?;
    home.mkdirs("bin", 0o755, TESTER)?;
    let base = ".codex/packages/standalone";
    for version in ["v0", "v1", "v2", "v3"] {
        home.mkdirs(&format!("{base}/releases/{version}"), 0o755, TESTER)?;
    }
    let nested = home.mkdirs(&format!("{base}/releases/v0/releases/v1"), 0o755, TESTER)?;
    home.mkdirs(&format!("{base}/releases/v0/child"), 0o755, TESTER)?;
    home.symlink(&format!("{base}/route"), Path::new("releases/v0/child"))?;
    home.symlink(
        &format!("{base}/current"),
        Path::new("route/../releases/v1"),
    )?;
    let actual = std::fs::canonicalize(home.path(&format!("{base}/current")))
        .map_err(|error| format!("resolve current through kernel: {error}"))?;
    ensure!(
        actual == nested,
        "kernel resolved current to {actual:?}, expected {nested:?}"
    );
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--min-free",
            "100%",
            "--no-index",
            "--select",
            "profile:codex-releases",
            "--roots",
            "/home/tester",
            "/home/tester",
        ],
    )?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    let r = report(&run)?;
    let old = home.path(&format!("{base}/releases/v0"));
    ensure!(
        r["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"] == old.display().to_string()
                    && entry["reason"] == "linked_current"
            })),
        "ambiguous current did not retain v0: {r}"
    );
    ensure!(old.exists(), "dry run touched v0");
    verdict.assert_clean()
}

/// `field` of every entry of the report's list `list`.
pub(crate) fn column<'a>(report: &'a Value, list: &str, field: &str) -> Vec<&'a Value> {
    report[list]
        .as_array()
        .map(|entries| entries.iter().map(|e| &e[field]).collect())
        .unwrap_or_default()
}

/// Removes the provisioned host lock (`/run/diskgc/lock`), as on a host
/// without the tmpfiles drop-in.
pub(crate) fn unprovision(ctx: &Ctx) -> TestResult {
    ctx.run_diskgc.remove_file("lock")?;
    ensure!(!Path::new(HOST_LOCK).exists(), "{HOST_LOCK} still there");
    Ok(())
}

/// A directory of tester's in the scratch tmpfs: PATH and the root. It
/// holds a 4 KiB file, so the tmpfs is never entirely free (an empty tmpfs
/// counts no used blocks, and `--min-free 100%` would be met).
fn work_dir(ctx: &Ctx) -> TestResult {
    ctx.scratch.mkdir("w", 0o755, TESTER)?;
    ctx.scratch
        .write_file("w/fill", &[0u8; 4096], 0o644, TESTER)?;
    let s = rustix::fs::statvfs("/tmp").map_err(|e| format!("statvfs /tmp: {e}"))?;
    ensure!(
        s.f_bavail < s.f_blocks,
        "the scratch tmpfs reports nothing used: {} of {} blocks available",
        s.f_bavail,
        s.f_blocks
    );
    Ok(())
}

/// AC #1 and exit 0: already above `--min-free`, `ensure` exits 0 without
/// taking a lock. The host lock file is removed first, so a run that tried
/// to take a lock would make tester's per-user lock (TASK-277), and none is
/// made; human mode prints nothing, `--json` says `met` with the mount
/// resolved.
pub fn ensure_above_min_free_exits_0_without_the_host_lock(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    unprovision(ctx)?;
    let args = [
        "--min-free",
        "1K",
        "--select",
        "profile:cargo-target",
        "--roots",
        "/tmp/w",
        "/tmp/w",
    ];
    let oracle = Oracle::arm(Fixture::new())?;
    let quiet = ensure_run(ctx, &args)?;
    let mut with_json = args.to_vec();
    with_json.insert(0, "--json");
    let json = ensure_run(ctx, &with_json)?;
    let judged = oracle.judge_runs(&[&quiet, &json])?;
    quiet.expect_code(0)?;
    ensure!(
        quiet.stdout.is_empty(),
        "nothing needed doing: silent, got {:?}",
        quiet.stdout
    );
    json.expect_code(0)?;
    let r = report(&json)?;
    ensure!(r["status"] == "met", "{r}");
    ensure!(r["mount"] == "/tmp" && r["path"] == "/tmp/w", "{r}");
    ensure!(r["target_met"] == true, "{r}");
    ensure!(r["avail_before_bytes"] == r["avail_after_bytes"], "{r}");
    ensure!(column(&r, "actions", "candidate").is_empty(), "{r}");
    no_user_lock_made()?;
    judged.assert_clean()
}

/// tester's per-user lock (docs/safety.md, Host lock): under the account's
/// home (`/home/tester` in the image's passwd), since the container has no
/// `/run/user/1000`.
const USER_LOCK: &str = "/home/tester/.local/state/diskgc/lock";

/// No per-user lock was made: a run that needed no reclaim (or a dry run)
/// makes none, a state write.
fn no_user_lock_made() -> TestResult {
    ensure!(
        !Path::new(USER_LOCK).exists(),
        "{USER_LOCK} was made by a run that takes no lock"
    );
    Ok(())
}

/// A dry run must return while another process owns the host lock. The
/// fixture is confined to the container's /run/diskgc mount, and the
/// deletion oracle permits no changes to the scratch tree.
pub fn ensure_dry_run_plans_while_the_host_lock_is_held(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    let holder = Holder::root(ctx, &["flock-hold", HOST_LOCK])?;
    ensure!(held_by_someone()?, "fixture did not take the host lock");
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--min-free",
            "100%",
            "--timeout",
            "8s",
            "--select",
            "profile:cargo-target",
            "--roots",
            "/tmp/w",
            "/tmp/w",
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    let result = report(&run)?;
    ensure!(
        result["status"] == "dry_run"
            && result["mount"] == "/tmp"
            && result["path"] == "/tmp/w"
            && result["actions"].as_array().is_some_and(Vec::is_empty),
        "dry run under a held lock: {result}"
    );
    ensure!(
        run.took < Duration::from_secs(8),
        "dry run waited for the host lock for {:?}",
        run.took
    );
    ensure!(held_by_someone()?, "dry run released the fixture's lock");
    holder.finish()?;
    judged.assert_clean()
}

/// CLI failures must be one JSON report, with no operation. A syntax error
/// has no resolved mount; a threshold error after measuring PATH has /tmp.
pub fn ensure_flag_errors_are_single_json_reports(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    let cases: &[(&[&str], &str, bool)] = &[
        (&["--min-free", "50"], "bare number", false),
        (
            &["--min-free", "1G", "--target-free", "50"],
            "bare number",
            false,
        ),
        (&["--min-free", "1G", "--timeout", "1M"], "no months", false),
        (
            &["--min-free", "1G", "--tally-budget", "1mo"],
            "no months",
            false,
        ),
        (
            &["--min-free", "1G", "--json=yes"],
            "--json takes no value",
            false,
        ),
        (&["--min-free", "101%"], "--min-free", true),
        (
            &["--min-free", "100%", "--target-free", "1K"],
            "below --min-free",
            true,
        ),
    ];
    let oracle = Oracle::arm(Fixture::new())?;
    let mut runs = Vec::new();
    for (flags, expected, measured) in cases {
        let mut args = vec!["--dry-run", "--json", "--select", "profile:cargo-target"];
        args.extend_from_slice(flags);
        args.extend(["--roots", "/tmp/w", "/tmp/w"]);
        let run = ensure_run(ctx, &args)?;
        runs.push((args, *expected, *measured, run));
    }
    let judged = oracle.judge_runs(&runs.iter().map(|(_, _, _, run)| run).collect::<Vec<_>>())?;
    for (args, expected, measured, run) in &runs {
        run.expect_code(2)?;
        let result = report(run)?;
        let message = result["errors"][0]["message"]
            .as_str()
            .ok_or("usage error has no message")?;
        ensure!(
            result["status"] == "usage_error"
                && message.contains(expected)
                && result["mount"]
                    == if *measured {
                        Value::from("/tmp")
                    } else {
                        Value::Null
                    }
                && result["actions"].as_array().is_some_and(Vec::is_empty),
            "{args:?}: {result}"
        );
    }
    judged.assert_clean()
}

/// AC #2 and exit 2, as root: below `--min-free` without
/// `/run/diskgc/lock`, `ensure --allow-root` exits 2 with cause
/// `provisioning`, printing the two tmpfiles lines (docs/safety.md, Host
/// lock) and the commands that apply them; nothing is deleted. Root never
/// falls back to a per-user lock (TASK-277); a user does
/// ([`ensure_as_a_user_without_the_host_lock_reclaims_under_a_per_user_lock`]).
pub fn ensure_as_root_without_the_host_lock_file_exits_2_printing_the_tmpfiles_lines(
    ctx: &Ctx,
) -> TestResult {
    work_dir(ctx)?;
    unprovision(ctx)?;
    // A protected root policy (root-owned under the root-owned /run/diskgc),
    // as root mode requires; its owner is tester.
    let config = b"version: 1\nowners: [tester]\nprofiles:\n  - name: root-lock-fixture\n    find: {roots: [/tmp], paths: [w], type: dir}\n    action: delete\n    examples: {match: [/tmp/w]}\n";
    ctx.run_diskgc
        .write_file("root-lock.yaml", config, 0o600, ROOT)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_root(
        &ctx.diskgc(),
        &[
            "ensure",
            "--allow-root",
            "--min-free",
            "100%",
            "--json",
            "--config",
            "/run/diskgc/root-lock.yaml",
            "--select",
            "profile:root-lock-fixture",
            "/tmp/w",
        ],
    )?;
    info!("as root without the host lock: {}", run.stderr);
    let judged = oracle.judge(&run)?;
    run.expect_code(2)?;
    for line in [
        "d /run/diskgc      0755 root root -",
        "f /run/diskgc/lock 0644 root root -",
    ] {
        ensure!(
            run.stderr.contains(line),
            "stderr lacks {line:?}: {}",
            run.stderr
        );
    }
    let r = report(&run)?;
    ensure!(r["status"] == "usage_error" && r["mount"] == "/tmp", "{r}");
    // TASK-151: its own cause, and the command that fixes it.
    ensure!(
        r["errors"]
            .as_array()
            .is_some_and(|errors| errors.iter().any(|e| {
                e["cause"] == "provisioning"
                    && e["message"].as_str().is_some_and(|m| {
                        m.contains("host lock /run/diskgc/lock is missing")
                            && m.contains(
                                "sudo systemd-tmpfiles --create /etc/tmpfiles.d/diskgc.conf",
                            )
                    })
            })),
        "no provisioning error with the fix-it command: {r}"
    );
    ensure!(
        !Path::new("/root/.local/state/diskgc/lock").exists(),
        "root made a per-user lock"
    );
    judged.assert_clean()
}

/// TASK-277 (Mark: "any user shall be able to garbage collect his own
/// files"): on a host without `/run/diskgc/lock`, tester's reclaiming
/// `ensure` makes its per-user lock (no `/run/user/1000` here, so under
/// the account's home: `~/.local/state/diskgc`, directory 0700, file 0600,
/// tester's), takes it,
/// deletes the one eligible candidate and exits 0 at the goal, saying in
/// one line which lock it took and that other users' reclaims are not
/// serialised with it. The deletion oracle allows only the candidate (and
/// diskgc's own state directory) to change.
pub fn ensure_as_a_user_without_the_host_lock_reclaims_under_a_per_user_lock(
    ctx: &Ctx,
) -> TestResult {
    let scratch = &ctx.scratch;
    work_dir(ctx)?;
    unprovision(ctx)?;
    scratch.mkdir("userlock-1", 0o755, TESTER)?;
    scratch.write_file("userlock-1/payload", &[5; 16384], 0o644, TESTER)?;
    ctx.home.write_file(
        "diskgc-userlock.yaml",
        b"version: 1\nprofiles:\n  - name: userlock-source\n    find: {roots: [/tmp], paths: [\"userlock-*\"], type: dir}\n    action: delete\n    examples: {match: [/tmp/userlock-1]}\ngroups:\n  - name: userlock\n    tiers:\n      - name: userlock-tier\n        select: profile:userlock-source\n",
        0o600,
        TESTER,
    )?;
    // A goal 4 KiB above what is available now: met once the 16 KiB
    // candidate is gone, not before.
    let goal = format!("{}K", (avail(Path::new("/tmp"))? + 4096).div_ceil(1024));
    let oracle = Oracle::arm(Fixture::new().eligible(scratch.path("userlock-1")))?;
    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/diskgc-userlock.yaml",
            "--min-free",
            &goal,
            "--timeout",
            "20s",
            "/tmp/w",
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    judged.assert_clean()?;
    let r = report(&run)?;
    ensure!(r["status"] == "met" && r["target_met"] == true, "{r}");
    ensure!(
        column(&r, "actions", "candidate") == [&Value::from("/tmp/userlock-1")],
        "{r}"
    );
    ensure!(
        !scratch.path("userlock-1").exists(),
        "the candidate is still there"
    );
    let taken: Vec<&str> = run
        .stderr
        .lines()
        .filter(|l| l.contains(&format!("per-user lock {USER_LOCK} taken after")))
        .collect();
    ensure!(
        taken.len() == 1
            && taken[0].contains("reclaims by other users are not serialised with this one")
            && !run.stderr.contains("sudo"),
        "one line names the per-user lock, without sudo:\n{}",
        run.stderr
    );
    ensure!(!Path::new(HOST_LOCK).exists(), "{HOST_LOCK} appeared");
    for (path, mode) in [(crate::oracle::DISKGC_STATE_DIR, 0o700), (USER_LOCK, 0o600)] {
        let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{path}: {e}"))?;
        ensure!(
            meta.mode() & 0o7777 == mode && meta.uid() == TESTER.uid,
            "{path}: mode {:o}, owner {}",
            meta.mode() & 0o7777,
            meta.uid()
        );
    }
    Ok(())
}

/// The arguments of a reclaiming `ensure` that needs a lock and has nothing
/// to delete (no target below `/tmp/w`).
const RECLAIM_NOTHING: [&str; 7] = [
    "--min-free",
    "100%",
    "--json",
    "--select",
    "profile:cargo-target",
    "--roots",
    "/tmp/w",
];

/// TASK-277 review (Codex 1): another user's object at tester's per-user
/// lock, a root-owned `lock` file (0600) in tester's directory and then a
/// root-owned directory (0700), which tester cannot even read, is inspected
/// through `O_PATH` and refused as unsafe: exit 2, `provisioning`, the
/// message naming the owner; never an `EACCES` (`usage` or a run without a
/// lock). Nothing is deleted, the objects stay.
pub fn ensure_as_a_user_refuses_another_users_per_user_lock(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    unprovision(ctx)?;
    ctx.home.mkdirs(".local/state/diskgc", 0o700, TESTER)?;
    ctx.home
        .write_file(".local/state/diskgc/lock", b"", 0o600, ROOT)?;
    let mut args = RECLAIM_NOTHING.to_vec();
    args.push("/tmp/w");
    for (case, why) in [
        ("root's lock file", "must be a regular file of uid 1000"),
        ("root's directory", "must be owned by uid 1000"),
    ] {
        if case == "root's directory" {
            ctx.home.remove_file(".local/state/diskgc/lock")?;
            ctx.home.chown(".local/state/diskgc", ROOT)?;
        }
        let oracle = Oracle::arm(Fixture::new())?;
        let run = ensure_run(ctx, &args)?;
        oracle.judge(&run)?.assert_clean()?;
        run.expect_code(2)?;
        let r = report(&run)?;
        ensure!(
            r["status"] == "usage_error"
                && r["errors"]
                    .as_array()
                    .is_some_and(|errors| errors.iter().any(|e| {
                        e["cause"] == "provisioning"
                            && e["message"].as_str().is_some_and(|m| {
                                m.contains(&format!("per-user lock {USER_LOCK}")) && m.contains(why)
                            })
                    })),
            "{case}: {r}"
        );
    }
    ensure!(
        std::fs::symlink_metadata(crate::oracle::DISKGC_STATE_DIR)
            .map_err(|e| e.to_string())?
            .uid()
            == ROOT.uid,
        "the root-owned directory was replaced"
    );
    Ok(())
}

/// TASK-277 review (the orchestrator's rule): a per-user lock that cannot
/// be made for a reason of the environment (here `EACCES`: tester's
/// `~/.local` is root's, so `~/.local/state` cannot be made) does not stop
/// a user's reclaim: it runs without a lock, with one warning naming the
/// path and the reason, deletes the eligible candidate and exits 0 at the
/// goal. The oracle allows only the candidate to go.
pub fn ensure_as_a_user_reclaims_unlocked_when_the_per_user_lock_cannot_be_made(
    ctx: &Ctx,
) -> TestResult {
    let scratch = &ctx.scratch;
    work_dir(ctx)?;
    unprovision(ctx)?;
    ctx.home.mkdir(".local", 0o755, ROOT)?;
    scratch.mkdir("userlock-1", 0o755, TESTER)?;
    scratch.write_file("userlock-1/payload", &[5; 16384], 0o644, TESTER)?;
    ctx.home.write_file(
        "diskgc-userlock.yaml",
        b"version: 1\nprofiles:\n  - name: userlock-source\n    find: {roots: [/tmp], paths: [\"userlock-*\"], type: dir}\n    action: delete\n    examples: {match: [/tmp/userlock-1]}\ngroups:\n  - name: userlock\n    tiers:\n      - name: userlock-tier\n        select: profile:userlock-source\n",
        0o600,
        TESTER,
    )?;
    let goal = format!("{}K", (avail(Path::new("/tmp"))? + 4096).div_ceil(1024));
    let oracle = Oracle::arm(Fixture::new().eligible(scratch.path("userlock-1")))?;
    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/diskgc-userlock.yaml",
            "--min-free",
            &goal,
            "--timeout",
            "20s",
            "/tmp/w",
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    judged.assert_clean()?;
    let r = report(&run)?;
    ensure!(r["status"] == "met", "{r}");
    ensure!(
        column(&r, "actions", "candidate") == [&Value::from("/tmp/userlock-1")],
        "{r}"
    );
    let warned: Vec<&str> = run
        .stderr
        .lines()
        .filter(|l| l.contains("runs without a lock"))
        .collect();
    ensure!(
        warned.len() == 1 && warned[0].contains(USER_LOCK) && warned[0].contains("EACCES"),
        "one warning naming the path and the reason:\n{}",
        run.stderr
    );
    ensure!(!Path::new(USER_LOCK).exists(), "{USER_LOCK} was made");
    Ok(())
}

/// A met goal still checks host-lock provisioning without taking the lock.
/// A nonregular lock object warns on stderr while JSON stays met. A missing
/// one does not, for tester: a reclaim would fall back to a per-user lock
/// (TASK-277), which the met run neither makes nor mentions.
pub fn ensure_goal_met_warns_about_an_unready_host_lock(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    unprovision(ctx)?;
    for (case, expected) in [
        ("missing", None),
        ("fifo", Some("host lock /run/diskgc/lock is unusable")),
    ] {
        if case == "fifo" {
            ctx.run_diskgc.fifo("lock", 0o644, ROOT)?;
        }
        let oracle = Oracle::arm(Fixture::new())?;
        let run = ensure_run(
            ctx,
            &[
                "--min-free",
                "1K",
                "--json",
                "--select",
                "profile:cargo-target",
                "--roots",
                "/tmp/w",
                "/tmp/w",
            ],
        )?;
        oracle.judge(&run)?.assert_clean()?;
        run.expect_code(0)?;
        match expected {
            Some(expected) => ensure!(
                run.stderr.contains("warning: host lock is not ready")
                    && run.stderr.contains(expected),
                "{case}: no provisioning warning: {}",
                run.stderr
            ),
            None => ensure!(
                !run.stderr.contains("warning") && !run.stderr.contains("per-user lock"),
                "{case}: a met run said something about the lock: {}",
                run.stderr
            ),
        }
        no_user_lock_made()?;
        let report = report(&run)?;
        ensure!(
            report["status"] == "met"
                && report["errors"].as_array().is_some_and(Vec::is_empty)
                && report["actions"].as_array().is_some_and(Vec::is_empty),
            "{case}: {report}"
        );
    }
    Ok(())
}

/// AC #4 and exit 2: run as root (uid 0 in the container's user
/// namespace) without `--allow-root`, `ensure` exits 2 before measuring
/// anything; `--allow-root` is refused too in this build (it needs
/// `owners:`, task-21).
pub fn ensure_as_root_without_allow_root_exits_2(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    let args = [
        "ensure",
        "--min-free",
        "100%",
        "--json",
        "--select",
        "profile:cargo-target",
        "--roots",
        "/tmp/w",
        "/tmp/w",
    ];
    let oracle = Oracle::arm(Fixture::new())?;
    let root = ctx.run_as_root(&ctx.diskgc(), &args)?;
    let mut allow: Vec<&str> = args.to_vec();
    allow.insert(1, "--allow-root");
    let allowed = ctx.run_as_root(&ctx.diskgc(), &allow)?;
    let judged = oracle.judge_runs(&[&root, &allowed])?;
    info!(
        "as root: {}\nwith --allow-root: {}",
        root.stderr, allowed.stderr
    );
    root.expect_code(2)?;
    ensure!(
        root.stderr.contains("refuses to run as root"),
        "{}",
        root.stderr
    );
    let r = report(&root)?;
    ensure!(r["status"] == "usage_error" && r["mount"].is_null(), "{r}");
    allowed.expect_code(2)?;
    ensure!(allowed.stderr.contains("owners:"), "{}", allowed.stderr);
    judged.assert_clean()
}

/// Root mode reads only a protected policy, resolves tester through the
/// container's passwd database, and applies that uid to the same eligibility
/// walk used by ordinary ensure. Every attempted deletion is oracle checked.
pub fn root_mode_uses_protected_config_and_named_owner(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("root-policy-work", 0o755, TESTER)?;
    s.mkdir("root-policy-work/reclaim", 0o755, TESTER)?;
    s.write_file(
        "root-policy-work/reclaim/payload",
        &[1u8; 4096],
        0o600,
        TESTER,
    )?;
    let config = b"version: 1\nowners: [tester]\nprofiles:\n  - name: root-policy-fixture\n    find: {roots: required, paths: [reclaim], type: dir}\n    action: delete\n    examples: {root: /tmp/root-policy-work, match: [/tmp/root-policy-work/reclaim]}\n";
    ctx.run_diskgc
        .write_file("root-policy.yaml", config, 0o600, ROOT)?;
    ctx.run_diskgc.symlink(
        "root-policy-link.yaml",
        Path::new("/run/diskgc/root-policy.yaml"),
    )?;
    let args = [
        "ensure",
        "--allow-root",
        "--json",
        "--min-free",
        "100%",
        "--config",
        "/run/diskgc/root-policy-link.yaml",
        "--select",
        "profile:root-policy-fixture",
        "--roots",
        "/tmp/root-policy-work",
        "/tmp/root-policy-work",
    ];
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("root-policy-work/reclaim")))?;
    let holder = Holder::root(ctx, &["hold", "/tmp/root-policy-work/reclaim/payload", "/"])?;
    let held_plan = ctx.run_as_root(
        &ctx.diskgc(),
        &[&args[..3], &["--dry-run"], &args[3..]].concat(),
    )?;
    held_plan.expect_code(0)?;
    let held = report(&held_plan)?;
    ensure!(
        held["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"] == "/tmp/root-policy-work/reclaim" && entry["reason"] == "in_use"
            })),
        "root holder was ignored: {held}"
    );
    holder.finish()?;
    let plan = ctx.run_as_root(
        &ctx.diskgc(),
        &[&args[..3], &["--dry-run"], &args[3..]].concat(),
    )?;
    plan.expect_code(0)?;
    let planned = report(&plan)?;
    ensure!(
        planned["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"] == "/tmp/root-policy-work/reclaim" && entry["reason"].is_null()
            })),
        "root plan did not accept tester-owned entry: {planned}"
    );
    ensure!(
        s.path("root-policy-work/reclaim").exists(),
        "dry run deleted entry"
    );
    let run = ctx.run_as_root(&ctx.diskgc(), &args)?;
    let judged = oracle.judge_runs(&[&held_plan, &plan, &run])?;
    ensure!(
        !s.path("root-policy-work/reclaim").exists(),
        "root mode did not reclaim eligible entry: {}",
        run.stderr
    );
    let result = report(&run)?;
    ensure!(
        result["status"] != "usage_error"
            && result["actions"]
                .as_array()
                .is_some_and(|entries| !entries.is_empty()),
        "{result}"
    );
    judged.assert_clean()
}

/// TASK-44 AC #4: container root whose effective set lacks only
/// `CAP_SYS_PTRACE` (the harness's `without-cap-sys-ptrace` shim drops it
/// from the bounding set before exec; Docker's and Podman's default sets
/// lack it too) is refused at start, exit 2 with cause `safety`, by every
/// `--allow-root` mode: `ensure`, `ensure --dry-run`, `watch --tally-only`
/// and a reclaiming `watch`, with a protected `owners:` policy that the
/// control run (same policy, the runner's `CAP_SYS_PTRACE`) accepts.
/// Nothing is deleted: the oracle allows no entry.
pub fn allow_root_without_cap_sys_ptrace_is_refused_in_every_mode(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("root-ptrace-work", 0o755, TESTER)?;
    s.mkdir("root-ptrace-work/reclaim", 0o755, TESTER)?;
    s.write_file(
        "root-ptrace-work/reclaim/payload",
        &[1u8; 4096],
        0o600,
        TESTER,
    )?;
    let config = b"version: 1\nowners: [tester]\nprofiles:\n  - name: root-ptrace-fixture\n    find: {roots: required, paths: [reclaim], type: dir}\n    action: delete\n    examples: {root: /tmp/root-ptrace-work, match: [/tmp/root-ptrace-work/reclaim]}\n";
    ctx.run_diskgc
        .write_file("root-ptrace-policy.yaml", config, 0o600, ROOT)?;
    let common = [
        "--allow-root",
        "--json",
        "--config",
        "/run/diskgc/root-ptrace-policy.yaml",
        "--select",
        "profile:root-ptrace-fixture",
        "--roots",
        "/tmp/root-ptrace-work",
    ];
    let path = "/tmp/root-ptrace-work";
    let diskgc = ctx.diskgc();
    let diskgc = diskgc.to_str().ok_or("diskgc path is not UTF-8")?;
    let modes: [&[&str]; 4] = [
        &["ensure", "--min-free", "100%"],
        &["ensure", "--dry-run", "--min-free", "100%"],
        &["watch", "--tally-only"],
        // Thresholds far below what the scratch tmpfs has free: should the
        // refusal regress, this watch samples and never reclaims, and the
        // shim's alarm ends it (as it ends the tally-only one).
        &["watch", "--min-free", "1M", "--target-free", "1M"],
    ];
    let oracle = Oracle::arm(Fixture::new())?;
    let control = ctx.run_as_root(&ctx.diskgc(), &[modes[1], &common[..], &[path]].concat())?;
    let mut refused = Vec::new();
    for mode in modes {
        let argv = [
            &["without-cap-sys-ptrace", "--", diskgc],
            mode,
            &common[..],
            &[path],
        ]
        .concat();
        refused.push(ctx.run_as_root(&ctx.self_exe, &argv)?);
    }
    let runs: Vec<&Run> = std::iter::once(&control).chain(&refused).collect();
    let judged = oracle.judge_runs(&runs)?;
    control.expect_code(0)?;
    ensure!(
        report(&control)?["status"] == "dry_run",
        "{}",
        control.stdout
    );
    for (mode, run) in modes.iter().zip(&refused) {
        info!("{mode:?} without CAP_SYS_PTRACE: {}", run.stderr);
        run.expect_code(2)?;
        let r = report(run)?;
        ensure!(r["status"] == "usage_error", "{mode:?}: {r}");
        ensure!(r["errors"][0]["cause"] == "safety", "{mode:?}: {r}");
        let message = r["errors"][0]["message"].as_str().unwrap_or_default();
        ensure!(
            message.contains("--allow-root needs CAP_SYS_PTRACE"),
            "{mode:?}: {message}"
        );
        // The set diskgc read: everything the container grants but bit 19
        // (CAP_DAC_OVERRIDE, bit 1, among them): root, not an empty set.
        let caps = message
            .split("CapEff is 0x")
            .nth(1)
            .and_then(|rest| u64::from_str_radix(&rest[..16.min(rest.len())], 16).ok())
            .ok_or_else(|| format!("{mode:?}: no CapEff in {message}"))?;
        ensure!(
            caps & (1 << 19) == 0 && caps & (1 << 1) != 0,
            "{mode:?}: CapEff {caps:#x}"
        );
    }
    ensure!(
        s.path("root-ptrace-work/reclaim/payload").exists(),
        "a refused run deleted the fixture"
    );
    judged.assert_clean()
}

/// A static binary can honor a trusted numeric UID without an account name
/// in the image. The root policy is protected and every deletion is judged.
pub fn root_mode_uses_explicit_uid_without_passwd_entry(ctx: &Ctx) -> TestResult {
    const NUMERIC_OWNER: Owner = Owner {
        uid: 2001,
        gid: 2001,
    };
    let passwd = std::fs::read_to_string("/etc/passwd")
        .map_err(|e| format!("read container passwd: {e}"))?;
    ensure!(
        !passwd
            .lines()
            .any(|line| line.split(':').nth(2) == Some("2001")),
        "the fixture UID unexpectedly has a passwd entry"
    );
    let s = &ctx.scratch;
    s.mkdir("numeric-owner-work", 0o755, NUMERIC_OWNER)?;
    s.mkdir("numeric-owner-work/reclaim", 0o755, NUMERIC_OWNER)?;
    s.write_file(
        "numeric-owner-work/reclaim/payload",
        &[1u8; 4096],
        0o600,
        NUMERIC_OWNER,
    )?;
    let config = b"version: 1\nowners: [{uid: 2001}]\nprofiles:\n  - name: numeric-owner-fixture\n    find: {roots: required, paths: [reclaim], type: dir}\n    action: delete\n    examples: {root: /tmp/numeric-owner-work, match: [/tmp/numeric-owner-work/reclaim]}\n";
    ctx.run_diskgc
        .write_file("numeric-owner.yaml", config, 0o600, ROOT)?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("numeric-owner-work/reclaim")))?;
    let run = ctx.run_as_root(
        &ctx.diskgc(),
        &[
            "ensure",
            "--allow-root",
            "--json",
            "--min-free",
            "100%",
            "--config",
            "/run/diskgc/numeric-owner.yaml",
            "--select",
            "profile:numeric-owner-fixture",
            "--roots",
            "/tmp/numeric-owner-work",
            "/tmp/numeric-owner-work",
        ],
    )?;
    let judged = oracle.judge_runs(&[&run])?;
    ensure!(
        !s.path("numeric-owner-work/reclaim").exists(),
        "numeric owner was not reclaimed: {}",
        run.stderr
    );
    ensure!(
        report(&run)?["status"] != "usage_error",
        "numeric owner was rejected: {}",
        run.stderr
    );
    judged.assert_clean()
}

/// `{uid}` would expand to root's 0, even if owners names one or several
/// other users. The selected profile must be refused before a plan exists.
pub fn root_mode_refuses_uid_placeholder_for_one_or_two_owners(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("claude-0", 0o755, TESTER)?;
    s.mkdir("claude-0/-project", 0o755, TESTER)?;
    s.mkdir("claude-0/-project/session", 0o755, TESTER)?;
    s.write_file("claude-0/-project/session/payload", b"keep", 0o600, TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let mut runs = Vec::new();
    for (name, owners) in [("one", "[tester]"), ("two", "[tester, {uid: 2001}]")] {
        // A user profile with the withdrawn claude-code-tmp's `{uid}` pattern.
        // Its example expands `{uid}` to 1000 as every example does
        // (docs/reference.md), so the file loads and the refusal under
        // test is root mode's, not a failed example.
        let config = format!(
            "version: 1\nowners: {owners}\nprofiles:\n  - name: claude-code-tmp-probe\n    find: {{roots: [/tmp], paths: ['claude-{{uid}}/-*/*'], type: dir}}\n    action: delete\n    examples: {{match: [/tmp/claude-1000/-project/session]}}\n"
        );
        let file = format!("uid-placeholder-{name}.yaml");
        ctx.run_diskgc
            .write_file(&file, config.as_bytes(), 0o600, ROOT)?;
        let path = format!("/run/diskgc/{file}");
        // The user profile, and a built-in whose temp-prefix patterns hold
        // `claude-{uid}` (node-compile-cache): both refused the same way.
        for profile in ["claude-code-tmp-probe", "node-compile-cache"] {
            for dry_run in [false, true] {
                let mut args = vec!["ensure", "--allow-root", "--json", "--min-free", "100%"];
                if dry_run {
                    args.push("--dry-run");
                }
                let select = format!("profile:{profile}");
                args.extend(["--config", &path, "--select", &select, "/tmp"]);
                let run = ctx.run_as_root(&ctx.diskgc(), &args)?;
                run.expect_code(2)?;
                ensure!(
                    run.stderr.contains("`{uid}`")
                        && run.stderr.contains("find.paths")
                        && run.stderr.contains(profile),
                    "{name}, {profile}, dry_run={dry_run}: {}",
                    run.stderr
                );
                let result = report(&run)?;
                ensure!(
                    result["status"] == "usage_error"
                        && result["plan"].as_array().is_some_and(Vec::is_empty),
                    "{name}, {profile}, dry_run={dry_run}: {result}"
                );
                runs.push(run);
            }
        }
    }
    let judged = oracle.judge_runs(&runs.iter().collect::<Vec<_>>())?;
    ensure!(
        s.path("claude-0/-project/session/payload").exists(),
        "root `{{uid}}` selection deleted another owner's session"
    );
    judged.assert_clean()
}

/// Mode checks cover the actual file, an unsafe original ancestor, a link's
/// target ancestor, and system-account resolution before any deletion.
pub fn root_mode_refuses_untrusted_policy_and_system_owner(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("root-rejected", 0o755, TESTER)?;
    s.mkdir("root-rejected/reclaim", 0o755, TESTER)?;
    s.write_file("root-rejected/reclaim/payload", b"keep", 0o600, TESTER)?;
    let policy = b"version: 1\nowners: [tester]\nprofiles:\n  - name: root-rejected-fixture\n    find: {roots: required, paths: [reclaim], type: dir}\n    action: delete\n    examples: {root: /tmp/root-rejected, match: [/tmp/root-rejected/reclaim]}\n";
    ctx.run_diskgc
        .write_file("group-writable.yaml", policy, 0o620, ROOT)?;
    ctx.run_diskgc.mkdir("unsafe-parent", 0o775, ROOT)?;
    ctx.run_diskgc
        .write_file("unsafe-parent/policy.yaml", policy, 0o600, ROOT)?;
    s.write_file("root-link-target.yaml", policy, 0o600, ROOT)?;
    ctx.run_diskgc
        .symlink("linked.yaml", Path::new("/tmp/root-link-target.yaml"))?;
    ctx.run_diskgc
        .write_file("safe-target.yaml", policy, 0o600, ROOT)?;
    s.symlink(
        "root-link-original.yaml",
        Path::new("/run/diskgc/safe-target.yaml"),
    )?;
    let system_policy = std::str::from_utf8(policy).expect("static YAML").replacen(
        "owners: [tester]",
        "owners: [root]",
        1,
    );
    ctx.run_diskgc
        .write_file("system-owner.yaml", system_policy.as_bytes(), 0o600, ROOT)?;
    let unknown_policy = std::str::from_utf8(policy).expect("static YAML").replacen(
        "owners: [tester]",
        "owners: [diskgc-no-such-user-97531]",
        1,
    );
    ctx.run_diskgc
        .write_file("unknown-owner.yaml", unknown_policy.as_bytes(), 0o600, ROOT)?;
    let duplicate_policy = std::str::from_utf8(policy).expect("static YAML").replacen(
        "owners: [tester]",
        "owners: [tester, {uid: 1000}]",
        1,
    );
    ctx.run_diskgc.write_file(
        "duplicate-owner.yaml",
        duplicate_policy.as_bytes(),
        0o600,
        ROOT,
    )?;
    let command_policy = std::str::from_utf8(policy).expect("static YAML").replacen(
        "action: delete",
        "action: {command: [/bin/true, '{path}']}",
        1,
    );
    ctx.run_diskgc
        .write_file("command.yaml", command_policy.as_bytes(), 0o600, ROOT)?;
    let literal_guard_policy = std::str::from_utf8(policy).expect("static YAML").replacen(
        "    action: delete",
        "    where: {protect_links: ['bin/{uid}/current']}\n    action: delete",
        1,
    );
    ctx.run_diskgc.write_file(
        "literal-guard.yaml",
        literal_guard_policy.as_bytes(),
        0o600,
        ROOT,
    )?;
    let protected_policy = format!(
        "{}protect:\n  - {{profile: root-rejected-fixture, roots: [/tmp/root-rejected]}}\n",
        std::str::from_utf8(policy).expect("static YAML")
    );
    ctx.run_diskgc.write_file(
        "protected-source.yaml",
        protected_policy.as_bytes(),
        0o600,
        ROOT,
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let mut runs = Vec::new();
    for (config, reason) in [
        ("/run/diskgc/group-writable.yaml", "StrictModes"),
        ("/run/diskgc/unsafe-parent/policy.yaml", "StrictModes"),
        ("/run/diskgc/linked.yaml", "StrictModes"),
        ("/tmp/root-link-original.yaml", "StrictModes"),
        ("/run/diskgc/system-owner.yaml", "system uid"),
        (
            "/run/diskgc/unknown-owner.yaml",
            "static musl builds cannot load",
        ),
        ("/run/diskgc/duplicate-owner.yaml", "duplicate uid 1000"),
        ("/run/diskgc/command.yaml", "command actions are refused"),
        (
            "/run/diskgc/literal-guard.yaml",
            "is literal and does not expand it",
        ),
    ] {
        let run = ctx.run_as_root(
            &ctx.diskgc(),
            &[
                "ensure",
                "--allow-root",
                "--json",
                "--min-free",
                "100%",
                "--config",
                config,
                "--select",
                "profile:root-rejected-fixture",
                "--roots",
                "/tmp/root-rejected",
                "/tmp/root-rejected",
            ],
        )?;
        run.expect_code(2)?;
        ensure!(run.stderr.contains(reason), "{config}: {}", run.stderr);
        ensure!(
            report(&run)?["status"] == "usage_error",
            "{config}: {}",
            run.stdout
        );
        runs.push(run);
    }
    let protected = ctx.run_as_root(
        &ctx.diskgc(),
        &[
            "ensure",
            "--allow-root",
            "--json",
            "--min-free",
            "100%",
            "--config",
            "/run/diskgc/protected-source.yaml",
            "--select",
            "profile:root-rejected-fixture",
            "--roots",
            "/tmp/root-rejected",
            "/tmp/root-rejected",
        ],
    )?;
    let protected_index = runs.len();
    runs.push(protected);
    let docker = ctx.run_as_root(
        &ctx.diskgc(),
        &[
            "ensure",
            "--allow-root",
            "--json",
            "--min-free",
            "100%",
            "--config",
            "/run/diskgc/safe-target.yaml",
            "--select",
            "profile:docker-build-cache",
            "/tmp/root-rejected",
        ],
    )?;
    docker.expect_code(2)?;
    ensure!(
        docker.stderr.contains("owners: cannot bound"),
        "{}",
        docker.stderr
    );
    ensure!(
        report(&docker)?["status"] == "usage_error",
        "{}",
        docker.stdout
    );
    runs.push(docker);
    let judged = oracle.judge_runs(&runs.iter().collect::<Vec<_>>())?;
    let protected = &runs[protected_index];
    protected.expect_code(1)?;
    let protected_report = report(protected)?;
    ensure!(
        protected_report["status"] == "exhausted"
            && protected_report["actions"]
                .as_array()
                .is_some_and(Vec::is_empty)
            && protected_report["skipped"]
                .as_array()
                .is_some_and(|skipped| {
                    skipped.iter().any(|item| item["reason"] == "protected")
                }),
        "protected source was not enforced: {protected_report}"
    );
    ensure!(
        s.path("root-rejected/reclaim/payload").exists(),
        "a rejected policy deleted payload"
    );
    judged.assert_clean()
}

/// task-92: as root (uid 0 in the container's user namespace), `profiles
/// list` and `show` work (exit 0, the built-ins, no configuration in the
/// image), since they only read and print (docs/safety.md, Privileges),
/// while `ensure` without `--allow-root` still refuses, exit 2, the same
/// binary side by side. Nothing is deleted.
pub fn profiles_work_as_root_while_ensure_refuses(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let list = ctx.run_as_root(&ctx.diskgc(), &["profiles", "list", "--json"])?;
    let show = ctx.run_as_root(
        &ctx.diskgc(),
        &["profiles", "show", "cargo-target", "--json"],
    )?;
    let ensure_run = ctx.run_as_root(
        &ctx.diskgc(),
        &[
            "ensure",
            "--dry-run",
            "--json",
            "--min-free",
            "100%",
            "--select",
            "profile:cargo-target",
            "--roots",
            "/tmp/w",
            "/tmp/w",
        ],
    )?;
    let judged = oracle.judge_runs(&[&list, &show, &ensure_run])?;
    list.expect_code(0)?;
    let listed = report(&list)?;
    ensure!(listed["config"].is_null(), "{listed}");
    let names: Vec<&str> = listed["profiles"]
        .as_array()
        .ok_or_else(|| format!("no profiles list: {listed}"))?
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    ensure!(names.contains(&"cargo-target"), "{names:?}");
    show.expect_code(0)?;
    let shown = report(&show)?;
    ensure!(
        shown["profile"]["checkouts"] == "allow" && shown["profile"]["origin"] == "built_in",
        "{shown}"
    );
    ensure_run.expect_code(2)?;
    ensure!(
        ensure_run.stderr.contains("refuses to run as root"),
        "{}",
        ensure_run.stderr
    );
    let refused = report(&ensure_run)?;
    ensure!(refused["status"] == "usage_error", "{refused}");
    judged.assert_clean()
}

/// The host lock path holding something other than a regular file (a
/// FIFO, a directory, a symlink to a regular file) is refused, exit 2, on a
/// real kernel: the FIFO is never opened (an open for reading would block
/// until a writer came), and nothing is locked or deleted.
pub fn ensure_refuses_a_host_lock_that_is_no_regular_file(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    ctx.scratch.write_file("w/elsewhere", b"", 0o644, TESTER)?;
    let r = &ctx.run_diskgc;
    for what in ["fifo", "directory", "symlink"] {
        r.remove_file("lock")?;
        match what {
            "fifo" => drop(r.fifo("lock", 0o644, ROOT)?),
            "directory" => drop(r.mkdir("lock", 0o755, ROOT)?),
            _ => drop(r.symlink("lock", Path::new("/tmp/w/elsewhere"))?),
        }
        let oracle = Oracle::arm(Fixture::new())?;
        let run = ensure_run(
            ctx,
            &[
                "--min-free",
                "100%",
                "--timeout",
                "10s",
                "--json",
                "--select",
                "profile:cargo-target",
                "--roots",
                "/tmp/w",
                "/tmp/w",
            ],
        )?;
        let judged = oracle.judge(&run)?;
        run.expect_code(2)?;
        ensure!(
            run.stderr
                .contains("host lock /run/diskgc/lock is unusable"),
            "{what}: {}",
            run.stderr
        );
        ensure!(
            report(&run)?["status"] == "usage_error",
            "{what}: {}",
            run.stdout
        );
        ensure!(
            run.took < Duration::from_secs(5),
            "{what}: took {:?} (blocked on the lock path?)",
            run.took
        );
        judged.assert_clean()?;
        // Back to a file, so the next case starts from the same place.
        let lock = Path::new(HOST_LOCK);
        if what == "directory" {
            std::fs::remove_dir(lock).map_err(|e| format!("rmdir {lock:?}: {e}"))?;
        } else {
            std::fs::remove_file(lock).map_err(|e| format!("rm {lock:?}: {e}"))?;
        }
        r.write_file("lock", b"", 0o644, ROOT)?;
    }
    Ok(())
}

/// Exit 75 (ahead of task-54's model tests, since the wait is here): the
/// host lock held by another process until `--timeout`, still below
/// `--min-free`: exit 75 after the deadline, nothing deleted.
pub fn ensure_waits_for_a_held_host_lock_until_the_deadline(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    // As root: the lock file is root's 0644, and flock-hold opens it for
    // writing; root holding it is the "any local user" of docs/safety.md.
    let holder = Holder::root(ctx, &["flock-hold", HOST_LOCK])?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ensure_run(
        ctx,
        &[
            "--min-free",
            "100%",
            "--timeout",
            "2s",
            "--json",
            "--select",
            "profile:cargo-target",
            "--roots",
            "/tmp/w",
            "/tmp/w",
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(75)?;
    ensure!(report(&run)?["status"] == "lock_timeout", "{}", run.stdout);
    ensure!(
        run.took >= Duration::from_secs(2),
        "returned after {:?}, before its deadline",
        run.took
    );
    holder.finish()?;
    judged.assert_clean()
}

/// Lock inheritance (docs/safety.md, Host lock): the host lock taken by
/// diskgc-core's `hostlock::acquire` (through the test driver) is
/// inherited by a child; after the driver closed its own descriptor and
/// exited, the child's copy still holds it, so nothing released it but
/// closing (`flock(LOCK_UN)` on any copy would have released every copy);
/// once the child exits, the lock is free.
pub fn ensure_host_lock_is_inherited_by_children_and_released_only_by_close(
    ctx: &Ctx,
) -> TestResult {
    let gate = ctx.scratch.path("go");
    let gate_text = gate.to_str().ok_or("gate path")?.to_owned();
    ensure!(
        free_within_release_bound()?,
        "the host lock is held before the test"
    );
    let oracle = Oracle::arm(Fixture::new())?;
    let mut spawned = ctx.spawn_as_tester(&ctx.driver(), &["host-lock", &gate_text])?;
    let child = spawned.next_line("child pid=")?;
    spawned.next_line(" closed")?;
    let pid: u32 = child
        .rsplit("pid=")
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| format!("no pid in {child:?}"))?;
    ensure!(
        Path::new(&format!("/proc/{pid}")).exists(),
        "the child {pid} is gone already"
    );
    let still = held_by_someone()?;
    ctx.scratch.write_file("go", b"", 0o644, TESTER)?;
    let run = spawned.wait()?;
    let judged = oracle.judge(&run)?;
    info!("host-lock:\n{}--- stderr\n{}", run.stdout, run.stderr);
    run.expect_code(0)?;
    ensure!(
        still,
        "the host lock was free once the driver closed its copy: the child did not inherit it"
    );
    ensure!(
        free_within_release_bound()?,
        "the host lock is still held after the child exited"
    );
    judged.assert_clean()
}

/// The actual `Delete::run_command` child holds the host lock after its
/// parent is killed. Waiting for the parent's `/proc` entry to disappear
/// makes the lock check prove child inheritance rather than a race with the
/// still-live parent.
pub fn command_action_child_holds_host_lock_after_parent_is_killed(ctx: &Ctx) -> TestResult {
    let gate = ctx.scratch.path("command-go");
    let cwd = ctx.scratch.mkdir("command-cwd", 0o755, TESTER)?;
    let cwd_meta = std::fs::metadata(&cwd).map_err(|e| format!("stat {}: {e}", cwd.display()))?;
    let expected_cwd = format!("command-cwd={}:{}", cwd_meta.dev(), cwd_meta.ino());
    let oracle = Oracle::arm(Fixture::new())?;
    let mut spawned = ctx.spawn_as_tester(
        &ctx.driver(),
        &[
            "command-lock",
            gate.to_str().ok_or("non-UTF-8 gate")?,
            cwd.to_str().ok_or("non-UTF-8 cwd")?,
        ],
    )?;
    let parent = spawned.next_line("parent pid=")?;
    let pid: u32 = parent
        .rsplit("pid=")
        .next()
        .ok_or("missing parent pid")?
        .parse()
        .map_err(|e| format!("invalid parent pid: {e}"))?;
    let ready = gate.with_extension("ready");
    let ready_deadline = Instant::now() + Duration::from_secs(3);
    let actual_cwd = loop {
        match std::fs::read_to_string(&ready) {
            Ok(text) if text.contains("command-child-ready") => {
                break text.lines().next().unwrap_or("").to_owned();
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("read {}: {e}", ready.display())),
        }
        ensure!(
            Instant::now() < ready_deadline,
            "command child did not write readiness file {}",
            ready.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    ensure!(
        actual_cwd == expected_cwd,
        "command cwd dev:ino {actual_cwd:?} != checked {expected_cwd:?}"
    );
    let raw = rustix::process::Pid::from_raw(pid.cast_signed()).ok_or("invalid parent pid")?;
    rustix::process::kill_process(raw, rustix::process::Signal::KILL)
        .map_err(|e| format!("kill command parent {pid}: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !process_exited(pid)? {
        ensure!(
            Instant::now() < deadline,
            "command parent {pid} remained alive after SIGKILL"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    ensure!(
        held_by_someone()?,
        "host lock was free after the command parent died while its child was running"
    );
    ctx.scratch.write_file("command-go", b"", 0o644, TESTER)?;
    let run = spawned.wait()?;
    let judged = oracle.judge(&run)?;
    ensure!(
        run.signal == Some(9),
        "command parent was not killed by SIGKILL: code {:?}, signal {:?}",
        run.code,
        run.signal
    );
    ensure!(
        free_within_release_bound()?,
        "host lock remained held after command exit"
    );
    judged.assert_clean()
}

/// The harness keeps a duplicate of the blocking stderr pipe's write end.
/// The command runner must never change status flags on that shared open
/// file description, including when its parent is killed mid-command.
pub fn command_action_never_changes_inherited_stderr_flags(ctx: &Ctx) -> TestResult {
    let gate = ctx.scratch.path("stderr-flags-go");
    let ready = gate.with_extension("ready");
    let cwd = ctx.scratch.mkdir("stderr-flags-cwd", 0o755, TESTER)?;
    let (read_end, write_end) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .map_err(|e| format!("make blocking stderr pipe: {e}"))?;
    let passed_write = rustix::io::fcntl_dupfd_cloexec(&write_end, 3)
        .map_err(|e| format!("duplicate stderr pipe write end: {e}"))?;
    let before = rustix::fs::fcntl_getfl(&write_end)
        .map_err(|e| format!("read initial stderr flags: {e}"))?;
    ensure!(
        !before.contains(rustix::fs::OFlags::NONBLOCK),
        "test stderr pipe was not blocking"
    );
    let oracle = Oracle::arm(Fixture::new())?;
    let mut spawned = ctx.spawn_as_tester_with_stderr(
        &ctx.driver(),
        &[
            "command-lock",
            gate.to_str().ok_or("non-UTF-8 gate")?,
            cwd.to_str().ok_or("non-UTF-8 cwd")?,
        ],
        passed_write,
    )?;
    let parent = spawned.next_line("parent pid=")?;
    let pid: u32 = parent
        .rsplit("pid=")
        .next()
        .ok_or("missing parent pid")?
        .parse()
        .map_err(|e| format!("invalid parent pid: {e}"))?;
    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        ensure!(
            Instant::now() < ready_deadline,
            "command child did not write readiness file"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let active = rustix::fs::fcntl_getfl(&write_end)
        .map_err(|e| format!("read active stderr flags: {e}"))?;
    let raw = rustix::process::Pid::from_raw(pid.cast_signed()).ok_or("invalid parent pid")?;
    rustix::process::kill_process(raw, rustix::process::Signal::KILL)
        .map_err(|e| format!("kill command parent {pid}: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !process_exited(pid)? {
        ensure!(
            Instant::now() < deadline,
            "command parent {pid} remained alive after SIGKILL"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let after = rustix::fs::fcntl_getfl(&write_end)
        .map_err(|e| format!("read stderr flags after SIGKILL: {e}"))?;
    let child_held_lock = held_by_someone()?;
    ctx.scratch
        .write_file("stderr-flags-go", b"", 0o644, TESTER)?;
    let run = spawned.wait()?;
    let judged = oracle.judge(&run)?;
    drop(read_end);
    ensure!(
        !active.contains(rustix::fs::OFlags::NONBLOCK)
            && !after.contains(rustix::fs::OFlags::NONBLOCK),
        "shared stderr pipe acquired O_NONBLOCK: active={active:?}, after kill={after:?}"
    );
    ensure!(
        child_held_lock,
        "command child lost host lock after parent SIGKILL"
    );
    ensure!(
        run.signal == Some(9),
        "command parent was not killed by SIGKILL"
    );
    ensure!(
        free_within_release_bound()?,
        "host lock remained held after command exit"
    );
    judged.assert_clean()
}

/// SIGTERM makes the leader exit; its descendant ignores TERM. The port
/// must still KILL that descendant without signaling a reused group ID.
pub fn command_action_timeout_kills_descendant_after_leader_exits(ctx: &Ctx) -> TestResult {
    let cwd = ctx.scratch.mkdir("timeout-cwd", 0o755, TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_tester(
        &ctx.driver(),
        &["command-timeout", cwd.to_str().ok_or("non-UTF-8 cwd")?],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        run.stdout.contains("command exit=TimedOut")
            && !run.stdout.contains("command-descendant-pid=")
            && !run.stdout.contains("command-stderr-probe"),
        "command did not time out: {}\n{}",
        run.stdout,
        run.stderr
    );
    ensure!(
        run.stderr
            .contains("diskgc: test-tier/test-profile: command-descendant-pid=")
            && run
                .stderr
                .contains("diskgc: test-tier/test-profile: command-stderr-probe")
            && run
                .stderr
                .contains("diskgc: test-tier/test-profile: command-term-output"),
        "child stdout, stderr, and TERM output were not all prefixed on stderr: {}",
        run.stderr
    );
    let descendant: u32 = run
        .stderr
        .lines()
        .find_map(|line| {
            line.split_once("command-descendant-pid=")
                .map(|(_, pid)| pid)
        })
        .ok_or_else(|| format!("no descendant pid in {}", run.stderr))?
        .parse()
        .map_err(|e| format!("invalid descendant pid: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !process_exited(descendant)? {
        ensure!(
            Instant::now() < deadline,
            "command descendant {descendant} survived group SIGKILL"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    ensure!(
        free_within_release_bound()?,
        "host lock remained held after timeout"
    );
    judged.assert_clean()
}

/// Two 128 KiB streams without line breaks must be forwarded as bounded
/// prefixed segments on stderr. If either pipe were left unread, the child
/// would block after filling it and miss the short deadline.
pub fn command_action_long_output_is_bounded_and_prefixed(ctx: &Ctx) -> TestResult {
    let cwd = ctx.scratch.mkdir("output-cwd", 0o755, TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_tester(
        &ctx.driver(),
        &["command-output", cwd.to_str().ok_or("non-UTF-8 cwd")?],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        run.stdout.contains("command exit=Exited(0)") && !run.stdout.contains("0000000000000000"),
        "child output reached stdout or command failed: {}",
        run.stdout
    );
    let prefix = "diskgc: test-tier/test-profile: ";
    let segments: Vec<&str> = run
        .stderr
        .lines()
        .filter_map(|line| line.strip_prefix(prefix))
        .collect();
    ensure!(
        segments.len() == 64,
        "expected 64 bounded stderr segments, got {}",
        segments.len()
    );
    ensure!(
        segments
            .iter()
            .all(|part| part.len() == 4096 && part.bytes().all(|b| b == b'0')),
        "child output was truncated, unprefixed or not split at 4096 bytes"
    );
    judged.assert_clean()
}

/// A normally exiting leader must remain unreaped until a descendant that
/// inherited its output pipes is killed. Reaping it first would permit the
/// process-group ID to be reused before cleanup.
pub fn command_action_orphaned_output_is_killed_before_reap(ctx: &Ctx) -> TestResult {
    let cwd = ctx.scratch.mkdir("orphan-cwd", 0o755, TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let started = Instant::now();
    let run = ctx.run_as_tester(
        &ctx.driver(),
        &["command-orphan", cwd.to_str().ok_or("non-UTF-8 cwd")?],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        started.elapsed() < Duration::from_secs(4),
        "orphaned output cleanup exceeded the bounded drain"
    );
    ensure!(
        run.stdout.contains("command error=")
            && run.stdout.contains("output pipe remained open")
            && !run.stdout.contains("command-descendant-pid="),
        "orphaned output was not reported as an error: {}\n{}",
        run.stdout,
        run.stderr
    );
    let descendant: u32 = run
        .stderr
        .lines()
        .find_map(|line| {
            line.split_once("command-descendant-pid=")
                .map(|(_, pid)| pid)
        })
        .ok_or_else(|| format!("no prefixed descendant pid in {}", run.stderr))?
        .parse()
        .map_err(|e| format!("invalid descendant pid: {e}"))?;
    ensure!(
        process_exited(descendant)?,
        "orphaned descendant {descendant} survived group SIGKILL"
    );
    ensure!(
        free_within_release_bound()?,
        "host lock remained held after orphan cleanup"
    );
    judged.assert_clean()
}

/// A full blocking fd 2 must fail forwarding promptly and kill the command
/// group. The test driver fills an unread pipe before invoking the port.
pub fn command_action_stalled_stderr_fails_without_hanging(ctx: &Ctx) -> TestResult {
    let cwd = ctx.scratch.mkdir("stalled-stderr-cwd", 0o755, TESTER)?;
    let pid_file = cwd.join("child.pid");
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_tester(
        &ctx.driver(),
        &[
            "command-stalled-stderr",
            cwd.to_str().ok_or("non-UTF-8 cwd")?,
            pid_file.to_str().ok_or("non-UTF-8 pid file")?,
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        run.stdout.contains("command error=")
            && run
                .stdout
                .contains("stderr is not accepting output without blocking"),
        "stalled stderr was not reported: {}\n{}",
        run.stdout,
        run.stderr
    );
    ensure_command_duration_below(&run.stdout, 700)?;
    ensure_command_descendant_exited(&pid_file)?;
    ensure!(
        free_within_release_bound()?,
        "host lock remained held after forwarding failed"
    );
    judged.assert_clean()
}

/// Draining an output pipe held open after leader exit is bounded by the
/// tier deadline plus the ten-second process-group termination grace.
pub fn command_action_leader_exit_drain_obeys_tier_deadline(ctx: &Ctx) -> TestResult {
    let cwd = ctx.scratch.mkdir("short-drain-cwd", 0o755, TESTER)?;
    let pid_file = cwd.join("child.pid");
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_tester(
        &ctx.driver(),
        &[
            "command-short-drain",
            cwd.to_str().ok_or("non-UTF-8 cwd")?,
            pid_file.to_str().ok_or("non-UTF-8 pid file")?,
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        run.stdout.contains("command exit=TimedOut"),
        "leader-exit drain ignored tier deadline: {}\n{}",
        run.stdout,
        run.stderr
    );
    ensure_command_duration_below(&run.stdout, 11_000)?;
    ensure_command_descendant_exited(&pid_file)?;
    ensure!(
        free_within_release_bound()?,
        "host lock remained held after drain timeout"
    );
    judged.assert_clean()
}

/// A descendant may deliberately close both output pipes. The leader's
/// successful exit must still trigger group cleanup before it is reaped.
pub fn command_action_closed_pipe_descendant_is_killed(ctx: &Ctx) -> TestResult {
    let cwd = ctx.scratch.mkdir("closed-pipes-cwd", 0o755, TESTER)?;
    let pid_file = cwd.join("child.pid");
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_tester(
        &ctx.driver(),
        &[
            "command-closed-pipes",
            cwd.to_str().ok_or("non-UTF-8 cwd")?,
            pid_file.to_str().ok_or("non-UTF-8 pid file")?,
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        run.stdout.contains("command exit=Exited(0)"),
        "leader's successful exit was lost: {}\n{}",
        run.stdout,
        run.stderr
    );
    ensure_command_descendant_exited(&pid_file)?;
    ensure!(
        free_within_release_bound()?,
        "host lock remained held after leader exit"
    );
    judged.assert_clean()
}

fn ensure_command_duration_below(stdout: &str, limit_ms: u128) -> TestResult {
    let elapsed: u128 = stdout
        .lines()
        .find_map(|line| line.split_once("command duration_ms=").map(|(_, ms)| ms))
        .ok_or_else(|| format!("no command duration in {stdout}"))?
        .parse()
        .map_err(|e| format!("invalid command duration: {e}"))?;
    ensure!(
        elapsed < limit_ms,
        "command took {elapsed} ms, exceeding the {limit_ms} ms test bound"
    );
    Ok(())
}

fn ensure_command_descendant_exited(pid_file: &Path) -> TestResult {
    let pid: u32 = std::fs::read_to_string(pid_file)
        .map_err(|e| format!("read {}: {e}", pid_file.display()))?
        .trim()
        .parse()
        .map_err(|e| format!("invalid command descendant pid: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !process_exited(pid)? {
        if Instant::now() >= deadline {
            // Keep a failing test from leaving its fixture process behind.
            if let Some(raw) = rustix::process::Pid::from_raw(pid.cast_signed()) {
                let _ = rustix::process::kill_process(raw, rustix::process::Signal::KILL);
            }
            return Err(format!("command descendant {pid} survived group cleanup"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// A zombie has already closed every fd, so it cannot hold the host lock.
fn process_exited(pid: u32) -> Result<bool, String> {
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Ok(text) => Ok(text
            .lines()
            .any(|line| line.starts_with("State:\tZ") || line.starts_with("State:\tX"))),
        Err(e) => Err(format!("read process {pid} status: {e}")),
    }
}

/// Whether someone holds the host lock.
fn held_by_someone() -> Result<bool, String> {
    flock_held(Path::new(HOST_LOCK))
}

/// Whether the host lock is free, or becomes free within the release bound.
fn free_within_release_bound() -> Result<bool, String> {
    flock_free_within_release_bound(Path::new(HOST_LOCK))
}

fn build(ctx: &Ctx, toolchain: &Toolchain, project: &Path) -> TestResult {
    Cargo::new(toolchain, project, &["build"])
        .output(ctx)?
        .expect_success()?;
    Ok(())
}

/// A Git checkout in `old`'s target, where a build script would clone a
/// vendored source (its `OUT_DIR`), relative to the scratch root.
const VENDORED_CHECKOUT: &str = "w/old/target/debug/build/vendored-0000/out/repo/.git";

/// Two projects built in turn, `old` first: `old`'s last build is older
/// (the clock moves on by at least a second between them, so their times
/// differ even on a coarse clock). `old`'s target also holds a Git
/// checkout ([`VENDORED_CHECKOUT`], made before `new` is built, so `old`
/// stays the older): the built-in `cargo-target` says `checkouts: allow`
/// (task-80), so it is part of the target like any other entry.
fn two_targets(ctx: &Ctx, toolchain: &Toolchain) -> Result<(), String> {
    work_dir(ctx)?;
    let old = SMOKE.write(&ctx.scratch, "w/old")?;
    build(ctx, toolchain, &old)?;
    ctx.scratch.mkdirs(VENDORED_CHECKOUT, 0o755, TESTER)?;
    ctx.scratch.write_file(
        &format!("{VENDORED_CHECKOUT}/HEAD"),
        b"ref: refs/heads/main\n",
        0o644,
        TESTER,
    )?;
    std::thread::sleep(Duration::from_millis(1100));
    let new = SMOKE.write(&ctx.scratch, "w/new")?;
    build(ctx, toolchain, &new)
}

/// Exit 0 after a real reclaim: two real targets, a `--min-free` the
/// harness sets halfway into what the older one frees: `ensure` takes the
/// host lock, reclaims `old` first (the tier's `oldest` order), observes
/// the goal and stops, so `new` is untouched. The oracle allows only
/// `old`'s profile entries to go; lock files keep their inodes; the next
/// build of `old` succeeds. The checkout in `old`'s target goes with it
/// (task-80: the built-in profile allows checkouts; no code exempts
/// Cargo).
pub fn ensure_reclaims_the_oldest_real_target_and_stops_at_min_free(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    two_targets(ctx, toolchain)?;
    let old = Path::new("/tmp/w/old/target");
    let freed = freed_by(old)?;
    let before = avail(Path::new("/tmp/w"))?;
    ensure!(freed > 64 << 10, "the old target frees only {freed} bytes");
    let min_k = (before + freed / 2).div_ceil(1024);
    let min = format!("{min_k}K");
    let locks = lock_inodes(old)?;
    let oracle = Oracle::arm(reclaimable(old)?)?;
    let run = ensure_run(
        ctx,
        &[
            "--min-free",
            &min,
            "--json",
            "--select",
            "profile:cargo-target",
            "--roots",
            "/tmp/w",
            "/tmp/w",
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    let r = report(&run)?;
    ensure!(r["status"] == "met", "{r}");
    let actions = column(&r, "actions", "candidate");
    ensure!(
        actions == [&Value::from("/tmp/w/old/target")],
        "actions {actions:?}"
    );
    let delta = r["actions"][0]["avail_delta_bytes"]
        .as_i64()
        .ok_or_else(|| format!("the observed change is not a number: {r}"))?;
    info!(
        "{}: freed {} bytes by the harness's count, observed change {delta}, before {before}, \
         min-free {min}",
        toolchain.name, freed
    );
    ensure!(delta > 0, "the observed change is {delta}");
    ensure!(
        r["avail_after_bytes"].as_u64().unwrap_or(0) >= min_k * 1024,
        "{r}"
    );
    ensure!(lock_inodes(old)? == locks, "lock files gone or replaced");
    ensure!(
        !ctx.scratch.path(VENDORED_CHECKOUT).exists(),
        "the checkout in the old target was not reclaimed with it"
    );
    ensure!(
        Path::new("/tmp/w/new/target/debug")
            .join(SMOKE.binary)
            .exists(),
        "the new target was touched"
    );
    judged.assert_clean()?;
    build(ctx, toolchain, Path::new("/tmp/w/old"))
}

/// Exit 1, twice: (a) a same-source Cargo union takes its shortest positive
/// idle bound, and every recent target is skipped `not_idle`; (b) an
/// unfiltered member admits both targets once, and the goal (100% free)
/// still is not met. Both:
/// every eligible candidate processed without errors, still below
/// `--min-free`.
pub fn ensure_exits_1_when_every_candidate_was_processed_below_min_free(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    two_targets(ctx, toolchain)?;
    let run = |select: &str| {
        ensure_run(
            ctx,
            &[
                "--min-free",
                "100%",
                "--json",
                "--select",
                select,
                "--roots",
                "/tmp/w",
                "/tmp/w",
            ],
        )
    };
    let oracle = Oracle::arm(Fixture::new())?;
    let recent = run(
        "(profile:cargo-target && filter:idle-7d) || (profile:cargo-target && filter:idle-1d)",
    )?;
    let judged = oracle.judge(&recent)?;
    recent.expect_code(1)?;
    let r = report(&recent)?;
    ensure!(r["status"] == "exhausted", "{r}");
    let reasons = column(&r, "skipped", "reason");
    ensure!(
        reasons == [&Value::from("not_idle"), &Value::from("not_idle")],
        "{reasons:?}"
    );
    judged.assert_clean()?;

    let mut fixture = reclaimable(Path::new("/tmp/w/old/target"))?;
    for path in reclaimable_paths(Path::new("/tmp/w/new/target"))? {
        fixture = fixture.eligible(path);
    }
    let oracle = Oracle::arm(fixture)?;
    let all = run("(profile:cargo-target && filter:idle-7d) || profile:cargo-target")?;
    let judged = oracle.judge(&all)?;
    all.expect_code(1)?;
    let r = report(&all)?;
    ensure!(r["status"] == "exhausted", "{r}");
    let actions = column(&r, "actions", "candidate");
    ensure!(
        actions
            == [
                &Value::from("/tmp/w/old/target"),
                &Value::from("/tmp/w/new/target")
            ],
        "oldest first: {actions:?}"
    );
    ensure!(r["errors"].as_array().is_some_and(Vec::is_empty), "{r}");
    judged.assert_clean()
}

/// Every entry of every profile directory of `target` but its lock files.
fn reclaimable_paths(target: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    let mut paths = Vec::new();
    for profile in profile_dirs(target)? {
        paths.extend(deletable(&profile)?);
    }
    Ok(paths)
}

/// `--dry-run` on real targets: it plans and deletes nothing, takes no
/// lock (the host lock file is removed, so a lock attempt would make
/// tester's per-user lock, and none is made), and prints even with the
/// goal met: exit 0, status
/// `dry_run`, the plan listing `old` eligible (sized) before `new`, which a
/// keep marker in its target makes `keep_marker`. The oracle allows nothing to
/// go; the strace record holds no unlink.
pub fn ensure_dry_run_plans_real_targets_and_deletes_nothing(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    two_targets(ctx, toolchain)?;
    ctx.scratch
        .write_file("w/new/target/.diskgc-keep", b"", 0o644, TESTER)?;
    unprovision(ctx)?;
    let args = [
        "--dry-run",
        "--min-free",
        "1K",
        "--select",
        "profile:cargo-target",
        "--roots",
        "/tmp/w",
        "/tmp/w",
    ];
    let oracle = Oracle::arm(Fixture::new())?;
    let human = ensure_run(ctx, &args)?;
    let mut with_json = args.to_vec();
    with_json.insert(0, "--json");
    let json = ensure_run(ctx, &with_json)?;
    let judged = oracle.judge_runs(&[&human, &json])?;
    human.expect_code(0)?;
    ensure!(
        human.stdout.starts_with("dry run: ") && human.stdout.contains("/tmp/w/old/target"),
        "a dry run prints its plan on stdout, got {:?}",
        human.stdout
    );
    json.expect_code(0)?;
    let r = report(&json)?;
    ensure!(r["status"] == "dry_run", "{r}");
    let plan: Vec<(&Value, &Value)> = r["plan"]
        .as_array()
        .map(|p| p.iter().map(|e| (&e["candidate"], &e["reason"])).collect())
        .unwrap_or_default();
    ensure!(
        plan == [
            (&Value::from("/tmp/w/old/target"), &Value::Null),
            (
                &Value::from("/tmp/w/new/target"),
                &Value::from("keep_marker")
            )
        ],
        "plan {plan:?}"
    );
    ensure!(
        r["plan"][0]["estimated_bytes"].as_u64().unwrap_or(0) > 0,
        "{r}"
    );
    ensure!(
        r["plan"][0]["eligible"] == true && r["plan"][1]["eligible"] == false,
        "{r}"
    );
    // The built-in cargo-target allows checkouts (task-80): every plan
    // entry says so.
    ensure!(
        r["plan"]
            .as_array()
            .is_some_and(|p| p.iter().all(|e| e["checkouts"] == "allow")),
        "{r}"
    );
    for run in [&human, &json] {
        let unlinks = run
            .events
            .iter()
            .filter(|e| matches!(e.syscall.as_str(), "unlink" | "unlinkat" | "rmdir"))
            .count();
        ensure!(unlinks == 0, "a dry run made {unlinks} unlink calls");
    }
    // TASK-277: it says what a reclaim would lock, without sudo.
    ensure!(
        human
            .stderr
            .contains("the host lock /run/diskgc/lock is not provisioned")
            && human.stderr.contains("per-user lock")
            && !human.stderr.contains("sudo"),
        "the dry run's lock note: {}",
        human.stderr
    );
    no_user_lock_made()?;
    judged.assert_clean()
}

/// docs/testing.md `ensure_warms_the_snapshot` (TASK-30): three reclaiming
/// `ensure` runs in a row on the container's `/tmp`, a new idle candidate
/// added before each. The first starts from nothing and writes the index
/// snapshot (mode 0600, its lock file 0600, the state directory 0700,
/// under `$HOME/.local/state/diskgc`); the second and third start from it
/// (their synchronous tally lists no root, the kept units are taken from
/// the index) and still find and delete the candidate created after the
/// snapshot. The deletion oracle judges every run: only each run's new
/// candidate goes; the kept ones (`.diskgc-keep`) stay.
pub fn ensure_warms_the_snapshot(ctx: &Ctx) -> TestResult {
    let scratch = &ctx.scratch;
    for kept in ["warm-kept-1", "warm-kept-2"] {
        scratch.mkdir(kept, 0o755, TESTER)?;
        scratch.write_file(&format!("{kept}/.diskgc-keep"), b"", 0o644, TESTER)?;
        scratch.write_file(&format!("{kept}/payload"), &[3; 4096], 0o644, TESTER)?;
    }
    ctx.home.write_file(
        "diskgc-warm.yaml",
        b"version: 1\nprofiles:\n  - name: warm-source\n    find: {roots: [/tmp], paths: [\"warm-*\"], type: dir}\n    action: delete\n    examples: {match: [/tmp/warm-1]}\ngroups:\n  - name: warm\n    tiers:\n      - name: warm-tier\n        select: profile:warm-source\n",
        0o600,
        TESTER,
    )?;
    let state = Path::new(crate::oracle::DISKGC_STATE_DIR);
    for run_number in 1..=3 {
        let fresh = format!("warm-{run_number}");
        scratch.mkdir(&fresh, 0o755, TESTER)?;
        scratch.write_file(&format!("{fresh}/payload"), &[7; 8192], 0o644, TESTER)?;
        let oracle = Oracle::arm(Fixture::new().eligible(scratch.path(&fresh)))?;
        let run = ensure_run(
            ctx,
            &[
                "--json",
                "--config",
                "/home/tester/diskgc-warm.yaml",
                "--min-free",
                "100%",
                "--timeout",
                "20s",
                "/tmp",
            ],
        )?;
        let judged = oracle.judge(&run)?;
        ensure!(
            matches!(run.code, Some(0 | 1)),
            "run {run_number}: exit {:?}",
            run.code
        );
        judged.assert_clean()?;
        ensure!(
            !scratch.path(&fresh).exists(),
            "run {run_number}: {fresh} was not reclaimed"
        );
        for kept in ["warm-kept-1", "warm-kept-2"] {
            ensure!(scratch.path(kept).exists(), "run {run_number}: {kept} gone");
        }
        let lines: Vec<&str> = run.stderr.lines().collect();
        let has = |needle: &str| lines.iter().any(|l| l.contains(needle));
        if run_number == 1 {
            ensure!(
                has("index: no snapshot index-dev-") && has("yet; the synchronous tally starts"),
                "run 1 starts from nothing:\n{}",
                run.stderr
            );
        } else {
            ensure!(
                has("index: started from snapshot index-dev-") && has("2 unit(s)"),
                "run {run_number} starts from the snapshot:\n{}",
                run.stderr
            );
            ensure!(
                has("0 source(s) listed, 1 recent") && has("warm-tier: 2 unit(s) from the index"),
                "run {run_number}: a warm tally and queue:\n{}",
                run.stderr
            );
        }
        ensure!(
            has("index: snapshot index-dev-") && has("written: 2 unit(s)"),
            "run {run_number} writes the snapshot:\n{}",
            run.stderr
        );
    }
    let mode = |p: &Path| -> Result<u32, String> {
        Ok(std::fs::symlink_metadata(p)
            .map_err(|e| format!("{}: {e}", p.display()))?
            .mode()
            & 0o7777)
    };
    let mut names: Vec<String> = std::fs::read_dir(state)
        .map_err(|e| format!("{}: {e}", state.display()))?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    names.sort();
    info!("state directory {}: {names:?}", state.display());
    ensure!(
        names.len() == 2
            && names[0].starts_with("index-dev-")
            && names[1] == format!("{}.lock", names[0]),
        "the snapshot and its lock only: {names:?}"
    );
    ensure!(
        mode(state)? == 0o700,
        "state directory mode {:o}",
        mode(state)?
    );
    for name in &names {
        let at = state.join(name);
        ensure!(
            mode(&at)? == 0o600,
            "{} mode {:o}",
            at.display(),
            mode(&at)?
        );
        let uid = std::fs::metadata(&at).map_err(|e| e.to_string())?.uid();
        ensure!(uid == TESTER.uid, "{} owned by {uid}", at.display());
    }
    Ok(())
}
