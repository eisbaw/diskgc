//! User journeys in containers (docs/testing.md, End-to-end tests in
//! containers: Journeys): the product driven end to end the way one of the
//! PRD's users drives it, through the files that user would write, judged
//! by the deletion oracle and the harness's own walks, never by diskgc's
//! report alone. What a journey shows to be confusing or missing is filed
//! as backlog tasks, not fixed silently (the journey tasks, e.g. task-12).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cargo_harness::{
    Seen, build, deletable, lock_inodes, path_str, profile_dirs, reclaimable, tree_state,
};
use crate::cargo_under_test::{
    self, Cargo, CargoChild, Gate, RACE, REACH_TIMEOUT, SMOKE, Toolchain,
};
use crate::ensure_suite::{avail, column, ensure_run, freed_by, unprovision};
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::proc_locks::{self, Watched};
use crate::suite::{Ctx, TestResult};
use crate::under_test::{self, Run};
use crate::{ROOT, TESTER, UNDER_TEST_BIN_DIR, ensure, info};

/// The pre-job hook as shipped (docs/ci-pre-job.sh): the journey runs it
/// as written, so the example cannot rot.
const PRE_JOB_SCRIPT: &str = include_str!("../../docs/ci-pre-job.sh");

/// The runner's builds directory (`CI_BUILDS_DIR`): the roots and `PATH`.
const BUILDS: &str = "/tmp/builds";

/// The builds directory of a fresh runner, before its first job.
const FRESH_BUILDS: &str = "/tmp/fresh/builds";

/// Where the hook lives and keeps its reports (outside the builds
/// directory, as a runner's own files would be).
const HOOK: &str = "/tmp/ci/pre-job.sh";
const REPORTS: &str = "/tmp/ci/reports";

/// The project whose `cargo test` still runs (its test binary waits at a
/// gate): no lock held, only the process checks protect it.
const TESTS_RUNNING: &str = "builds/tests-running";

/// The project whose `cargo build` is paused in its build script, holding
/// the profile's locks.
const BUILD_RUNNING: &str = "builds/build-running";

/// Idle projects, oldest last build first.
const IDLE: [&str; 3] = ["builds/idle-1", "builds/idle-2", "builds/idle-3"];

/// A Git checkout a build script vendored into the oldest idle target (its
/// `OUT_DIR`): the built-in `cargo-target` says `checkouts: allow`, so it
/// goes with the target (task-80).
const VENDORED_CHECKOUT: &str = "builds/idle-1/target/debug/build/vendored-0000/out/repo/.git";

/// A file a stale process of a previous job still holds open after it was
/// deleted: diskgc cannot free it and must say so (`held_by_deleted_files`).
const HELD_LOG: &str = "ci/previous-job.log";
const HELD_LOG_BYTES: usize = 4 << 20;

/// The busy work on the runner while the hook runs, and the stale holder.
struct Busy {
    tests: CargoChild,
    tests_gate: Gate,
    test_binary: u32,
    build: CargoChild,
    build_gate: Gate,
    holder: Holder,
}

fn target_of(rel: &str) -> PathBuf {
    Path::new("/tmp").join(rel).join("target")
}

/// The CI operator's journey (task-12; PRD, Users: the CI runner fleet
/// operator): a persistent runner's builds directory holds five real
/// Cargo projects: three idle targets of different ages (the oldest with a
/// vendored Git checkout), one whose `cargo build` is in progress (paused
/// in its build script, holding the profile's locks) and one whose test
/// binary still runs (no lock held). A stale process of a previous job
/// holds a deleted log open. In order:
///
/// - the operator previews with `--dry-run` (human mode);
/// - the shipped hook (docs/ci-pre-job.sh) with its default selection
///   (`filter:idle-1d`) and A's goal: every target is seconds old, so
///   nothing is selected, exit 1, nothing deleted;
/// - A: the hook with a `--min-free` / `--target-free` pair set from the
///   harness's statvfs and its own count of what each target frees, so the
///   run must reclaim the two oldest idle targets and stop: exit 0, `met`,
///   `target_met`, the busy targets in `skipped` (`in_use`, `locked`), the
///   held log in `held_by_deleted_files`;
/// - B: a goal no reclaim can meet (`100%`): the last idle target goes,
///   exit 1, `exhausted`, the busy ones still skipped; then the same goal
///   in human mode;
/// - the hook on a fresh runner without a builds directory yet (it makes
///   it; exit 0), misconfigured (an unwritable report path, a relative or
///   unset builds directory) and on a runner without the host lock: exit 2
///   from the hook's own checks, before diskgc runs.
///
/// Every exit 0 or 1 is checked against the harness's own statvfs and
/// min-free, and the hook's summary line against the report. Judged by the
/// oracle (only idle targets' profile entries may go), by the harness's
/// walk (the busy targets unchanged object by object, lock files keep
/// their inodes, which idle ones went), by `/proc/locks` (the build holds
/// its locks, the test binary none) and by the busy commands finishing
/// successfully afterwards; the idle projects build again.
///
/// Apart from the default-selection case, the hook selects
/// `profile:cargo-target` without the idle filter: a target's last build
/// counts ctimes, which cannot be set back, so every fixture target is
/// seconds old. The in-use checks, not the idle filter, are what keeps the
/// busy targets in A and B.
pub fn journey_ci_operator_frees_space_with_ensure(ctx: &Ctx, toolchain: &Toolchain) -> TestResult {
    let busy = set_up_runner(ctx, toolchain)?;
    let busy_targets = [target_of(TESTS_RUNNING), target_of(BUILD_RUNNING)];
    let idle: Vec<PathBuf> = IDLE.iter().map(|p| target_of(p)).collect();
    let mut all_targets = busy_targets.to_vec();
    all_targets.extend(idle.iter().cloned());
    let locks = all_lock_inodes(&all_targets)?;
    let busy_before = trees(&busy_targets)?;
    let idle_3_before = tree_state(&idle[2])?;
    let freed: Vec<u64> = idle.iter().map(|t| freed_by(t)).collect::<Result<_, _>>()?;
    info!("the harness counts the idle targets free {freed:?} bytes");
    for (target, bytes) in idle.iter().zip(&freed) {
        ensure!(
            *bytes > 64 << 10,
            "{} frees only {bytes} bytes",
            target.display()
        );
    }
    let only_idle = || -> Result<Fixture, String> {
        let mut fixture = Fixture::new();
        for target in &idle {
            fixture = fixture.eligible_from(reclaimable(target)?);
        }
        Ok(fixture)
    };

    // Case A's goal: below min-free by half the oldest idle target; the
    // target-free is reached halfway into the second.
    let before = avail(Path::new(BUILDS))?;
    let min_free = Goal::kib(before + freed[0] / 2);
    let target_free = Goal::kib(before + freed[0] + freed[1] / 2);
    let goal_a = [
        ("DISKGC_MIN_FREE", Some(min_free.text.as_str())),
        ("DISKGC_TARGET_FREE", Some(target_free.text.as_str())),
    ];

    // The operator previews first: a dry run, human mode.
    let oracle = Oracle::arm(Fixture::new())?;
    let preview = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--min-free",
            &min_free.text,
            "--target-free",
            &target_free.text,
            "--select",
            "profile:cargo-target",
            "--roots",
            BUILDS,
            BUILDS,
        ],
    )?;
    oracle.judge(&preview)?.assert_clean()?;
    preview.expect_code(0)?;
    expect_preview(&preview, &idle, &busy_targets)?;

    // The hook as shipped, its default selection (targets idle for a day)
    // left in place: every target is seconds old, so nothing is selected.
    let oracle = Oracle::arm(Fixture::new())?;
    let default = pre_job(
        ctx,
        "default",
        &[goal_a[0], goal_a[1], ("DISKGC_SELECT", None)],
    )?;
    oracle.judge(&default.run)?.assert_clean()?;
    expect_space_agrees(&default.run, BUILDS, &min_free)?;
    default.run.expect_code(1)?;
    let r = default.report?;
    ensure!(r["status"] == "exhausted" && acted(&r).is_empty(), "{r}");
    // Idleness is judged under the target's locks (Reclaim protocol, step
    // 2), so the paused build's target stops at its locks first: `locked`,
    // not `not_idle` (task-87).
    let not_idle: Vec<(&Path, &str)> = all_targets
        .iter()
        .map(|t| {
            let reason = if *t == busy_targets[1] {
                "locked"
            } else {
                "not_idle"
            };
            (t.as_path(), reason)
        })
        .collect();
    expect_skipped(&r, &not_idle, "the default selection")?;
    expect_summary(&default.run, &r, 0, all_targets.len(), 0)?;
    expect_unchanged(&busy_before, "the default selection")?;

    // A: the hook frees the two oldest idle targets and stops.
    let oracle = Oracle::arm(only_idle()?)?;
    let a = pre_job(ctx, "a", &goal_a)?;
    oracle.judge(&a.run)?.assert_clean()?;
    expect_space_agrees(&a.run, BUILDS, &min_free)?;
    a.run.expect_code(0)?;
    let r = a.report?;
    ensure!(r["status"] == "met" && r["target_met"] == true, "{r}");
    ensure!(r["mount"] == "/tmp" && r["path"] == BUILDS, "{r}");
    ensure!(
        acted(&r) == idle[..2],
        "A reclaims the two oldest idle targets, oldest first: {:?}",
        acted(&r)
    );
    expect_skipped(
        &r,
        &[(&busy_targets[0], "in_use"), (&busy_targets[1], "locked")],
        "A",
    )?;
    expect_summary(&a.run, &r, 2, 2, 0)?;
    expect_held_log(&r, busy.holder.pid())?;
    let after_a = r["avail_after_bytes"].as_u64().ok_or("avail_after_bytes")?;
    ensure!(
        after_a >= target_free.bytes,
        "A ended at {after_a} bytes, below its target-free {}",
        target_free.text
    );
    for target in &idle[..2] {
        expect_reclaimed(target)?;
    }
    ensure!(
        !ctx.scratch.path(VENDORED_CHECKOUT).exists(),
        "the vendored checkout in the oldest idle target was not reclaimed with it"
    );
    ensure!(
        tree_state(&idle[2])? == idle_3_before,
        "A went past its target-free: the newest idle target changed"
    );
    expect_unchanged(&busy_before, "A")?;

    // B: a goal no reclaim can meet: the whole filesystem available,
    // while the busy targets, the held log and the hook's own files stay.
    let unreachable = Goal::whole_filesystem(Path::new(BUILDS))?;
    let goal_b = [
        ("DISKGC_MIN_FREE", Some(unreachable.text.as_str())),
        ("DISKGC_TARGET_FREE", Some(unreachable.text.as_str())),
    ];
    let oracle = Oracle::arm(only_idle()?)?;
    let b = pre_job(ctx, "b", &goal_b)?;
    oracle.judge(&b.run)?.assert_clean()?;
    expect_space_agrees(&b.run, BUILDS, &unreachable)?;
    b.run.expect_code(1)?;
    ensure!(
        b.run.stderr.contains("pre-job: not enough space"),
        "the hook says why the job fails: {}",
        b.run.stderr
    );
    let r = b.report?;
    ensure!(
        r["status"] == "exhausted" && r["target_met"] == false,
        "{r}"
    );
    ensure!(
        acted(&r) == idle[2..],
        "B reclaims the last idle target: {:?}",
        acted(&r)
    );
    expect_skipped(
        &r,
        &[
            (&busy_targets[0], "in_use"),
            (&busy_targets[1], "locked"),
            (&idle[0], "empty"),
            (&idle[1], "empty"),
        ],
        "B",
    )?;
    expect_summary(&b.run, &r, 1, 2, 2)?;
    expect_held_log(&r, busy.holder.pid())?;
    expect_reclaimed(&idle[2])?;
    expect_unchanged(&busy_before, "B")?;

    // The same goal in human mode: what a person reading the job log sees.
    let oracle = Oracle::arm(Fixture::new())?;
    let human = ensure_run(
        ctx,
        &[
            "--min-free",
            &unreachable.text,
            "--select",
            "profile:cargo-target",
            "--roots",
            BUILDS,
            BUILDS,
        ],
    )?;
    oracle.judge(&human)?.assert_clean()?;
    expect_space_agrees(&human, BUILDS, &unreachable)?;
    human.expect_code(1)?;
    ensure!(
        human.stdout.starts_with("exhausted: ") && human.stdout.lines().count() == 1,
        "human mode prints a one-line summary: {:?}",
        human.stdout
    );
    expect_unchanged(&busy_before, "the human-mode run")?;

    // A fresh runner: its builds directory does not exist before its first
    // job; the hook makes it, and the goal is met without a reclaim. Its
    // report goes where the hook puts it by default (mktemp).
    ctx.scratch.mkdir("fresh", 0o755, TESTER)?;
    let tiny = Goal::kib(1);
    let oracle = Oracle::arm(Fixture::new())?;
    let fresh = pre_job(
        ctx,
        "fresh",
        &[
            ("CI_BUILDS_DIR", Some(FRESH_BUILDS)),
            ("DISKGC_MIN_FREE", Some(tiny.text.as_str())),
            ("DISKGC_REPORT", None),
        ],
    )?;
    oracle.judge(&fresh.run)?.assert_clean()?;
    expect_space_agrees(&fresh.run, FRESH_BUILDS, &tiny)?;
    fresh.run.expect_code(0)?;
    let r = fresh.report?;
    ensure!(
        r["status"] == "met" && r["path"] == FRESH_BUILDS && acted(&r).is_empty(),
        "{r}"
    );
    expect_summary(&fresh.run, &r, 0, 0, 0)?;
    ensure!(
        fresh.run.stdout.contains("report in /tmp/diskgc-pre-job."),
        "the default report is a new file in /tmp: {}",
        fresh.run.stdout
    );
    ensure!(
        Path::new(FRESH_BUILDS).is_dir(),
        "the hook did not make the builds directory"
    );

    // Misconfigured hooks stop with exit 2 and say why, before diskgc
    // runs: a report path that cannot be written (a directory), and a
    // builds directory unset or relative.
    let misconfigured: [(&str, &[Var], &str); 3] = [
        (
            "report-unwritable",
            &[("DISKGC_REPORT", Some(REPORTS))],
            "pre-job: cannot write the report /tmp/ci/reports",
        ),
        (
            "builds-relative",
            &[("CI_BUILDS_DIR", Some("builds"))],
            "pre-job: CI_BUILDS_DIR must name the builds directory as an absolute path",
        ),
        (
            "builds-unset",
            &[("CI_BUILDS_DIR", None)],
            "pre-job: CI_BUILDS_DIR must name the builds directory as an absolute path",
        ),
    ];
    for (case, vars, message) in misconfigured {
        let oracle = Oracle::arm(Fixture::new())?;
        let run = pre_job(ctx, case, vars)?.run;
        oracle.judge(&run)?.assert_clean()?;
        run.expect_code(2)?;
        ensure!(
            run.stderr.contains(message) && !run.stdout.contains("diskgc ensure exited"),
            "{case}: expected {message:?} before diskgc runs:\n{}{}",
            run.stdout,
            run.stderr
        );
    }

    // A runner set up without the host lock: the hook's own check stops it
    // even though the goal is met (diskgc would exit 0 without looking).
    unprovision(ctx)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let unprovisioned = pre_job(ctx, "unprovisioned", &[("DISKGC_MIN_FREE", Some("1K"))])?;
    let run = unprovisioned.run;
    oracle.judge(&run)?.assert_clean()?;
    run.expect_code(2)?;
    for line in [
        "d /run/diskgc      0755 root root -",
        "f /run/diskgc/lock 0644 root root -",
    ] {
        ensure!(
            run.stderr.contains(line),
            "the hook prints the tmpfiles line {line:?}: {}",
            run.stderr
        );
    }
    ensure!(
        !run.stdout.contains("diskgc ensure exited"),
        "the hook ran diskgc without the host lock: {}",
        run.stdout
    );

    ensure!(
        all_lock_inodes(&all_targets)? == locks,
        "lock files gone or replaced"
    );
    finish_busy(ctx, busy)?;
    for project in IDLE {
        build(ctx, toolchain, &ctx.scratch.path(project), &["build"])?;
    }
    info!(
        "journey times: dry run {:.2?}, hook default {:.2?}, A {:.2?}, B {:.2?}, human {:.2?}",
        preview.took, default.run.took, a.run.took, b.run.took, human.took
    );
    Ok(())
}

/// An `ensure` size as the operator writes it, and the bytes it means.
struct Goal {
    text: String,
    bytes: u64,
}

impl Goal {
    /// `bytes` rounded up to whole KiB (`K` is 1024).
    fn kib(bytes: u64) -> Self {
        let k = bytes.div_ceil(1024);
        Self {
            text: format!("{k}K"),
            bytes: k * 1024,
        }
    }

    /// `100%`: the size of the filesystem holding `path` (`f_blocks ×
    /// f_frsize`, docs/reference.md, Sizes), by the harness's statvfs.
    fn whole_filesystem(path: &Path) -> Result<Self, String> {
        let s = rustix::fs::statvfs(path).map_err(|e| format!("statvfs {path:?}: {e}"))?;
        Ok(Self {
            text: "100%".to_owned(),
            bytes: s.f_blocks * s.f_frsize,
        })
    }
}

/// The exit code against the harness's own statvfs of `path`, not
/// diskgc's report: 0 only with at least `min_free` available, 1 only
/// below it (docs/reference.md, Exit codes). Checked before the expected
/// code, so a diskgc that says "met" while below fails here.
fn expect_space_agrees(run: &Run, path: impl AsRef<Path>, min_free: &Goal) -> TestResult {
    let path = path.as_ref();
    let now = avail(path)?;
    match run.code {
        Some(0) => ensure!(
            now >= min_free.bytes,
            "exit 0, but the harness sees {now} bytes available on {}, below --min-free {} \
             ({} bytes)",
            path.display(),
            min_free.text,
            min_free.bytes
        ),
        Some(1) => ensure!(
            now < min_free.bytes,
            "exit 1, but the harness sees {now} bytes available on {}, at least --min-free {} \
             ({} bytes)",
            path.display(),
            min_free.text,
            min_free.bytes
        ),
        _ => {}
    }
    Ok(())
}

/// The hook's summary line says what the report says: its status, space
/// before and after, min-free and target met (the fields it reads without
/// jq), and the counts of reclaimed, kept and already empty targets.
fn expect_summary(
    run: &Run,
    report: &Value,
    reclaimed: usize,
    kept: usize,
    empty: usize,
) -> TestResult {
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let want = format!(
        "status {}, {} -> {} bytes available (min-free {}, target met: {}); {reclaimed} \
         target(s) reclaimed, {kept} kept, {empty} already empty;",
        text(&report["status"]),
        text(&report["avail_before_bytes"]),
        text(&report["avail_after_bytes"]),
        text(&report["min_free_bytes"]),
        text(&report["target_met"]),
    );
    ensure!(
        run.stdout.contains(&want),
        "the hook's summary lacks {want:?}: {:?}",
        run.stdout
    );
    Ok(())
}

/// The runner: five projects in the builds directory, the busy work
/// started before the idle projects are built (so the busy targets are the
/// oldest and the reclaim meets them first), the hook installed, and a
/// stale holder of a deleted log.
fn set_up_runner(ctx: &Ctx, toolchain: &Toolchain) -> Result<Busy, String> {
    let s = &ctx.scratch;
    s.mkdir("builds", 0o755, TESTER)?;
    s.mkdir("ci", 0o755, ROOT)?;
    s.write_file("ci/pre-job.sh", PRE_JOB_SCRIPT.as_bytes(), 0o755, ROOT)?;
    s.mkdir("ci/reports", 0o755, TESTER)?;

    // A test binary still running: built, then `cargo test` waits in it.
    let tests = RACE.write(s, TESTS_RUNNING)?;
    build(ctx, toolchain, &tests, &["build"])?;
    build(ctx, toolchain, &tests, &["test", "--no-run"])?;
    let tests_target = tests.join("target");
    let tests_gate = Gate::new(s, "journey-test-binary")?;
    let tests_cargo = Cargo::new(toolchain, &tests, &["test", "--", "--nocapture"])
        .test_gate(&tests_gate)
        .spawn(ctx)?;
    let test_binary = tests_gate.wait_until_waiting(REACH_TIMEOUT)?;
    let exe = std::fs::read_link(format!("/proc/{test_binary}/exe"))
        .map_err(|e| format!("readlink /proc/{test_binary}/exe: {e}"))?;
    ensure!(
        exe.starts_with(&tests_target),
        "the waiting test (pid {test_binary}) runs {}, not a binary of its target",
        exe.display()
    );
    let mut lock_files = Vec::new();
    for profile in profile_dirs(&tests_target)? {
        lock_files.extend(cargo_under_test::lock_files(&profile)?);
    }
    let sample = proc_locks::sample()?;
    let held = Watched::files(&lock_files)?.locks_on(&sample).len();
    ensure!(
        held == 0,
        "{held} lock(s) held on the running tests' lock files: only the process checks may \
         protect it"
    );

    // A build in progress: built once, then a rebuild paused in its build
    // script, holding the profile's locks (/proc/locks, not Cargo's word).
    let building = RACE.write(s, BUILD_RUNNING)?;
    build(ctx, toolchain, &building, &["build"])?;
    let build_gate = Gate::new(s, "journey-build-script")?;
    let build_cargo = Cargo::new(toolchain, &building, &["build"])
        .build_gate(&build_gate)
        .spawn(ctx)?;
    build_gate.wait_until_waiting(REACH_TIMEOUT)?;
    let debug = building.join("target/debug");
    let usage = Watched::files(&cargo_under_test::lock_files(&debug)?)?
        .usage(&proc_locks::sample()?, build_cargo.pid());
    ensure!(
        usage.holds(&debug.join(".cargo-lock")),
        "the paused build does not hold .cargo-lock: {usage:?}"
    );

    // Idle targets of three ages; the clock moves on by more than a second
    // between them, so their last builds differ on a coarse clock too.
    for (n, project) in IDLE.iter().enumerate() {
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let dir = SMOKE.write(s, project)?;
        build(ctx, toolchain, &dir, &["build"])?;
        if n == 0 {
            s.mkdirs(VENDORED_CHECKOUT, 0o755, TESTER)?;
            s.write_file(
                &format!("{VENDORED_CHECKOUT}/HEAD"),
                b"ref: refs/heads/main\n",
                0o644,
                TESTER,
            )?;
        }
    }

    s.write_file(HELD_LOG, &vec![b'x'; HELD_LOG_BYTES], 0o644, TESTER)?;
    let held_log = s.path(HELD_LOG);
    let holder = Holder::tester(ctx, &["hold", path_str(&held_log)?, "/"], &[])?;
    std::fs::remove_file(&held_log).map_err(|e| format!("rm {}: {e}", held_log.display()))?;

    Ok(Busy {
        tests: tests_cargo,
        tests_gate,
        test_binary,
        build: build_cargo,
        build_gate,
        holder,
    })
}

/// One of the hook's environment variables: set (`Some`) or unset (`None`).
type Var<'a> = (&'a str, Option<&'a str>);

/// A finished hook run and the report it kept, or why there is none (it
/// stopped before diskgc ran, or the report is not one JSON object).
struct HookRun {
    run: Run,
    report: Result<Value, String>,
}

/// Runs the pre-job hook as tester, traced, with the operator's settings:
/// diskgc on `PATH`, `CI_BUILDS_DIR` [`BUILDS`], `DISKGC_SELECT`
/// `profile:cargo-target` (see the journey's note on `filter:idle-1d`) and
/// the report at `REPORTS/<case>.json` (unset: the path the hook prints);
/// `vars` then sets (`Some`) or
/// unsets (`None`) variables. The caller arms and judges the oracle.
fn pre_job(ctx: &Ctx, case: &str, vars: &[Var]) -> Result<HookRun, String> {
    let mut env: BTreeMap<String, String> = [
        ("PATH", format!("{UNDER_TEST_BIN_DIR}:/bin")),
        ("CI_BUILDS_DIR", BUILDS.to_owned()),
        ("DISKGC_SELECT", "profile:cargo-target".to_owned()),
        ("DISKGC_REPORT", format!("{REPORTS}/{case}.json")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    for (key, value) in vars {
        match value {
            Some(v) => env.insert((*key).to_owned(), (*v).to_owned()),
            None => env.remove(*key),
        };
    }
    let report_path = env.get("DISKGC_REPORT").cloned();
    let env: Vec<(String, String)> = env.into_iter().collect();
    let run = under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        Path::new("/bin/sh"),
        &[HOOK],
    )?;
    info!(
        "pre-job hook ({case}, {vars:?}) -> {:?} in {:.2?}\n--- stdout\n{}--- stderr\n{}",
        run.code, run.took, run.stdout, run.stderr
    );
    // Without DISKGC_REPORT the hook makes its own and names it.
    let report = report_path
        .or_else(|| {
            run.stdout
                .split("; report in ")
                .nth(1)
                .map(|rest| rest.trim_end().to_owned())
        })
        .ok_or_else(|| "the hook named no report".to_owned())
        .and_then(|path| {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
            info!("report {path}:\n{text}");
            let lines: Vec<&str> = text.lines().collect();
            ensure!(
                lines.len() == 1,
                "{path}: --json writes one line, got {text:?}"
            );
            serde_json::from_str(lines[0]).map_err(|e| format!("{path}: {e}"))
        });
    Ok(HookRun { run, report })
}

/// The lock files of every target, by inode.
fn all_lock_inodes(targets: &[PathBuf]) -> Result<BTreeMap<PathBuf, u64>, String> {
    let mut all = BTreeMap::new();
    for target in targets {
        all.extend(lock_inodes(target)?);
    }
    Ok(all)
}

/// The harness's walk of each target (inode, size, mtime, ctime of every
/// object).
fn trees(targets: &[PathBuf]) -> Result<BTreeMap<PathBuf, BTreeMap<PathBuf, Seen>>, String> {
    targets
        .iter()
        .map(|t| Ok((t.clone(), tree_state(t)?)))
        .collect()
}

/// Fails unless every busy target is unchanged, object by object: nothing
/// deleted, created, written or replaced in it.
fn expect_unchanged(before: &BTreeMap<PathBuf, BTreeMap<PathBuf, Seen>>, when: &str) -> TestResult {
    for (target, was) in before {
        let now = tree_state(target)?;
        let changes = crate::cargo_harness::tree_changes(was, &now);
        ensure!(
            changes.is_empty(),
            "{when}: the busy target {} changed: {changes:#?}",
            target.display()
        );
    }
    Ok(())
}

/// Fails unless every profile directory of `target` holds nothing but its
/// lock files (what a reclaim leaves: docs/safety.md, Reclaim protocol).
fn expect_reclaimed(target: &Path) -> TestResult {
    for profile in profile_dirs(target)? {
        let left = deletable(&profile)?;
        ensure!(
            left.is_empty(),
            "{} was not reclaimed: {left:?} left",
            profile.display()
        );
    }
    Ok(())
}

/// The candidates of the report's `actions`, in order.
fn acted(report: &Value) -> Vec<PathBuf> {
    column(report, "actions", "candidate")
        .into_iter()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .collect()
}

/// `(candidate, reason)` of every `skipped` entry.
fn kept(report: &Value) -> Result<Vec<(PathBuf, String)>, String> {
    let entries = report["skipped"]
        .as_array()
        .ok_or_else(|| format!("no skipped list: {report}"))?;
    entries
        .iter()
        .map(|e| match (e["candidate"].as_str(), e["reason"].as_str()) {
            (Some(candidate), Some(reason)) => Ok((PathBuf::from(candidate), reason.to_owned())),
            _ => Err(format!("a skipped entry without candidate or reason: {e}")),
        })
        .collect()
}

/// Fails unless the report's `skipped` holds exactly `want` (candidate,
/// reason), in any order: the busy targets kept for what keeps them (the
/// running test binary `in_use`, the paused build `locked`: Reclaim
/// protocol, steps 2 and 1), and targets an earlier run reclaimed, `empty`.
fn expect_skipped(report: &Value, want: &[(&Path, &str)], case: &str) -> TestResult {
    let got = kept(report)?;
    ensure!(
        got.len() == want.len()
            && want
                .iter()
                .all(|(t, r)| got.contains(&(t.to_path_buf(), (*r).to_owned()))),
        "{case}: skipped {got:?}, expected {want:?}"
    );
    Ok(())
}

/// Fails unless the deleted log the stale holder keeps open is listed with
/// its process and at least its size (docs/reference.md, Output).
fn expect_held_log(report: &Value, pid: u32) -> TestResult {
    let held = report["held_by_deleted_files"]
        .as_array()
        .ok_or_else(|| format!("no held_by_deleted_files: {report}"))?;
    ensure!(
        held.iter().any(|h| h["pid"] == pid
            && h["command"] == "diskgc-e2e"
            && h["bytes"].as_u64().unwrap_or(0) >= HELD_LOG_BYTES as u64),
        "the deleted log held by pid {pid} is not in held_by_deleted_files: {held:?}"
    );
    Ok(())
}

/// The dry run's human plan: the three idle targets eligible, oldest
/// first, and both busy targets skipped (the dry run takes no lock, so the
/// paused build is kept by its process, not its locks); nothing else.
fn expect_preview(run: &Run, idle: &[PathBuf], busy: &[PathBuf; 2]) -> TestResult {
    let lines: Vec<&str> = run.stdout.lines().collect();
    ensure!(
        lines.first().is_some_and(|l| l.starts_with("dry run: ")),
        "{:?}",
        run.stdout
    );
    let eligible: Vec<&str> = lines
        .iter()
        .filter(|l| l.trim_start().starts_with("eligible"))
        .copied()
        .collect();
    let mentions =
        |line: &str, target: &Path| line.split_whitespace().any(|w| Path::new(w) == target);
    ensure!(
        eligible.len() == idle.len() && eligible.iter().zip(idle).all(|(l, t)| mentions(l, t)),
        "eligible, oldest first: {idle:?}; plan:\n{}",
        run.stdout
    );
    for target in busy {
        ensure!(
            lines
                .iter()
                .any(|l| l.trim_start().starts_with("skip ") && mentions(l, target)),
            "{} is not skipped in the plan:\n{}",
            target.display(),
            run.stdout
        );
    }
    ensure!(lines.len() == 1 + idle.len() + busy.len(), "{}", run.stdout);
    Ok(())
}

/// Lets the busy work go on; both commands must succeed (the test ran to
/// its end), and the stale holder exits cleanly.
fn finish_busy(ctx: &Ctx, busy: Busy) -> TestResult {
    let Busy {
        tests,
        tests_gate,
        test_binary,
        build,
        build_gate,
        holder,
    } = busy;
    tests_gate.open(&ctx.scratch)?;
    let out = tests.wait()?;
    out.expect_success()?;
    ensure!(
        out.stdout
            .contains("diskgc-e2e cargo race fixture test ran"),
        "the test binary (pid {test_binary}) did not run to its end:\n{}",
        out.failure()
    );
    build_gate.open(&ctx.scratch)?;
    build.wait()?.expect_success()?;
    holder.finish()
}
