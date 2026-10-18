//! The Cargo race tests 1-9 (task-11; docs/testing.md, End-to-end tests in
//! containers: Cargo race tests) that the earlier suites do not already
//! hold, against real Cargo processes of every toolchain under test. Which
//! test is which race test, which step of the reclaim protocol
//! (docs/safety.md, `cargo-target`: Reclaim protocol) it checks, and which
//! of its assertions are independent of diskgc and which come from the
//! driver's report, is the table in docs/testing.md (Cargo race tests).
//!
//! The independent evidence: `/proc/locks` samples (one `read` each,
//! [`proc_locks::sample`]), the deletion oracle (snapshot and strace
//! record), the harness's own walks of the target, and whether the Cargo
//! commands succeed. Waits have wall-clock hang guards; a state the test
//! asserts is checked over a count of samples, not a time (this host's
//! wall time varies several-fold under load).
//!
//! diskgc runs as the test driver (`diskgc-e2e-driver cargo-reclaim`,
//! traced, paused or killed at `OP:N` positions); concurrent Cargo
//! processes are untraced fixtures, declared to the oracle as concurrent
//! writers only where they write while a judged run is going.

use std::path::{Path, PathBuf};

use diskgc_core::engine::CARGO_LOCK_NAMES;

use crate::cargo_harness::{
    PlannedPart, SAMPLE_GAP, SIGKILL, STEADY_SAMPLES, build, cargo_reclaim, deletable, expect_skip,
    first_unlink_of, layout_of, lock_inodes, lock_usage, name_of, part_objects, path_str,
    planned_parts, profile_dirs, rebuilds, reclaim_touching_nothing, reclaimable, results,
    spawn_paused, steadily_blocked, tree_changes, tree_state, unlinked, verdict, verdict_of,
    watch_target_locks,
};
use crate::cargo_under_test::{self, Cargo, Gate, RACE, REACH_TIMEOUT, Toolchain};
use crate::holder::Holder;
use crate::oracle::Oracle;
use crate::proc_locks::{self, Watched};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{TESTER, ensure, info};

/// The Cargo commands race test 2 starts during a deletion.
const RACE_2_COMMANDS: [&[&str]; 4] = [&["build"], &["check"], &["test", "--no-run"], &["doc"]];

/// Race test 2, and H2 (the invariant the protocol rests on, docs/safety.md
/// Reclaim protocol step 3): a Cargo command started while diskgc deletes
/// waits, writes nothing into the profile directory, and succeeds once
/// diskgc is done.
///
/// Rounds of [`started_during_deletion`], each on a fresh target, for each
/// of `build`, `check`, `test --no-run` and `doc` ([`RACE_2_COMMANDS`]):
/// on a target this toolchain built, and (H2) on a target the oldest
/// stable built ([`Toolchain::oldest`], `.cargo-lock` alone), on which a
/// newer Cargo creates a lock file diskgc does not hold. On the oldest
/// itself only the own-target rounds run (no newer Cargo; logged).
pub fn cargo_race_2_a_build_started_during_deletion_waits_writing_nothing(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let oldest = Toolchain::oldest()?;
    let mut makers = vec![(toolchain, "own")];
    if oldest == *toolchain {
        info!(
            "toolchain {} is the oldest stable: the rounds on an older target (H2's cross-version \
             case) are not applicable here (vacuous, not passed)",
            toolchain.name
        );
    } else {
        cargo_under_test::smoke(ctx, &oldest).map_err(|broken| ctx.toolchain_broken(&broken))?;
        makers.push((&oldest, "oldest"));
    }
    for (maker, label) in makers {
        for command in RACE_2_COMMANDS {
            started_during_deletion(
                ctx,
                &Round {
                    rel: format!("{label}-{}", command[0]),
                    maker,
                    builder: toolchain,
                    command,
                    split_build_dir: None,
                },
            )?;
        }
    }
    Ok(())
}

/// M3 (task-11 round 2): a target first built whole keeps all three lock
/// files in its profile directory, so diskgc reclaims it, while a later
/// build may use a separate build directory (`CARGO_BUILD_BUILD_DIR`)
/// for it. Two cases:
///
/// - functional: the whole target reclaimed (no race), then a build, a
///   run and a test in split mode succeed;
/// - race test 2 with a split-mode build started during the deletion: it
///   takes the build directory's `.cargo-build-lock`, which diskgc does not
///   hold (`/proc/locks`), creates no lock file in the target's profile
///   directory, waits for a lock diskgc holds there, and writes nothing
///   into it (the harness's walk); diskgc stops at its next part.
///
/// 1.90.0 ignores the setting (logged not applicable).
pub fn cargo_race_2_split_build_dir_on_a_target_first_built_whole(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    let build_dir = s.path("bd");
    let split = |cargo: Cargo| cargo.env("CARGO_BUILD_BUILD_DIR", build_dir.display().to_string());

    // Functional, no race.
    s.mkdir("whole", 0o755, TESTER)?;
    let p = RACE.write(s, "whole/p")?;
    build(ctx, toolchain, &p, &["build"])?;
    let target = p.join("target");
    let oracle = Oracle::arm(reclaimable(&target)?)?;
    let run = cargo_reclaim(ctx, &[], &["/tmp/whole"])?;
    let judged = oracle.judge(&run)?;
    let got = verdict_of(&run, &target)?;
    ensure!(
        got.starts_with(&format!("reclaimed {target:?} ")),
        "the whole target: {got}"
    );
    judged.assert_clean()?;
    split(Cargo::new(toolchain, &p, &["build"]))
        .output(ctx)?
        .expect_success()?;
    if !build_dir.exists() {
        ensure!(
            *toolchain == Toolchain::oldest()?,
            "toolchain {} made no build directory at {}",
            toolchain.name,
            build_dir.display()
        );
        info!(
            "toolchain {} ignores CARGO_BUILD_BUILD_DIR: a split build on a whole target is not \
             applicable here (vacuous, not passed)",
            toolchain.name
        );
        return Ok(());
    }
    let ran = split(Cargo::new(toolchain, &p, &["run"])).output(ctx)?;
    ran.expect_success()?;
    ensure!(
        ran.stdout.contains(RACE.output),
        "the split-mode binary printed {:?}",
        ran.stdout
    );
    let tested = split(Cargo::new(toolchain, &p, &["test", "--", "--nocapture"])).output(ctx)?;
    tested.expect_success()?;
    ensure!(
        tested
            .stdout
            .contains("diskgc-e2e cargo race fixture test ran"),
        "the split-mode test did not run:\n{}",
        tested.failure()
    );
    info!("split-mode build, run and test succeed on a reclaimed whole target");
    std::fs::remove_dir_all(&build_dir).map_err(|e| format!("rm -r {build_dir:?}: {e}"))?;

    // The race.
    started_during_deletion(
        ctx,
        &Round {
            rel: "split".to_owned(),
            maker: toolchain,
            builder: toolchain,
            command: &["build"],
            split_build_dir: Some(build_dir.clone()),
        },
    )
}

/// One round of race test 2.
struct Round<'t> {
    /// Its scratch directory (created).
    rel: String,
    /// Builds the target (`cargo build`, whole).
    maker: &'t Toolchain,
    /// Runs `command` during the deletion.
    builder: &'t Toolchain,
    command: &'static [&'static str],
    /// The builder runs with this `CARGO_BUILD_BUILD_DIR`.
    split_build_dir: Option<PathBuf>,
}

/// One round of race test 2 (see its test): a target `maker` built, the
/// driver paused at the first unlink of the second-to-last part of step 3
/// ([`planned_parts`]; on the classic layout `build/`, on the per-unit
/// layout the last entry before `build/`), and `builder`'s `command`
/// started then:
///
/// - `/proc/locks`: diskgc holds every lock file it found; Cargo waits for
///   one of them (`.cargo-lock` when an older Cargo made the target) and
///   holds only lock files it created, steadily over [`STEADY_SAMPLES`]
///   samples (a split-mode Cargo also holds the build directory's
///   `.cargo-build-lock` and creates no lock file in the profile);
/// - the harness's walk of the profile directory: nothing changed but the
///   lock files Cargo holds being created (and the directory's times with
///   them): it wrote nothing else before the lock it waits for;
/// - resumed, diskgc finishes the part it was in and stops before the last
///   (strace: nothing of it unlinked): `changed` if Cargo created a lock
///   file (the lock re-listing), otherwise `in_use` (the part's in-use
///   check sees the waiting Cargo);
/// - the oracle (snapshot, strace) finds nothing outside the profile's
///   entries touched, no lock file unlinked or replaced; Cargo waited
///   ("Blocking waiting for file lock"), compiled again (its fingerprints
///   were gone), and a built binary runs.
fn started_during_deletion(ctx: &Ctx, round: &Round<'_>) -> TestResult {
    let what = format!(
        "cargo {} ({}{}) on a target {} made",
        round.command.join(" "),
        round.builder.name,
        if round.split_build_dir.is_some() {
            ", split build directory"
        } else {
            ""
        },
        round.maker.name
    );
    let s = &ctx.scratch;
    s.mkdir(&round.rel, 0o755, TESTER)?;
    let p = RACE.write(s, &format!("{}/p", round.rel))?;
    build(ctx, round.maker, &p, &["build"])?;
    let target = p.join("target");
    let profile = target.join("debug");
    ensure!(
        profile_dirs(&target)? == [profile.clone()],
        "{what}: expected the debug profile alone: {:?}",
        profile_dirs(&target)?
    );
    let found_locks = cargo_under_test::lock_files(&profile)?;
    let locks = lock_inodes(&target)?;
    // Paused at the first unlink of the second-to-last part, so that one
    // part is in progress when Cargo starts and one is left after it.
    let parts = planned_parts(std::slice::from_ref(&profile))?;
    ensure!(parts.len() >= 2, "{what}: too few parts: {parts:?}");
    let in_progress = parts[parts.len() - 2].path.clone();
    let k = first_unlink_of(&parts, parts.len() - 2);
    let total = first_unlink_of(&parts, parts.len()) - 1;
    info!(
        "{what}: lock files {:?}; parts {}; pausing at unlinkat:{k} of {total} (the first of {})",
        found_locks.iter().map(|l| name_of(l)).collect::<Vec<_>>(),
        sizes(&parts),
        in_progress.display(),
    );

    // Cargo writes `.rustc_info.json` next to the profiles when it starts;
    // everything else it may write is a new object or an eligible entry.
    let oracle =
        Oracle::arm(reclaimable(&target)?.concurrent_writer(target.join(".rustc_info.json")))?;
    let root = s.path(&round.rel);
    let (mut spawned, paused) = spawn_paused(ctx, &format!("unlinkat:{k}"), &[path_str(&root)?])?;
    let driver = lock_usage(&profile, paused.pid)?;
    ensure!(
        found_locks.iter().all(|l| driver.holds(l)) && driver.waits.is_empty(),
        "{what}: diskgc paused at unlinkat:{k} does not hold every lock file it found: it {driver}"
    );
    let before = tree_state(&target)?;
    let mut command = Cargo::new(round.builder, &p, round.command);
    if let Some(dir) = &round.split_build_dir {
        command = command.env("CARGO_BUILD_BUILD_DIR", dir.display().to_string());
    }
    let mut cargo = command.spawn(ctx)?;
    let cargo_pid = cargo.pid();
    let blocked = steadily_blocked(&profile, &mut cargo, &what)?;
    let after = tree_state(&target)?;

    // It waits for a lock diskgc holds, and holds only lock files it made.
    let waits_for = blocked.waits.clone();
    ensure!(
        waits_for
            .iter()
            .all(|w| found_locks.contains(w) && driver.holds(w)),
        "{what}: Cargo waits for {waits_for:?}, not for a lock file diskgc holds ({driver})"
    );
    if round.maker != round.builder {
        ensure!(
            waits_for == [profile.join(".cargo-lock")],
            "{what}: H2: Cargo does not wait for .cargo-lock: it {blocked}"
        );
    }
    let created: Vec<PathBuf> = cargo_under_test::lock_files(&profile)?
        .into_iter()
        .filter(|l| !found_locks.contains(l))
        .collect();
    ensure!(
        blocked.holds.iter().all(|(p, _)| created.contains(p)),
        "{what}: Cargo holds a lock file it did not create: it {blocked}"
    );
    ensure!(
        created.iter().all(|c| blocked.holds(c)),
        "{what}: Cargo created lock files {created:?} but holds only {:?}",
        blocked.holds
    );
    for lock in &created {
        ensure!(
            CARGO_LOCK_NAMES.contains(&name_of(lock).as_str()),
            "{what}: Cargo created a lock file the allowlist does not know: {}",
            lock.display()
        );
    }
    if let Some(dir) = &round.split_build_dir {
        ensure!(
            created.is_empty(),
            "{what}: a split-mode Cargo created lock files in the target's profile: {created:?}"
        );
        let build_lock = dir.join("debug/.cargo-build-lock");
        let in_build_dir = lock_usage(&dir.join("debug"), cargo_pid)?;
        ensure!(
            in_build_dir.holds(&build_lock),
            "{what}: the split-mode Cargo does not hold the build directory's \
             .cargo-build-lock: it {in_build_dir}"
        );
        info!("{what}: in the build directory it {in_build_dir}");
    }
    let mut writes = Vec::new();
    for (path, change) in tree_changes(&before, &after) {
        let allowed = if path == profile {
            // Its times move with a created lock file, nothing else.
            !created.is_empty() && change.starts_with("changed")
        } else if path.starts_with(&profile) {
            created.contains(&path) && change.starts_with("created")
        } else {
            info!(
                "{what}: outside the profile directory: {} {change}",
                path.display()
            );
            true
        };
        if !allowed {
            writes.push(format!("{} {change}", path.display()));
        }
    }
    ensure!(
        writes.is_empty(),
        "{what}: H2 FAILS: Cargo wrote into the profile directory while it waited for a lock \
         diskgc holds (it {blocked}):\n{}",
        writes.join("\n")
    );
    info!(
        "{what}: H2 holds here: Cargo created {:?}, holds {:?}, waits for {:?}, and wrote nothing \
         else into {} ({} objects compared)",
        created.iter().map(|c| name_of(c)).collect::<Vec<_>>(),
        blocked.holds,
        waits_for,
        profile.display(),
        before.len()
    );

    spawned.resume(&paused)?;
    let run = spawned.wait()?;
    info!(
        "{what}: driver:\n{}\n--- stderr\n{}",
        run.stdout, run.stderr
    );
    let out = cargo.wait()?;
    let judged = oracle.judge(&run)?;
    let got = verdict_of(&run, &target)?;
    // diskgc finished the part it was in, then stopped before the last one:
    // as `changed` when Cargo created a lock file (the lock re-listing
    // comes first), otherwise as `in_use` (the part's own check sees the
    // waiting Cargo in the project directory, holding a lock file open).
    let (reason, needle) = if created.is_empty() {
        (
            "in_use",
            format!("pid {cargo_pid} (cargo) is a Cargo process working in the project"),
        )
    } else {
        ("changed", "a lock file was created".to_owned())
    };
    ensure!(
        got.starts_with(&format!("skip {reason} {target:?} ")) && got.contains(&needle),
        "{what}: expected diskgc to stop at the last part as {reason} ({needle:?}): {got}"
    );
    unlinks_end_in_part(&run, k, &in_progress, &what)?;
    judged.assert_clean()?;
    let now = lock_inodes(&target)?;
    for (lock, ino) in &locks {
        ensure!(
            now.get(lock) == Some(ino),
            "{what}: {} gone or replaced",
            lock.display()
        );
    }
    out.expect_success()?;
    ensure!(
        out.stderr.contains("Blocking waiting for file lock"),
        "{what}: Cargo did not wait:\n{}",
        out.failure()
    );
    ensure!(
        ["Compiling", "Checking", "Documenting"]
            .iter()
            .any(|word| out.stderr.contains(&format!("{word} {}", RACE.binary))),
        "{what}: Cargo did not compile the package again although its fingerprints were gone:\n{}",
        out.failure()
    );
    if round.command == ["build"] {
        let mut run_it = Cargo::new(round.builder, &p, &["run"]);
        if let Some(dir) = &round.split_build_dir {
            run_it = run_it.env("CARGO_BUILD_BUILD_DIR", dir.display().to_string());
        }
        let ran = run_it.output(ctx)?;
        ran.expect_success()?;
        ensure!(
            ran.stdout.contains(RACE.output),
            "{what}: the rebuilt binary printed {:?}",
            ran.stdout
        );
    }
    Ok(())
}

/// Fails unless the run's unlinks from position `k` on (strace) all lie in
/// `part`, and `part` itself was unlinked: the driver, resumed at `k`,
/// finished the part it was in and unlinked nothing past it.
fn unlinks_end_in_part(run: &Run, k: usize, part: &Path, what: &str) -> TestResult {
    let gone = unlinked(run);
    ensure!(
        gone.len() >= k && gone[k - 1].starts_with(part),
        "{what}: unlink {k} was not in {}: {:?}",
        part.display(),
        gone.get(k - 1)
    );
    ensure!(
        gone.iter().any(|g| g == part),
        "{what}: the part in progress ({}) was not finished",
        part.display()
    );
    let past: Vec<&PathBuf> = gone[k - 1..]
        .iter()
        .filter(|g| !g.starts_with(part))
        .collect();
    ensure!(
        past.is_empty(),
        "{what}: unlinked past the part in progress although Cargo had started: {past:?}"
    );
    Ok(())
}

/// The index of the part of `parts` whose unlinks include position `k`
/// (from 1).
fn part_at(parts: &[PlannedPart], k: usize) -> usize {
    (0..parts.len())
        .rfind(|&i| first_unlink_of(parts, i) <= k)
        .unwrap_or(0)
}

/// A target found after `skipped` ones, for [`no_lock_held_after`]: the
/// race fixture at `rel`, built for debug and release, so it has at least
/// two lock files on every toolchain.
fn later_target(ctx: &Ctx, toolchain: &Toolchain, rel: &str) -> Result<PathBuf, String> {
    let q = if ctx.scratch.path(rel).exists() {
        ctx.scratch.path(rel)
    } else {
        RACE.write(&ctx.scratch, rel)?
    };
    build(ctx, toolchain, &q, &["build"])?;
    build(ctx, toolchain, &q, &["build", "--release"])?;
    Ok(q.join("target"))
}

/// Runs `cargo-reclaim ROOTS` (judged by the caller's oracle, armed
/// before) paused before the run's second lock attempt (`flock_try:2`).
/// In a correct run every `flock_try` is `later`'s, the `skipped` targets
/// being refused before their locks, so the pause falls on `later`'s
/// second lock; a run that tried the locks of a skipped target with two or
/// more lock files is paused inside that target's lock loop, holding its
/// first lock. At the pause `/proc/locks` must show the driver holding
/// nothing of any `skipped` target: the independent sign that it took no
/// lock there. A lock taken and released before the second attempt is not
/// seen (the strace record traces no `flock`). The `skipped` targets must
/// be reported before `later`, or the check says nothing.
fn no_lock_held_after(
    ctx: &Ctx,
    roots: &[&str],
    skipped: &[&Path],
    later: &Path,
    what: &str,
) -> Result<Run, String> {
    let mut later_locks = 0;
    for profile in profile_dirs(later)? {
        later_locks += cargo_under_test::lock_files(&profile)?.len();
    }
    ensure!(
        later_locks >= 2,
        "{what}: the later target has {later_locks} lock files"
    );
    let mut skipped_locks = Vec::new();
    for target in skipped {
        for profile in profile_dirs(target)? {
            skipped_locks.extend(cargo_under_test::lock_files(&profile)?);
        }
    }
    let watched = Watched::files(&skipped_locks)?;
    let (mut spawned, paused) = spawn_paused(ctx, "flock_try:2", roots)?;
    let usage = watched.usage(&proc_locks::sample()?, paused.pid);
    ensure!(
        usage.is_empty(),
        "{what}: paused at flock_try:2, diskgc {usage} on a target it must not lock"
    );
    spawned.resume(&paused)?;
    let run = spawned.wait()?;
    info!(
        "{what}: driver:\n{}\n--- stderr\n{}",
        run.stdout, run.stderr
    );
    let lines = results(&run)?;
    let at = |t: &Path| {
        lines
            .iter()
            .position(|l| l.starts_with(&format!("target {t:?} ")))
            .ok_or_else(|| format!("{what}: {} not found: {lines:#?}", t.display()))
    };
    let later_at = at(later)?;
    for target in skipped {
        ensure!(
            at(target)? < later_at,
            "{what}: {} was reached after {}: the pause shows nothing about it",
            target.display(),
            later.display()
        );
    }
    info!(
        "{what}: paused at the run's second lock attempt (the later target's), diskgc held no \
         lock of {skipped:?}"
    );
    Ok(run)
}

/// `name:objects` of every part, for the log.
fn sizes(parts: &[PlannedPart]) -> String {
    parts
        .iter()
        .map(|p| format!("{}:{}", name_of(&p.path), p.objects))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Race test 3 (docs/safety.md, Reclaim protocol, step 1; lock files
/// never move): a build blocked on a lock diskgc holds during the deletion,
/// and one started after diskgc is done, lock the same lock files (the
/// same inodes, `/proc/locks` by file id) and so never run together. The
/// driver paused at its second unlink; the first build (its build script
/// gated) starts and waits for a lock diskgc holds; resumed, diskgc
/// finishes the part it was in and stops at its next part (`in_use`: the
/// waiting Cargo works in the project; strace: nothing unlinked past the
/// part), then exits, releasing its locks with its process; the first
/// build goes on and pauses in its build script holding every lock file;
/// the second build then waits for one of those very files, over
/// [`STEADY_SAMPLES`] samples in which the first keeps holding all of them;
/// the gate opens and both succeed. The oracle finds no lock file unlinked
/// or replaced. (The release between targets within one run, step 4, is
/// [`cargo_race_locks_held_through_the_last_part_released_per_target`].)
pub fn cargo_race_3_a_blocked_build_and_a_later_one_never_run_together(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    build(ctx, toolchain, &p, &["build"])?;
    let target = p.join("target");
    let profile = target.join("debug");
    let found_locks = cargo_under_test::lock_files(&profile)?;
    let locks = lock_inodes(&target)?;
    let parts = planned_parts(std::slice::from_ref(&profile))?;
    let k = 2;
    let in_progress = parts[part_at(&parts, k)].path.clone();
    let oracle =
        Oracle::arm(reclaimable(&target)?.concurrent_writer(target.join(".rustc_info.json")))?;
    let (mut spawned, paused) = spawn_paused(ctx, &format!("unlinkat:{k}"), &["/tmp/w"])?;
    let driver = lock_usage(&profile, paused.pid)?;
    let gate = Gate::new(s, "first")?;
    let mut first = Cargo::new(toolchain, &p, &["build"])
        .build_gate(&gate)
        .spawn(ctx)?;
    let first_pid = first.pid();
    let blocked = steadily_blocked(&profile, &mut first, "the first build")?;
    ensure!(
        blocked.waits.iter().all(|w| driver.holds(w)),
        "the first build waits for {:?}, which diskgc does not hold ({driver})",
        blocked.waits
    );
    spawned.resume(&paused)?;
    let run = spawned.wait()?;
    info!("driver:\n{}\n--- stderr\n{}", run.stdout, run.stderr);
    let got = verdict_of(&run, &target)?;
    ensure!(
        got.starts_with(&format!("skip in_use {target:?} "))
            && got.contains(&format!("pid {first_pid} (")),
        "diskgc should stop at its next part, seeing the waiting build: {got}"
    );
    unlinks_end_in_part(&run, k, &in_progress, "race test 3")?;

    // diskgc is gone; the first build now holds every lock file, in its
    // build script.
    gate.wait_until_waiting(REACH_TIMEOUT)?;
    ensure!(
        cargo_under_test::lock_files(&profile)? == found_locks,
        "the lock files changed: {:?}, found {found_locks:?}",
        cargo_under_test::lock_files(&profile)?
    );
    let watched = Watched::files(&found_locks)?;
    watched.wait_for(first_pid, REACH_TIMEOUT, "holding every lock file", |u| {
        found_locks.iter().all(|f| u.holds(f))
    })?;
    let mut second = Cargo::new(toolchain, &p, &["build"]).spawn(ctx)?;
    let second_pid = second.pid();
    watched.wait_for(second_pid, REACH_TIMEOUT, "waiting for a lock file", |u| {
        !u.waits.is_empty()
    })?;
    for n in 0..STEADY_SAMPLES {
        let sample = proc_locks::sample()?;
        let a = watched.usage(&sample, first_pid);
        let b = watched.usage(&sample, second_pid);
        ensure!(
            found_locks.iter().all(|f| a.holds(f))
                && !b.waits.is_empty()
                && b.waits.iter().all(|w| a.holds(w))
                && b.holds.iter().all(|(f, _)| !a.holds(f)),
            "sample {n}: the builds overlap or the second is not waiting behind the first: \
             first {a}; second {b}"
        );
        ensure!(second.is_running()?, "the second build ended while blocked");
        std::thread::sleep(SAMPLE_GAP);
    }
    info!(
        "over {STEADY_SAMPLES} samples the first build held every lock file and the second waited \
         for one of them"
    );
    gate.open(s)?;
    first.wait()?.expect_success()?;
    let out = second.wait()?;
    out.expect_success()?;
    ensure!(
        out.stderr.contains("Blocking waiting for file lock"),
        "the second build did not wait:\n{}",
        out.failure()
    );
    oracle.judge(&run)?.assert_clean()?;
    ensure!(
        lock_inodes(&target)? == locks,
        "lock files gone or replaced"
    );
    Ok(())
}

/// Race test 4 (docs/safety.md, Reclaim protocol step 2, the in-use check
/// under the locks, or the per-part check: the two are indistinguishable
/// from outside; In use: an executable inside the target): `cargo test`
/// whose test binary still runs (paused at the fixture test's gate; on the
/// nightly it runs from `build/<package>/<hash>/out/`). `/proc/locks`: no
/// process holds any lock file of the target, so only the process checks
/// can protect it; the driver skips it as `in_use`, naming the test
/// binary, and deletes nothing (oracle with nothing eligible, no unlink in
/// the strace record); the test then finishes and passes.
pub fn cargo_race_4_a_target_whose_test_binary_still_runs_is_skipped(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    build(ctx, toolchain, &p, &["build"])?;
    build(ctx, toolchain, &p, &["test", "--no-run"])?;
    let target = p.join("target");
    let watched = watch_target_locks(&target)?;
    let gate = Gate::new(s, "test-binary")?;
    let cargo = Cargo::new(toolchain, &p, &["test", "--", "--nocapture"])
        .test_gate(&gate)
        .spawn(ctx)?;
    let test_binary = gate.wait_until_waiting(REACH_TIMEOUT)?;
    let exe = std::fs::read_link(format!("/proc/{test_binary}/exe"))
        .map_err(|e| format!("readlink /proc/{test_binary}/exe: {e}"))?;
    ensure!(
        exe.starts_with(&target),
        "the waiting test (pid {test_binary}) runs {}, not a binary of the target",
        exe.display()
    );
    let sample = proc_locks::sample()?;
    ensure!(
        watched.locks_on(&sample).is_empty(),
        "a lock is held on a lock file while the test binary runs: {:?}",
        watched.locks_on(&sample)
    );
    let run = reclaim_touching_nothing(ctx, &["/tmp/w"])?;
    let got = verdict_of(&run, &target)?;
    expect_skip(
        &got,
        "in_use",
        &target,
        &[(test_binary, "runs an executable inside the target")],
    )?;
    gate.open(s)?;
    let out = cargo.wait()?;
    out.expect_success()?;
    ensure!(
        out.stdout
            .contains("diskgc-e2e cargo race fixture test ran"),
        "the test did not run to its end:\n{}",
        out.failure()
    );
    Ok(())
}

/// Where in its part a seeded kill lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Within {
    /// Before the part's first unlink: the parts before it are done.
    First,
    /// Before the unlink in its middle: the part is half deleted.
    Middle,
    /// Before its last unlink (the part's own directory).
    Last,
}

/// The seeded kill positions of race test 6, by part (named by path, so a
/// rebuild between kills, which changes counts inside parts, keeps them):
/// the first unlink of every part (each boundary between parts, the very
/// first unlink included), the middle of every part of at least three
/// objects (inside `.fingerprint/`, `incremental/`, `build/`, `deps/`),
/// and the last unlink of the run.
fn seeds(parts: &[PlannedPart]) -> Vec<(PathBuf, Within)> {
    let mut seeds = Vec::new();
    for part in parts {
        seeds.push((part.path.clone(), Within::First));
        if part.objects >= 3 {
            seeds.push((part.path.clone(), Within::Middle));
        }
    }
    if let Some(last) = parts.last().filter(|l| l.objects >= 2) {
        seeds.push((last.path.clone(), Within::Last));
    }
    seeds
}

/// What a run of race test 6 kills and rebuilds with.
struct Kills<'t> {
    label: &'t str,
    /// `cargo build` arguments of each profile (`[]`, `["--release"]`).
    profiles: &'t [&'t [&'t str]],
    /// Rebuild the target with these, in order, before every kill (the
    /// mixed layout needs both to be mixed again); empty when the recovery
    /// build already restores it.
    restore_with: &'t [&'t Toolchain],
    /// Each kill is made once per recovery builder, which alone rebuilds
    /// after it, so each recovers from the same interrupted state.
    recover_with: &'t [&'t Toolchain],
    /// Seeds that must be among the planned ones (part name, where in it).
    must_include: &'t [(&'t str, Within)],
}

/// Race test 6 on the target of `project`: for every seed ([`seeds`]) and
/// every recovery builder, the target is restored, the driver is SIGKILLed
/// before that unlink (`--kill-at unlinkat:K`, K computed from the
/// harness's walk), then:
///
/// - the oracle finds nothing but the profiles' entries touched, the lock
///   files keep their inodes, and `/proc/locks` shows no lock left on any
///   of them;
/// - exactly K-1 objects were unlinked (strace), and the tree shows the
///   protocol's order: every part before the seed's is gone, every part
///   after it whole, the seed's part missing exactly the objects before K
///   (fingerprints of every profile first, `build/` and `deps/` last);
/// - every profile directory is still in a known layout;
/// - the next build of every profile, with the recovery builder alone,
///   succeeds and its binary runs; once the kill came after the last
///   fingerprint part, it compiled the package again.
fn kill_at_seeds(ctx: &Ctx, project: &Path, kills: &Kills<'_>) -> TestResult {
    let label = kills.label;
    let target = project.join("target");
    let root = project
        .parent()
        .ok_or("the project has no parent")?
        .to_owned();
    let dirs = profile_dirs(&target)?;
    let planned = seeds(&planned_parts(&dirs)?);
    for (name, within) in kills.must_include {
        ensure!(
            planned
                .iter()
                .any(|(path, w)| name_of(path) == *name && w == within),
            "{label}: no seeded kill {within:?} of a part named {name}: {planned:?}"
        );
    }
    info!(
        "{label}: {} seeded kill positions, each for {} recovery builder(s)",
        planned.len(),
        kills.recover_with.len()
    );
    let mut done = Vec::new();
    for ((part_path, within), recovery) in planned
        .iter()
        .flat_map(|seed| kills.recover_with.iter().map(move |r| (seed, *r)))
    {
        for toolchain in kills.restore_with {
            for args in kills.profiles {
                let mut with = vec!["build"];
                with.extend_from_slice(args);
                build(ctx, toolchain, project, &with)?;
            }
        }
        ensure!(
            profile_dirs(&target)? == dirs,
            "{label}: the profile directories changed: {:?}",
            profile_dirs(&target)?
        );
        let parts = planned_parts(&dirs)?;
        let i = parts
            .iter()
            .position(|p| &p.path == part_path)
            .ok_or_else(|| format!("{label}: no part {} after a rebuild", part_path.display()))?;
        let sets = part_objects(&parts)?;
        let j = match within {
            Within::First => 1,
            Within::Middle => parts[i].objects / 2 + 1,
            Within::Last => parts[i].objects,
        };
        let k = first_unlink_of(&parts, i) + j - 1;
        let total = first_unlink_of(&parts, parts.len()) - 1;
        let at = format!("unlinkat:{k}");
        let locks = lock_inodes(&target)?;
        let oracle = Oracle::arm(reclaimable(&target)?)?;
        let run = cargo_reclaim(ctx, &["--kill-at", &at], &[path_str(&root)?])?;
        let judged = oracle.judge(&run)?;
        let case = format!(
            "{label}: killed at {at} of {total} ({within:?} of {}), recovered by {}",
            part_path.display(),
            recovery.name
        );
        info!("{case}: parts {}", sizes(&parts));
        ensure!(
            run.signal == Some(SIGKILL),
            "{case}: the driver was not killed: {:?} {:?}",
            run.code,
            run.signal
        );
        judged.assert_clean().map_err(|e| format!("{case}: {e}"))?;
        ensure!(
            lock_inodes(&target)? == locks,
            "{case}: lock files gone or replaced"
        );
        // The kernel released the killed driver's locks with its
        // descriptors: nothing is left for the next build to wait for.
        let sample = proc_locks::sample()?;
        let left = watch_target_locks(&target)?;
        ensure!(
            left.locks_on(&sample).is_empty(),
            "{case}: locks remain on the lock files after the kill: {:?}",
            left.locks_on(&sample)
        );
        let gone = unlinked(&run);
        ensure!(
            gone.len() == k - 1,
            "{case}: {} unlinks before the kill, want {}",
            gone.len(),
            k - 1
        );
        for (n, (part, objects)) in parts.iter().zip(&sets).enumerate() {
            let left = objects
                .iter()
                .filter(|o| std::fs::symlink_metadata(o).is_ok())
                .count();
            let want = match n.cmp(&i) {
                std::cmp::Ordering::Less => 0,
                std::cmp::Ordering::Equal => objects.len() - (j - 1),
                std::cmp::Ordering::Greater => objects.len(),
            };
            ensure!(
                left == want,
                "{case}: part {} ({}) has {left} of its {} objects left, want {want}: not the \
                 protocol's order",
                n,
                part.path.display(),
                objects.len()
            );
        }
        for profile in &dirs {
            let layout = layout_of(profile)?;
            ensure!(
                layout != "unknown",
                "{case}: {} is in no known layout: {:?}",
                profile.display(),
                deletable(profile)?
            );
        }
        let last_fingerprint = parts.iter().rposition(|p| p.fingerprint);
        let recompiles = last_fingerprint.is_some_and(|f| i > f);
        for args in kills.profiles {
            rebuilds(ctx, recovery, project, args, recompiles, &case)?;
        }
        done.push(format!(
            "{at}({within:?} {}, {})",
            name_of(part_path),
            recovery.name
        ));
    }
    info!(
        "{label}: killed at {} seeded positions, each time the next builds succeeded: {}",
        done.len(),
        done.join(" ")
    );
    Ok(())
}

/// Race test 6 (docs/safety.md, Reclaim protocol step 3, the order; PRD
/// weak assumption: half-deleted `incremental/`): diskgc SIGKILLed during
/// the deletion of a target with a debug and a release profile that this
/// toolchain built, at every seeded position ([`seeds`], [`kill_at_seeds`]):
/// the next `cargo build` always succeeds.
pub fn cargo_race_6_killed_at_seeded_positions_the_next_build_succeeds(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    let profiles: [&[&str]; 2] = [&[], &["--release"]];
    for args in profiles {
        let mut with = vec!["build"];
        with.extend_from_slice(args);
        build(ctx, toolchain, &p, &with)?;
    }
    let first_fingerprint = if planned_parts(&profile_dirs(&p.join("target"))?)?
        .iter()
        .any(|part| name_of(&part.path) == ".fingerprint")
    {
        ".fingerprint"
    } else {
        "fingerprint"
    };
    kill_at_seeds(
        ctx,
        &p,
        &Kills {
            label: &toolchain.name,
            profiles: &profiles,
            restore_with: &[],
            recover_with: &[toolchain],
            must_include: &[
                ("incremental", Within::Middle),
                (first_fingerprint, Within::Middle),
            ],
        },
    )
}

/// Race test 6 on a mixed layout (H1; docs/safety.md, Layouts and Reclaim
/// protocol step 3): a debug profile that the fast stable toolchain
/// (1.99.1) and then the nightly built holds `.fingerprint/` and `deps/` and per-unit units in `build/`;
/// both kinds of fingerprints go first. Every seeded position is killed
/// twice, once per recovery builder, the target made mixed again (built by
/// both) before each kill, so each builder recovers alone from the same
/// interrupted state; its next build succeeds (and compiles again once
/// every fingerprint of both kinds was gone). The seeds include kills
/// inside `.fingerprint/`, inside a per-unit fingerprint and inside
/// `incremental/`.
pub fn cargo_race_6_a_mixed_layout_target_killed_at_seeded_positions(ctx: &Ctx) -> TestResult {
    let stable = Toolchain::fast()?;
    let nightly = Toolchain::all()?
        .into_iter()
        .find(|t| t.name.starts_with("nightly"))
        .ok_or("no nightly toolchain under test")?;
    for toolchain in [&stable, &nightly] {
        cargo_under_test::smoke(ctx, toolchain).map_err(|broken| ctx.toolchain_broken(&broken))?;
    }
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    build(ctx, &stable, &p, &["build"])?;
    build(ctx, &nightly, &p, &["build"])?;
    let profile = p.join("target/debug");
    let fingerprints_of = |per_unit: bool| -> Result<usize, String> {
        Ok(planned_parts(std::slice::from_ref(&profile))?
            .iter()
            .filter(|part| part.fingerprint && (name_of(&part.path) == "fingerprint") == per_unit)
            .count())
    };
    ensure!(
        fingerprints_of(false)? == 1 && fingerprints_of(true)? > 0,
        "not a mixed layout: {:?}",
        planned_parts(std::slice::from_ref(&profile))?
    );
    let builders = [&stable, &nightly];
    let label = format!("{} then {}", stable.name, nightly.name);
    kill_at_seeds(
        ctx,
        &p,
        &Kills {
            label: &label,
            profiles: &[&[]],
            restore_with: &builders,
            recover_with: &builders,
            must_include: &[
                ("incremental", Within::Middle),
                (".fingerprint", Within::Middle),
                ("fingerprint", Within::Middle),
            ],
        },
    )
}

/// Race test 7 (docs/safety.md, Reclaim protocol step 1: every lock, in
/// Cargo's order, without waiting): a `cargo check` paused after taking
/// `.cargo-build-lock` and before `.cargo-lock` (the lock holder holds
/// `.cargo-lock` exclusively), plus a concurrent `cargo build` waiting
/// behind it. `/proc/locks` shows both states, steadily; the driver skips
/// the target as `locked` at its first lock, `.cargo-build-lock`, with no
/// lock taken (`held_us=none`), unlinks nothing (strace) and touches
/// nothing (oracle); the states are unchanged after it; then both commands
/// succeed and no lock file was unlinked or replaced. On 1.90.0, which has
/// no `.cargo-build-lock`, the check waits for `.cargo-lock` holding
/// nothing (the "between its locks" state is not applicable there; logged).
///
/// Then the order itself, where the toolchain has a `.cargo-build-lock`:
/// a holder stands in for a Cargo that holds `.cargo-build-lock` and has
/// not yet taken `.cargo-lock`; a second target follows in discovery, and
/// the driver is paused at `flock_try:2`. In Cargo's order that call is the
/// second target's first, and `/proc/locks` shows the driver holding no
/// lock of the first target; a driver that took `.cargo-lock` first would
/// be paused holding it (a Cargo starting then would wait for diskgc).
pub fn cargo_race_7_a_check_between_its_locks_and_a_build_keep_the_target(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    build(ctx, toolchain, &p, &["build"])?;
    let target = p.join("target");
    let profile = target.join("debug");
    let found_locks = cargo_under_test::lock_files(&profile)?;
    let locks = lock_inodes(&target)?;
    let cargo_lock = profile.join(".cargo-lock");
    let build_lock = profile.join(".cargo-build-lock");
    let has_build_lock = found_locks.contains(&build_lock);
    let holder = Holder::tester(ctx, &["flock-hold", path_str(&cargo_lock)?], &[])?;
    let mut check = Cargo::new(toolchain, &p, &["check"]).spawn(ctx)?;
    let check_pid = check.pid();
    let checking = steadily_blocked(&profile, &mut check, "cargo check")?;
    ensure!(
        checking.waits == [cargo_lock.clone()],
        "cargo check does not wait for .cargo-lock: it {checking}"
    );
    if has_build_lock {
        ensure!(
            checking.holds == [(build_lock.clone(), "WRITE".to_owned())],
            "cargo check is not between its locks (holding .cargo-build-lock alone): it {checking}"
        );
    } else {
        info!(
            "toolchain {} has no .cargo-build-lock: a check between its locks is not applicable \
             here (vacuous, not passed); the check waits for .cargo-lock holding nothing",
            toolchain.name
        );
    }
    let mut concurrent = Cargo::new(toolchain, &p, &["build"]).spawn(ctx)?;
    let concurrent_pid = concurrent.pid();
    let building = steadily_blocked(&profile, &mut concurrent, "the concurrent build")?;
    let first_lock = if has_build_lock {
        &build_lock
    } else {
        &cargo_lock
    };
    ensure!(
        building.waits == [first_lock.clone()] && building.holds.is_empty(),
        "the concurrent build does not wait for {} holding nothing: it {building}",
        first_lock.display()
    );

    let later = later_target(ctx, toolchain, "w/q")?;
    let oracle = Oracle::arm(reclaimable(&later)?)?;
    let run = no_lock_held_after(ctx, &["/tmp/w"], &[&target], &later, "race test 7")?;
    let judged = oracle.judge(&run)?;
    let got = verdict_of(&run, &target)?;
    ensure!(
        got.starts_with(&format!(
            "skip locked {target:?} entries=0 held_us=none -- {}: ",
            first_lock.display()
        )),
        "expected locked at {} with no lock taken: {got}",
        first_lock.display()
    );
    ensure!(
        unlinked(&run).iter().all(|u| u.starts_with(&later)),
        "unlinked in the locked target: {:?}",
        unlinked(&run)
    );
    judged.assert_clean()?;
    ensure!(
        lock_usage(&profile, check_pid)? == checking
            && lock_usage(&profile, concurrent_pid)? == building,
        "the Cargo commands' locks changed across the driver's run"
    );
    holder.finish()?;
    check.wait()?.expect_success()?;
    concurrent.wait()?.expect_success()?;
    ensure!(
        lock_inodes(&target)? == locks,
        "lock files gone or replaced"
    );

    if !has_build_lock {
        info!(
            "toolchain {}: the lock order check needs a .cargo-build-lock: not applicable here \
             (vacuous, not passed)",
            toolchain.name
        );
        return Ok(());
    }
    let second = later_target(ctx, toolchain, "w/q")?;
    let q = s.path("w/q");
    let holder = Holder::tester(ctx, &["flock-hold", path_str(&build_lock)?], &[])?;
    let oracle = Oracle::arm(reclaimable(&second)?)?;
    let (mut spawned, paused) = spawn_paused(ctx, "flock_try:2", &["/tmp/w"])?;
    let driver = lock_usage(&profile, paused.pid)?;
    ensure!(
        driver.is_empty(),
        "paused at its second lock attempt, diskgc holds or waits for a lock of the first \
         target, whose .cargo-build-lock a Cargo holds: it {driver} (not Cargo's lock order)"
    );
    spawned.resume(&paused)?;
    let run = spawned.wait()?;
    info!("driver:\n{}\n--- stderr\n{}", run.stdout, run.stderr);
    let judged = oracle.judge(&run)?;
    let lines = results(&run)?;
    run.expect_code(0)?;
    let first_at = lines
        .iter()
        .position(|l| l.starts_with(&format!("target {target:?} ")));
    let second_at = lines
        .iter()
        .position(|l| l.starts_with(&format!("target {second:?} ")));
    ensure!(
        first_at.is_some() && second_at.is_some() && first_at < second_at,
        "the targets were not found in the order the check needs: {lines:#?}"
    );
    let got = verdict(&lines, &target)?;
    ensure!(
        got.starts_with(&format!(
            "skip locked {target:?} entries=0 held_us=none -- {}: ",
            build_lock.display()
        )),
        "expected locked at .cargo-build-lock with no lock taken: {got}"
    );
    let got = verdict(&lines, &second)?;
    ensure!(
        got.starts_with(&format!("reclaimed {second:?} ")),
        "the second target: {got}"
    );
    judged.assert_clean()?;
    holder.finish()?;
    build(ctx, toolchain, &p, &["build"])?;
    rebuilds(ctx, toolchain, &q, &[], true, "the second target")
}

/// Race test 8 (docs/safety.md, Reclaim protocol step 2, the scan made
/// under the locks, and the per-part check; In use: `CARGO_MANIFEST_DIR`
/// of the project, the parent Cargo): `cargo run` and `cargo test` on an
/// old, up-to-date target, paused between compiling and running (the
/// runner wrapper waits; `/proc/locks`: no lock is held on any lock file,
/// so only the process checks protect it). The driver skips the target as
/// `in_use`, deletes nothing (oracle with nothing eligible, no unlink),
/// and the binary then runs. Also with the run started while the driver,
/// which had judged the target eligible (`--after-eligibility`), is
/// paused before its first lock: the scan under the locks sees it
/// (`in_use`), or, where the harness's walk shows that `cargo run` wrote
/// into a profile directory meanwhile, the recheck finds a newer last
/// build first (`changed`); either way nothing is deleted. That late case
/// is retried up to [`LATE_ROUNDS`] times until a round reaches the
/// in-use check, and warns (without failing) when none did: conditional
/// evidence, as is the pause's length, which it logs ([`late_run`]).
pub fn cargo_race_8_run_or_test_paused_before_its_binary_keeps_the_target(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    build(ctx, toolchain, &p, &["build"])?;
    build(ctx, toolchain, &p, &["test", "--no-run"])?;
    let target = p.join("target");
    let watched = watch_target_locks(&target)?;
    let manifest = "was started by Cargo for a package of the project directory";
    let no_locks = |case: &str| -> TestResult {
        let sample = proc_locks::sample()?;
        ensure!(
            watched.locks_on(&sample).is_empty(),
            "{case}: a lock is held on a lock file: {:?}",
            watched.locks_on(&sample)
        );
        Ok(())
    };
    let cases: [(&[&str], bool, &str); 2] = [
        (&["run"], true, RACE.output),
        (
            &["test", "--", "--nocapture"],
            false,
            "diskgc-e2e cargo race fixture test ran",
        ),
    ];
    for (command, execs, output) in cases {
        let case = format!("cargo {} paused in its runner", command[0]);
        let gate = Gate::new(s, &format!("runner-{}", command[0]))?;
        let cargo = Cargo::new(toolchain, &p, command)
            .runner_gate(ctx, &gate)
            .spawn(ctx)?;
        let runner = gate.wait_until_waiting(REACH_TIMEOUT)?;
        no_locks(&case)?;
        log_cargo_env(&case, runner)?;
        let oracle = Oracle::arm(crate::oracle::Fixture::new())?;
        let run = cargo_reclaim(ctx, &[], &["/tmp/w"])?;
        let judged = oracle.judge(&run)?;
        let got = verdict_of(&run, &target)?;
        let mut needles = vec![(runner, manifest)];
        if !execs {
            needles.push((
                cargo.pid(),
                "is a Cargo process working in the project directory",
            ));
        }
        expect_skip(&got, "in_use", &target, &needles).map_err(|e| format!("{case}: {e}"))?;
        ensure!(
            unlinked(&run).is_empty(),
            "{case}: unlinked {:?}",
            unlinked(&run)
        );
        judged.assert_clean()?;
        gate.open(s)?;
        let out = cargo.wait()?;
        out.expect_success()?;
        ensure!(
            out.stdout.contains(output),
            "{case}: the binary did not run:\n{}",
            out.failure()
        );
        info!("{case}: in use, nothing deleted, the binary ran");
    }

    // Started between judging and locking: up to LATE_ROUNDS rounds, until
    // one reaches the in-use check under the locks (a round where `cargo
    // run` wrote into the profile ends `changed` before it).
    let mut exercised = false;
    for round in 1..=LATE_ROUNDS {
        let case = format!("cargo run started between judging and locking (round {round})");
        if late_run(ctx, toolchain, &p, &target, &case, round)? {
            exercised = true;
            break;
        }
    }
    if !exercised {
        info!(
            "WARNING: in {LATE_ROUNDS} rounds no late cargo run reached the in-use check under \
             the locks within the reuse limit (each wrote into the profile first and was \
             `changed`, or the driver was paused {} ms or longer): the scan made under the \
             locks was not exercised on toolchain {}",
            diskgc_core::inuse::REUSE_LIMIT.as_millis(),
            toolchain.name
        );
    }
    Ok(())
}

/// Rounds race test 8's late case may take to reach the in-use check
/// under the locks.
const LATE_ROUNDS: usize = 5;

/// One round of race test 8's late case: the driver, having judged the
/// target eligible (`--after-eligibility`), paused before its first lock;
/// a `cargo run` started then and paused in its runner (no lock held).
/// Resumed, the driver skips the target, deletes nothing (oracle, strace),
/// and the binary then runs. The verdict expected follows from the
/// harness's walk: `in_use` by the runner's `CARGO_MANIFEST_DIR` (the scan
/// made under the locks), or `changed` when `cargo run` wrote into a
/// profile directory meanwhile (1.90.0 rewrites `<bin>.d` now and then),
/// whose newer last build the recheck finds first. Returns whether the
/// round reached the in-use check with the driver paused for less than
/// the reuse limit (only such a round counts). Logs how long the driver
/// was paused: from the reuse limit on ([`diskgc_core::inuse::REUSE_LIMIT`])
/// a scan kept from before the locks would have been renewed anyway, so
/// the round tells nothing about it.
fn late_run(
    ctx: &Ctx,
    toolchain: &Toolchain,
    p: &Path,
    target: &Path,
    case: &str,
    round: usize,
) -> Result<bool, String> {
    let s = &ctx.scratch;
    let manifest = "was started by Cargo for a package of the project directory";
    let watched = watch_target_locks(target)?;
    let oracle = Oracle::arm(crate::oracle::Fixture::new())?;
    let (mut spawned, paused) =
        spawn_paused(ctx, "flock_try:1", &["--after-eligibility", "/tmp/w"])?;
    let paused_at = std::time::Instant::now();
    let before = tree_state(target)?;
    let gate = Gate::new(s, &format!("runner-late-{round}"))?;
    let cargo = Cargo::new(toolchain, p, &["run"])
        .runner_gate(ctx, &gate)
        .spawn(ctx)?;
    let runner = gate.wait_until_waiting(REACH_TIMEOUT)?;
    let sample = proc_locks::sample()?;
    ensure!(
        watched.locks_on(&sample).is_empty(),
        "{case}: a lock is held on a lock file: {:?}",
        watched.locks_on(&sample)
    );
    let profiles = profile_dirs(target)?;
    let written: Vec<(PathBuf, String)> = tree_changes(&before, &tree_state(target)?)
        .into_iter()
        .filter(|(path, _)| profiles.iter().any(|p| path.starts_with(p)))
        .collect();
    let pause = paused_at.elapsed();
    spawned.resume(&paused)?;
    let run = spawned.wait()?;
    info!(
        "{case}: driver:\n{}\n--- stderr\n{}",
        run.stdout, run.stderr
    );
    let limit = diskgc_core::inuse::REUSE_LIMIT;
    info!(
        "{case}: the driver was paused {:.0} ms between judging and locking{}",
        pause.as_secs_f64() * 1e3,
        if pause >= limit {
            format!(
                " (at or over the {} ms reuse limit: a kept scan would have been renewed anyway, \
                 so this round says nothing about the scan under the locks)",
                limit.as_millis()
            )
        } else {
            String::new()
        }
    );
    let judged = oracle.judge(&run)?;
    let got = verdict_of(&run, target)?;
    // A round that ran into the in-use check under the locks shows that
    // scan was made there only if the driver's pause was shorter than the
    // reuse limit (a longer pause renews any kept scan anyway). The scan
    // from the eligibility check is older than the pause by the time
    // between that scan and the pause, so `pause < REUSE_LIMIT` is
    // necessary for such a round to tell anything, not sufficient.
    let reached_the_check = written.is_empty();
    let exercised = reached_the_check && pause < diskgc_core::inuse::REUSE_LIMIT;
    if reached_the_check {
        expect_skip(&got, "in_use", target, &[(runner, manifest)])
            .map_err(|e| format!("{case}: {e}"))?;
    } else {
        info!("{case}: cargo run wrote into the profile directories: {written:?}");
        ensure!(
            got.starts_with(&format!("skip changed {target:?} entries=0 "))
                && got.contains("built since it was judged"),
            "{case}: cargo run wrote into the target ({written:?}), so diskgc should find it \
             changed: {got}"
        );
    }
    ensure!(
        unlinked(&run).is_empty(),
        "{case}: unlinked {:?}",
        unlinked(&run)
    );
    judged.assert_clean()?;
    gate.open(s)?;
    let out = cargo.wait()?;
    out.expect_success()?;
    ensure!(
        out.stdout.contains(RACE.output),
        "{case}: the binary did not run:\n{}",
        out.failure()
    );
    Ok(exercised)
}

/// Logs what Cargo gave the process `pid` it started (docs/safety.md,
/// Lock files: what Cargo gives the programs it runs).
fn log_cargo_env(case: &str, pid: u32) -> TestResult {
    let path = format!("/proc/{pid}/environ");
    let raw = std::fs::read(&path).map_err(|e| format!("read {path}: {e}"))?;
    let vars: Vec<String> = raw
        .split(|b| *b == 0)
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .filter(|v| {
            [
                "CARGO_MANIFEST_DIR=",
                "CARGO=",
                "LD_LIBRARY_PATH=",
                "OUT_DIR=",
            ]
            .iter()
            .any(|k| v.starts_with(k))
        })
        .collect();
    info!("{case}: the runner (pid {pid}) has {vars:?}");
    Ok(())
}

/// Race test 9 (docs/safety.md, Lock files: the allowlist; Reclaim
/// protocol step 1, the layout judged before any lock): a target real
/// builds made, with an unknown `.cargo-future-lock` next to its lock
/// files, is skipped as `unsupported_layout` without a lock taken. With
/// `.cargo-lock` held exclusively by the lock holder (`/proc/locks`), the
/// verdict is still `unsupported_layout`, not `locked`: diskgc did not try
/// a lock; `held_us=none`; nothing unlinked or touched (strace, oracle).
/// The same without the holder; the unknown file and every lock file stay,
/// and the next build succeeds.
pub fn cargo_race_9_an_unknown_lock_file_is_unsupported_layout(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    build(ctx, toolchain, &p, &["build"])?;
    let target = p.join("target");
    let profile = target.join("debug");
    s.write_file("w/p/target/debug/.cargo-future-lock", b"", 0o644, TESTER)?;
    let locks = lock_inodes(&target)?;
    let cargo_lock = profile.join(".cargo-lock");
    for held in [true, false] {
        let case = if held {
            ".cargo-lock held by another process"
        } else {
            "no lock held"
        };
        let holder = if held {
            Some(Holder::tester(
                ctx,
                &["flock-hold", path_str(&cargo_lock)?],
                &[],
            )?)
        } else {
            None
        };
        let later = later_target(ctx, toolchain, "w/q")?;
        let oracle = Oracle::arm(reclaimable(&later)?)?;
        let run = no_lock_held_after(ctx, &["/tmp/w"], &[&target], &later, case)?;
        let judged = oracle.judge(&run)?;
        let got = verdict_of(&run, &target)?;
        ensure!(
            got.starts_with(&format!(
                "skip unsupported_layout {target:?} entries=0 held_us=none "
            )) && got.contains(".cargo-future-lock"),
            "{case}: expected unsupported_layout naming .cargo-future-lock: {got}"
        );
        ensure!(
            unlinked(&run).iter().all(|u| u.starts_with(&later)),
            "{case}: unlinked in the unsupported target: {:?}",
            unlinked(&run)
        );
        judged.assert_clean()?;
        if let Some(holder) = holder {
            let usage = lock_usage(&profile, holder.pid())?;
            ensure!(
                usage.holds == [(cargo_lock.clone(), "WRITE".to_owned())],
                "{case}: the holder {usage}"
            );
            holder.finish()?;
        }
    }
    ensure!(
        lock_inodes(&target)? == locks,
        "lock files gone or replaced"
    );
    build(ctx, toolchain, &p, &["build"])
}

/// AC #4, separate build directory (docs/safety.md, Lock sets: neither
/// half of a split pair is reclaimed): a target built with
/// `CARGO_BUILD_BUILD_DIR` outside the project, given to the driver with
/// both halves as roots, idle; while a build (its build script gated)
/// holds the locks of both halves (`/proc/locks`); and while `cargo test`
/// is paused in its runner (its test binaries run from the build
/// directory). Each time both halves are `unsupported_layout` with no lock
/// taken, nothing is unlinked or touched, and the Cargo commands succeed.
/// 1.90.0 ignores the setting: there the case is not applicable (logged;
/// checked that no build directory exists).
pub fn cargo_race_split_build_dir_is_never_reclaimed(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    let build_dir = s.path("bd");
    let bd = path_str(&build_dir)?.to_owned();
    let cargo =
        |args: &[&str]| Cargo::new(toolchain, &p, args).env("CARGO_BUILD_BUILD_DIR", bd.clone());
    cargo(&["build"]).output(ctx)?.expect_success()?;
    cargo(&["test", "--no-run"]).output(ctx)?.expect_success()?;
    if !build_dir.exists() {
        let oldest = Toolchain::oldest()?;
        ensure!(
            *toolchain == oldest,
            "toolchain {} made no build directory at {} (measured: only {} ignores \
             CARGO_BUILD_BUILD_DIR, docs/safety.md Lock files)",
            toolchain.name,
            build_dir.display(),
            oldest.name
        );
        info!(
            "toolchain {} ignores CARGO_BUILD_BUILD_DIR: the split build directory case is not \
             applicable here (vacuous, not passed)",
            toolchain.name
        );
        return Ok(());
    }
    let target = p.join("target");
    let halves = [target.clone(), build_dir.clone()];
    s.mkdir("z", 0o755, TESTER)?;
    let expect_unsupported = |case: &str| -> TestResult {
        // A whole target found after both halves (its own root, given
        // last), so a pause at its last lock shows none of theirs held.
        let later = later_target(ctx, toolchain, "z/q")?;
        let oracle = Oracle::arm(reclaimable(&later)?)?;
        let run = no_lock_held_after(
            ctx,
            &["/tmp/w", "/tmp/bd", "/tmp/z"],
            &[&target, &build_dir],
            &later,
            case,
        )?;
        let judged = oracle.judge(&run)?;
        for half in &halves {
            let got = verdict_of(&run, half)?;
            ensure!(
                got.starts_with(&format!(
                    "skip unsupported_layout {half:?} entries=0 held_us=none "
                )) && got.contains("half of a split build directory"),
                "{case}: {}: {got}",
                half.display()
            );
        }
        ensure!(
            unlinked(&run).iter().all(|u| u.starts_with(&later)),
            "{case}: unlinked in a half: {:?}",
            unlinked(&run)
        );
        judged.assert_clean()?;
        info!("{case}: both halves unsupported_layout, nothing deleted");
        Ok(())
    };
    expect_unsupported("idle")?;

    let gate = Gate::new(s, "split-build")?;
    let main = p.join("src/main.rs");
    let text = std::fs::read_to_string(&main).map_err(|e| e.to_string())?;
    std::fs::write(&main, format!("{text}\n// edit\n")).map_err(|e| e.to_string())?;
    let mut building = cargo(&["build"]).build_gate(&gate).spawn(ctx)?;
    gate.wait_until_waiting(REACH_TIMEOUT)?;
    let mut held = Vec::new();
    for half in &halves {
        for profile in profile_dirs(half)? {
            held.extend(cargo_under_test::lock_files(&profile)?);
        }
    }
    let usage = Watched::files(&held)?.wait_for(
        building.pid(),
        REACH_TIMEOUT,
        "holding the lock files of both halves",
        |u| held.iter().all(|f| u.holds(f)),
    )?;
    info!("the gated build {usage}");
    expect_unsupported("a build holding both halves' locks")?;
    ensure!(building.is_running()?, "the gated build ended early");
    gate.open(s)?;
    building.wait()?.expect_success()?;

    let gate = Gate::new(s, "split-test")?;
    let testing = cargo(&["test", "--", "--nocapture"])
        .runner_gate(ctx, &gate)
        .spawn(ctx)?;
    gate.wait_until_waiting(REACH_TIMEOUT)?;
    expect_unsupported("cargo test paused before its test binary")?;
    gate.open(s)?;
    let out = testing.wait()?;
    out.expect_success()?;
    ensure!(
        out.stdout
            .contains("diskgc-e2e cargo race fixture test ran"),
        "the test did not run:\n{}",
        out.failure()
    );
    Ok(())
}

/// AC #4, a target directory shared through `.cargo/config.toml`
/// (`[build] target-dir`), outside every project (detached: docs/safety.md
/// In use, any visible Cargo keeps it), used by two projects (the race
/// fixture `p` and the smoke fixture `q`): race test 1 (a build of `p`
/// holding the locks: `locked`), race test 8 (`cargo run` of `q` paused in
/// its runner: `in_use` by the runner's environment naming the target,
/// nothing deleted, the binary runs), race test 4 (a test binary of `p`
/// running from the shared target: `in_use`) and race test 2 (a build of `q`
/// started while the driver is paused deleting: it waits for a lock diskgc
/// holds and writes nothing into the profile; diskgc stops at the next
/// part, `in_use`; the build succeeds). Then the same shared `target-dir`
/// pointing into another project's own (attached) target, which the
/// detached rule does not cover: `cargo run` of `q` paused in its runner
/// keeps `p`'s target `in_use` by the runner's environment. The window
/// between Cargo compiling and starting the runner, where only the Cargo
/// process exists and works in `q`, is the documented limit (docs/safety.md
/// In use); no gate stops Cargo there, so no test reaches it.
pub fn cargo_race_shared_target_dir_from_cargo_config(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs("w", 0o755, TESTER)?;
    s.mkdirs("shared", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    let q = crate::cargo_under_test::SMOKE.write(s, "w/q")?;
    let config = |project: &str, target_dir: &str| -> TestResult {
        s.mkdir(&format!("{project}/.cargo"), 0o755, TESTER)?;
        s.write_file(
            &format!("{project}/.cargo/config.toml"),
            format!("[build]\ntarget-dir = \"{target_dir}\"\n").as_bytes(),
            0o644,
            TESTER,
        )
        .map(|_| ())
    };
    config("w/p", "/tmp/shared/target")?;
    config("w/q", "/tmp/shared/target")?;
    build(ctx, toolchain, &p, &["build"])?;
    build(ctx, toolchain, &q, &["build"])?;
    let target = s.path("shared/target");
    let profile = target.join("debug");
    ensure!(
        !p.join("target").exists() && !q.join("target").exists(),
        "a project built its own target despite the shared target-dir"
    );

    // Race test 1: a build of p holding the shared target's locks.
    let gate = Gate::new(s, "shared-build")?;
    let main = p.join("src/main.rs");
    let text = std::fs::read_to_string(&main).map_err(|e| e.to_string())?;
    std::fs::write(&main, format!("{text}\n// edit\n")).map_err(|e| e.to_string())?;
    let holding = Cargo::new(toolchain, &p, &["build"])
        .build_gate(&gate)
        .spawn(ctx)?;
    gate.wait_until_waiting(REACH_TIMEOUT)?;
    let usage = lock_usage(&profile, holding.pid())?;
    ensure!(
        usage.holds(&profile.join(".cargo-lock")),
        "the gated build does not hold the shared .cargo-lock: it {usage}"
    );
    let oracle = Oracle::arm(crate::oracle::Fixture::new())?;
    let run = cargo_reclaim(ctx, &[], &["/tmp/shared"])?;
    let judged = oracle.judge(&run)?;
    let got = verdict_of(&run, &target)?;
    ensure!(
        got.starts_with(&format!("skip locked {target:?} entries=0 ")),
        "race test 1 on the shared target: {got}"
    );
    judged.assert_clean()?;
    gate.open(s)?;
    holding.wait()?.expect_success()?;

    // Race test 8: cargo run of q paused in its runner.
    let gate = Gate::new(s, "shared-run")?;
    let running = Cargo::new(toolchain, &q, &["run"])
        .runner_gate(ctx, &gate)
        .spawn(ctx)?;
    let runner = gate.wait_until_waiting(REACH_TIMEOUT)?;
    let oracle = Oracle::arm(crate::oracle::Fixture::new())?;
    let run = cargo_reclaim(ctx, &[], &["/tmp/shared"])?;
    let judged = oracle.judge(&run)?;
    let got = verdict_of(&run, &target)?;
    expect_skip(
        &got,
        "in_use",
        &target,
        &[(
            runner,
            "names a path inside the target in its initial environment",
        )],
    )
    .map_err(|e| format!("race test 8 on the shared target: {e}"))?;
    ensure!(unlinked(&run).is_empty(), "unlinked {:?}", unlinked(&run));
    judged.assert_clean()?;
    gate.open(s)?;
    let out = running.wait()?;
    out.expect_success()?;
    ensure!(
        out.stdout.contains(crate::cargo_under_test::SMOKE.output),
        "the binary did not run:\n{}",
        out.failure()
    );

    // Race test 4: a test binary of p still running from the shared target.
    let gate = Gate::new(s, "shared-test")?;
    let testing = Cargo::new(toolchain, &p, &["test", "--", "--nocapture"])
        .test_gate(&gate)
        .spawn(ctx)?;
    let test_binary = gate.wait_until_waiting(REACH_TIMEOUT)?;
    let oracle = Oracle::arm(crate::oracle::Fixture::new())?;
    let run = cargo_reclaim(ctx, &[], &["/tmp/shared"])?;
    let judged = oracle.judge(&run)?;
    let got = verdict_of(&run, &target)?;
    expect_skip(
        &got,
        "in_use",
        &target,
        &[(test_binary, "runs an executable inside the target")],
    )
    .map_err(|e| format!("race test 4 on the shared target: {e}"))?;
    ensure!(unlinked(&run).is_empty(), "unlinked {:?}", unlinked(&run));
    judged.assert_clean()?;
    gate.open(s)?;
    testing.wait()?.expect_success()?;

    // Race test 2: a build of q started while diskgc deletes.
    let parts = planned_parts(std::slice::from_ref(&profile))?;
    ensure!(parts.len() >= 2, "too few parts: {parts:?}");
    let in_progress = parts[parts.len() - 2].path.clone();
    let k = first_unlink_of(&parts, parts.len() - 2);
    let oracle =
        Oracle::arm(reclaimable(&target)?.concurrent_writer(target.join(".rustc_info.json")))?;
    let (mut spawned, paused) = spawn_paused(ctx, &format!("unlinkat:{k}"), &["/tmp/shared"])?;
    let driver = lock_usage(&profile, paused.pid)?;
    let before = tree_state(&profile)?;
    let mut building = Cargo::new(toolchain, &q, &["build"]).spawn(ctx)?;
    let building_pid = building.pid();
    let blocked = steadily_blocked(&profile, &mut building, "a build of q")?;
    let after = tree_state(&profile)?;
    ensure!(
        blocked.waits.iter().all(|w| driver.holds(w)) && blocked.holds.is_empty(),
        "the build of q does not wait for a lock diskgc holds ({driver}): it {blocked}"
    );
    let changes = tree_changes(&before, &after);
    ensure!(
        changes.is_empty(),
        "the build of q wrote into the shared profile while it waited: {changes:?}"
    );
    spawned.resume(&paused)?;
    let run = spawned.wait()?;
    let out = building.wait()?;
    let judged = oracle.judge(&run)?;
    let got = verdict_of(&run, &target)?;
    ensure!(
        got.starts_with(&format!("skip in_use {target:?} "))
            && got.contains(&format!("pid {building_pid} ("))
            && got.contains("the target is detached"),
        "race test 2 on the shared target: expected in_use by the detached rule: {got}"
    );
    let gone = unlinked(&run);
    ensure!(
        gone.len() >= k && gone[k - 1..].iter().all(|g| g.starts_with(&in_progress)),
        "race test 2 on the shared target: unlinked past the part in progress: {:?}",
        gone.get(k - 1..)
    );
    judged.assert_clean()?;
    out.expect_success()?;
    rebuilds(ctx, toolchain, &p, &[], false, "p after the shared reclaim")?;

    // The shared target-dir pointing at p's own, attached target.
    let r = crate::cargo_under_test::SMOKE.write(s, "w/r")?;
    let attached_project = RACE.write(s, "w/a")?;
    build(ctx, toolchain, &attached_project, &["build"])?;
    config("w/r", "/tmp/w/a/target")?;
    build(ctx, toolchain, &r, &["build"])?;
    let attached = attached_project.join("target");
    let gate = Gate::new(s, "attached-run")?;
    let running = Cargo::new(toolchain, &r, &["run"])
        .runner_gate(ctx, &gate)
        .spawn(ctx)?;
    let runner = gate.wait_until_waiting(REACH_TIMEOUT)?;
    let oracle = Oracle::arm(crate::oracle::Fixture::new())?;
    let run = cargo_reclaim(ctx, &[], &["/tmp/w/a"])?;
    let judged = oracle.judge(&run)?;
    let got = verdict_of(&run, &attached)?;
    expect_skip(
        &got,
        "in_use",
        &attached,
        &[(
            runner,
            "names a path inside the target in its initial environment",
        )],
    )
    .map_err(|e| format!("another project's attached target: {e}"))?;
    judged.assert_clean()?;
    gate.open(s)?;
    let out = running.wait()?;
    out.expect_success()?;
    ensure!(
        out.stdout.contains(crate::cargo_under_test::SMOKE.output),
        "the binary did not run:\n{}",
        out.failure()
    );
    Ok(())
}

/// M1(b) (task-11 round 2; docs/safety.md, Lock files: what a Cargo
/// writes before `.cargo-lock`): `cargo clean` started while diskgc is
/// paused deleting, at the first unlink of the second-to-last part.
///
/// - `cargo clean` (the whole target) and `cargo clean --profile dev` (the
///   whole profile directory) take no lock: they run to the end while
///   diskgc holds every lock (`/proc/locks` samples: never waiting), and
///   remove what diskgc was deleting, lock files included. Resumed, diskgc's
///   next identity-checked unlink finds nothing (`ENOENT`): it stops as
///   `changed` and unlinks nothing more (strace). Both sides only delete,
///   so nothing new is at risk; the oracle finds nothing outside the target
///   touched; the next build succeeds.
/// - `cargo clean -p PACKAGE` waits for a lock diskgc holds, writing
///   nothing into the profile directory meanwhile (the harness's walk);
///   diskgc finishes its part and stops at the next one (`in_use`: the
///   waiting Cargo); the clean then succeeds.
pub fn cargo_race_2_a_cargo_clean_during_deletion(ctx: &Ctx, toolchain: &Toolchain) -> TestResult {
    let s = &ctx.scratch;
    let variants: [(&[&str], bool); 3] = [
        (&["clean"], false),
        (&["clean", "--profile", "dev"], false),
        (&["clean", "-p", RACE.binary], true),
    ];
    for (n, (command, waits)) in variants.into_iter().enumerate() {
        let what = format!("cargo {} ({})", command.join(" "), toolchain.name);
        let rel = format!("clean-{n}");
        s.mkdir(&rel, 0o755, TESTER)?;
        let p = RACE.write(s, &format!("{rel}/p"))?;
        build(ctx, toolchain, &p, &["build"])?;
        let target = p.join("target");
        let profile = target.join("debug");
        let parts = planned_parts(std::slice::from_ref(&profile))?;
        ensure!(parts.len() >= 2, "{what}: too few parts: {parts:?}");
        let in_progress = parts[parts.len() - 2].path.clone();
        let k = first_unlink_of(&parts, parts.len() - 2);
        // A clean without the lock removes the whole target or profile,
        // lock files included: the target is its to change. The strace
        // record below checks what diskgc itself unlinked.
        let fixture = if waits {
            reclaimable(&target)?.concurrent_writer(target.join(".rustc_info.json"))
        } else {
            crate::oracle::Fixture::new().concurrent_writer(target.clone())
        };
        let oracle = Oracle::arm(fixture)?;
        let root = s.path(&rel);
        let original_locks = cargo_under_test::lock_files(&profile)?;
        let original = Watched::files(&original_locks)?;
        let (mut spawned, paused) =
            spawn_paused(ctx, &format!("unlinkat:{k}"), &[path_str(&root)?])?;
        // Before the clean, while the profile still exists: diskgc holds
        // every lock file it found (as race test 2 checks).
        let driver = original.usage(&proc_locks::sample()?, paused.pid);
        ensure!(
            original_locks.iter().all(|l| driver.holds(l)) && driver.waits.is_empty(),
            "{what}: diskgc paused at unlinkat:{k} does not hold every lock file it found: it \
             {driver}"
        );
        let before = tree_state(&target)?;
        let mut cargo = Cargo::new(toolchain, &p, command).spawn(ctx)?;
        let cargo_pid = cargo.pid();
        if waits {
            let blocked = steadily_blocked(&profile, &mut cargo, &what)?;
            let driver = lock_usage(&profile, paused.pid)?;
            ensure!(
                blocked.waits.iter().all(|w| driver.holds(w)),
                "{what}: waits for {:?}, which diskgc does not hold ({driver})",
                blocked.waits
            );
            let changes = tree_changes(&before, &tree_state(&target)?);
            ensure!(
                changes.iter().all(|(path, _)| !path.starts_with(&profile)
                    || cargo_under_test::lock_files(&profile).is_ok_and(|l| l.contains(path))
                    || path == &profile),
                "{what}: wrote into the profile directory while waiting: {changes:?}"
            );
        } else {
            let mut samples = 0;
            while cargo.is_running()? {
                // The lock files as found (their locks show even once the
                // clean removed them) and any present now; a profile the
                // clean removed holds none, any other error fails.
                let usage = original
                    .clone()
                    .and_present_in(&profile)?
                    .usage(&proc_locks::sample()?, cargo_pid);
                ensure!(
                    usage.waits.is_empty(),
                    "{what}: waits for a lock, measured not to: it {usage}"
                );
                samples += 1;
                ensure!(
                    samples < 6_000,
                    "{what}: still running after {samples} samples"
                );
                std::thread::sleep(SAMPLE_GAP);
            }
            info!("{what}: ran to its end without waiting ({samples} samples while it ran)");
        }
        spawned.resume(&paused)?;
        let run = spawned.wait()?;
        info!(
            "{what}: driver:\n{}\n--- stderr\n{}",
            run.stdout, run.stderr
        );
        let out = cargo.wait()?;
        let judged = oracle.judge(&run)?;
        let got = verdict_of(&run, &target)?;
        if waits {
            ensure!(
                got.starts_with(&format!("skip in_use {target:?} "))
                    && got.contains(&format!("pid {cargo_pid} (cargo)")),
                "{what}: expected diskgc to stop at its next part, in_use: {got}"
            );
            unlinks_end_in_part(&run, k, &in_progress, &what)?;
        } else {
            ensure!(
                got.starts_with(&format!("skip changed {target:?} entries={} ", k - 1))
                    && got.contains("ENOENT"),
                "{what}: expected diskgc's next unlink to find nothing (changed, ENOENT): {got}"
            );
            let gone = unlinked(&run);
            ensure!(
                gone.len() == k - 1,
                "{what}: diskgc unlinked {} objects after the clean: {:?}",
                gone.len() - (k - 1),
                &gone[k - 1..]
            );
        }
        judged.assert_clean()?;
        out.expect_success()?;
        ensure!(
            out.stderr.contains("Blocking waiting for file lock") == waits,
            "{what}: waited {}, expected {waits}:\n{}",
            !waits,
            out.failure()
        );
        rebuilds(ctx, toolchain, &p, &[], false, &what)?;
        info!("{what}: diskgc reported {got}");
    }
    Ok(())
}

/// M2 (task-11 round 2; docs/safety.md, Reclaim protocol steps 3 and 4):
/// diskgc keeps every lock of a target until its last unlink, and releases
/// them before it goes on to the next target in the same run. Two targets
/// `p` and `q` in one `cargo-reclaim`; the driver paused
///
/// 1. at the first unlink of `p`'s last part: it holds every lock file of
///    `p` (`/proc/locks`); a `cargo build` of `p` starts and waits for one
///    of them, steadily;
/// 2. at `p`'s very last unlink (when the last part has more than one
///    object): it still holds every lock of `p`, and the build still waits
///    for the same one;
/// 3. at `q`'s first lock (`flock_try:N+1`, `p` having N lock files): it
///    holds no lock of `p`, and the build of `p` no longer waits.
///
/// `p` and `q` are both reclaimed (the oracle: nothing outside their
/// profiles' entries touched), the build of `p` waited and succeeds.
pub fn cargo_race_locks_held_through_the_last_part_released_per_target(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    let q = RACE.write(s, "w/q")?;
    build(ctx, toolchain, &p, &["build"])?;
    build(ctx, toolchain, &q, &["build"])?;
    let target = p.join("target");
    let second = q.join("target");
    let profile = target.join("debug");
    let p_locks = cargo_under_test::lock_files(&profile)?;
    let parts = planned_parts(std::slice::from_ref(&profile))?;
    let last = parts.len() - 1;
    let k_last = first_unlink_of(&parts, last);
    let k_end = first_unlink_of(&parts, parts.len()) - 1;
    let q_first_lock = p_locks.len() + 1;
    info!(
        "parts of p: {}; pausing at unlinkat:{k_last}, unlinkat:{k_end} and flock_try:{q_first_lock}",
        sizes(&parts)
    );
    let oracle = Oracle::arm(
        reclaimable(&target)?
            .concurrent_writer(target.join(".rustc_info.json"))
            .eligible_from(reclaimable(&second)?),
    )?;
    let mut args = vec![format!("--pause-at=unlinkat:{k_last}")];
    let two_pauses_in_part = k_end > k_last;
    if two_pauses_in_part {
        args.push(format!("--pause-at=unlinkat:{k_end}"));
    } else {
        info!("p's last part has one object: no pause at its last unlink apart from its first");
    }
    args.push(format!("--pause-at=flock_try:{q_first_lock}"));
    let mut full: Vec<&str> = Vec::new();
    for a in &args {
        let (flag, point) = a.split_once('=').ok_or("bad pause flag")?;
        full.push(flag);
        full.push(point);
    }
    full.extend(["cargo-reclaim", "/tmp/w"]);
    let mut spawned = ctx.spawn_as_tester(&ctx.driver(), &full)?;

    let paused = spawned.wait_paused()?;
    ensure!(
        paused.op == "unlinkat" && paused.nth == k_last as u64,
        "first pause: {paused:?}"
    );
    let driver = lock_usage(&profile, paused.pid)?;
    ensure!(
        p_locks.iter().all(|l| driver.holds(l)),
        "at the first unlink of p's last part diskgc does not hold every lock of p: it {driver}"
    );
    let mut cargo = Cargo::new(toolchain, &p, &["build"]).spawn(ctx)?;
    let cargo_pid = cargo.pid();
    let blocked = steadily_blocked(&profile, &mut cargo, "the build of p")?;
    ensure!(
        blocked.waits.iter().all(|w| driver.holds(w)),
        "the build of p waits for {:?}, which diskgc does not hold ({driver})",
        blocked.waits
    );
    spawned.resume(&paused)?;

    if two_pauses_in_part {
        let paused = spawned.wait_paused()?;
        ensure!(
            paused.op == "unlinkat" && paused.nth == k_end as u64,
            "second pause: {paused:?}"
        );
        let sample = proc_locks::sample()?;
        let watched = Watched::files(&cargo_under_test::lock_files(&profile)?)?;
        let driver = watched.usage(&sample, paused.pid);
        let build_now = watched.usage(&sample, cargo_pid);
        ensure!(
            p_locks.iter().all(|l| driver.holds(l)),
            "at p's last unlink diskgc no longer holds every lock of p: it {driver}"
        );
        ensure!(
            build_now.waits == blocked.waits,
            "at p's last unlink the build of p no longer waits as before: it {build_now}, before \
             it {blocked}"
        );
        info!("at p's last unlink: diskgc {driver}; the build of p {build_now}");
        spawned.resume(&paused)?;
    }

    let paused = spawned.wait_paused()?;
    ensure!(
        paused.op == "flock_try" && paused.nth == q_first_lock as u64,
        "third pause: {paused:?}"
    );
    let driver = lock_usage(&profile, paused.pid)?;
    ensure!(
        driver.is_empty(),
        "before q's first lock diskgc still {driver} on p's lock files: not released"
    );
    Watched::files(&cargo_under_test::lock_files(&profile)?)?.wait_for(
        cargo_pid,
        REACH_TIMEOUT,
        "no longer waiting (diskgc released p)",
        |u| u.waits.is_empty(),
    )?;
    info!("before q's first lock diskgc holds nothing of p, and the build of p goes on");
    spawned.resume(&paused)?;
    let run = spawned.wait()?;
    info!("driver:\n{}\n--- stderr\n{}", run.stdout, run.stderr);
    let out = cargo.wait()?;
    let judged = oracle.judge(&run)?;
    let lines = results(&run)?;
    run.expect_code(0)?;
    for t in [&target, &second] {
        let got = verdict(&lines, t)?;
        ensure!(
            got.starts_with(&format!("reclaimed {t:?} ")),
            "{}: {got}",
            t.display()
        );
    }
    judged.assert_clean()?;
    out.expect_success()?;
    ensure!(
        out.stderr.contains("Blocking waiting for file lock"),
        "the build of p did not wait:\n{}",
        out.failure()
    );
    rebuilds(ctx, toolchain, &q, &[], true, "q after the run")
}
