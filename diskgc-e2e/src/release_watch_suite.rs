//! The release candidate's reclaiming `watch` (TASK-362: 0.2 runs one,
//! Mark 2026-11-05) as a plain process, through the gate-lifted package
//! (`diskgc-release`, the release runner's default `diskgc`), in the
//! host's PID namespace (the release build refuses to reclaim in one of
//! its own: TASK-263). The release has no test clock offset and no user
//! profiles, so the candidates are 0.2 built-ins that need no idle time:
//! `pytest-cache` (a `.pytest_cache` directory below a supplied root) and
//! `ccache` (its cleaner a stand-in on `PATH`, which diskgc trusts as any
//! program of that name: docs/safety.md, Command actions).
//!
//! What runs here is short (the fast tier, `just e2e`). The service path
//! under a live systemd, the per-cycle deadline, a `lock_timeout` cycle
//! and a busy host during a cycle need minutes and run in the
//! `vm-release-watch` check (docs/testing.md, Deployment units).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::ensure_suite::report;
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::release_suite::{binary_path, release_candidate_with};
use crate::scratch::Scratch;
use crate::suite::{Ctx, TestResult};
use crate::under_test::{self, Run, Spawned};
use crate::watch_suite::{host_lock_held_elsewhere, pid_below_strace, signal, snapshot_written};
use crate::{TESTER, ensure};

const MIB: u64 = 1 << 20;
/// The watched tmpfs, below the `/tmp` scratch mount.
const WATCHED: &str = "/tmp/watchfs";
const WATCHED_REL: &str = "watchfs";
/// Where the projects are: the `--roots` of `pytest-cache`.
const SRC: &str = "/tmp/watchfs/src";
const PROJECTS: usize = 10;
const PAD: u64 = 4 * MIB;

fn avail(path: &str) -> Result<u64, String> {
    let s = rustix::fs::statvfs(path).map_err(|e| format!("statvfs {path}: {e}"))?;
    Ok(s.f_bavail * s.f_frsize)
}

fn kib(bytes: u64) -> String {
    format!("{}K", bytes / 1024)
}

fn record(line: &str) -> Result<Value, String> {
    serde_json::from_str(line).map_err(|e| format!("record {line:?}: {e}"))
}

/// The process the traced run is: `diskgc` in the release runner,
/// `diskgc-release` in the standard one.
fn traced_pid(spawned: &Spawned, binary: &Path) -> Result<i32, String> {
    let comm = binary
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("binary name of {}", binary.display()))?;
    pid_below_strace(spawned, comm)
}

/// A 64 MiB tmpfs at [`WATCHED`] owned by tester, with [`PROJECTS`]
/// projects whose `.pytest_cache` holds [`PAD`] bytes each (pytest's own
/// `v/cache` layout). Returns the scratch mount and the caches, oldest
/// first.
fn pytest_caches(ctx: &Ctx) -> Result<(Scratch, Vec<PathBuf>), String> {
    ctx.scratch.mkdir(WATCHED_REL, 0o755, TESTER)?;
    let watched = Scratch::mount_sized(WATCHED, "0755", TESTER, "64m")?;
    let pad = vec![0x5a_u8; PAD as usize];
    let mut caches = Vec::new();
    for i in 0..PROJECTS {
        watched.mkdirs(
            &format!("src/p-{i:02}/.pytest_cache/v/cache"),
            0o755,
            TESTER,
        )?;
        watched.write_file(
            &format!("src/p-{i:02}/test_x.py"),
            b"def test_x(): pass\n",
            0o644,
            TESTER,
        )?;
        watched.write_file(
            &format!("src/p-{i:02}/.pytest_cache/v/cache/lastfailed"),
            b"{}\n",
            0o644,
            TESTER,
        )?;
        watched.write_file(
            &format!("src/p-{i:02}/.pytest_cache/v/cache/pad"),
            &pad,
            0o644,
            TESTER,
        )?;
        caches.push(PathBuf::from(format!("{SRC}/p-{i:02}/.pytest_cache")));
    }
    Ok((watched, caches))
}

/// The skipped rows of `record` for `candidate`.
fn skips_of<'a>(record: &'a Value, candidate: &Path) -> Vec<&'a Value> {
    let shown = candidate.display().to_string();
    record["skipped"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["candidate"].as_str() == Some(shown.as_str()))
        .collect()
}

/// TASK-362, thresholds and hysteresis through the gate-lifted package:
/// below `--min-free`, one cycle reclaims 0.2's `pytest-cache` candidates
/// up to `--target-free` and stops there, keeping the one a process
/// holds open (`in_use`); between the thresholds, no record and no
/// deletion; a drop below `--min-free` that the rest cannot cover is one
/// `exhausted` cycle with a `hold` back-off (15 minutes, no rule-1 level
/// on a filesystem smaller than its step); SIGHUP in the back-off stops
/// it at once with exit 0, no further record, the snapshot written and
/// the log saying there is no reload. The deletion oracle judges every
/// path.
pub fn release_candidate_watch_reclaims_to_target_keeps_in_use_and_stops_on_sighup(
    ctx: &Ctx,
) -> TestResult {
    let binary = release_candidate_with(ctx, "pytest-cache")?;
    let (watched, caches) = pytest_caches(ctx)?;
    let held = caches[0].join("v/cache/lastfailed");
    let holder = Holder::tester(ctx, &["hold", held.to_str().ok_or("path")?, "/"], &[])?;
    let start = avail(WATCHED)?;
    // Below --min-free at once; the goal needs six of the nine free caches.
    let min = start + 6 * MIB;
    let target = start + 22 * MIB;
    let fixture = caches
        .iter()
        .skip(1)
        .fold(Fixture::new(), |f, c| f.eligible(c.clone()));
    let oracle = Oracle::arm(fixture)?;
    let (min_s, target_s) = (kib(min), kib(target));
    let mut spawned = ctx.spawn_as_tester(
        &binary,
        &[
            "watch",
            "--json",
            "--min-free",
            &min_s,
            "--target-free",
            &target_s,
            "--interval",
            "1s",
            "--timeout",
            "1m",
            "--select",
            "profile:pytest-cache",
            "--roots",
            SRC,
            WATCHED,
        ],
    )?;
    let pid = traced_pid(&spawned, &binary)?;
    let first = record(&spawned.next_line("\"watch\"")?)?;
    ensure!(
        first["status"] == "met" && first["target_met"] == true,
        "first cycle: {first}"
    );
    ensure!(
        first["exit_code"].is_null()
            && first["watch"]["cycle"] == 1
            && first["watch"]["backoff"].is_null(),
        "first cycle: {first}"
    );
    let reclaimed = first["actions"].as_array().map_or(0, Vec::len);
    let after_first = avail(WATCHED)?;
    // Stops at the goal: reached, and one deletion fewer would not have.
    ensure!(
        after_first >= target && after_first - PAD < target,
        "{reclaimed} actions left {after_first}, target {target}: {first}"
    );

    // Between the thresholds: nothing for 4 s.
    let left: Vec<PathBuf> = caches.iter().filter(|c| c.exists()).cloned().collect();
    let between = after_first - (min + target) / 2;
    watched.write_file("filler-1", &vec![0x33; between as usize], 0o644, TESTER)?;
    ensure!(
        spawned.line_within(Duration::from_secs(4))?.is_none(),
        "a record between the thresholds"
    );
    ensure!(
        left.iter().all(|c| c.exists()),
        "a candidate deleted between the thresholds"
    );

    // Below --min-free by more than what is left: one exhausted cycle.
    let now = avail(WATCHED)?;
    let goal_after_fill = min - 14 * MIB;
    watched.write_file(
        "filler-2",
        &vec![0x33; (now - goal_after_fill) as usize],
        0o644,
        TESTER,
    )?;
    let second = record(&spawned.next_line("\"watch\"")?)?;
    ensure!(
        second["status"] == "exhausted" && second["watch"]["cycle"] == 2,
        "second cycle: {second}"
    );
    let backoff = &second["watch"]["backoff"];
    ensure!(
        backoff["class"] == "hold"
            && backoff["attempt"] == 0
            && backoff["wait_seconds"] == 900
            && backoff["resume_below_bytes"].is_null(),
        "an exhausted cycle holds for 15 minutes; a 64 MiB filesystem cannot drop by \
         rule 1's 1 GiB step: {second}"
    );
    // Held open: kept in whichever cycle reached it, never deleted.
    let kept = [&first, &second]
        .iter()
        .flat_map(|r| skips_of(r, &caches[0]))
        .any(|s| s["reason"] == "in_use");
    ensure!(kept, "the held cache is not kept in_use: {first}\n{second}");
    ensure!(caches[0].exists(), "the held cache is gone");

    // In back-off: SIGHUP stops at once, no record.
    let hup = Instant::now();
    signal(pid, libc::SIGHUP)?;
    let run = spawned.wait()?;
    let stopped_in = hup.elapsed();
    drop(holder);
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        stopped_in < Duration::from_secs(5),
        "SIGHUP in back-off took {stopped_in:?}"
    );
    ensure!(
        run.stderr
            .contains("stopping on SIGHUP: diskgc does not reload"),
        "stderr: {}",
        run.stderr
    );
    ensure!(
        run.stdout.lines().count() == 2,
        "two records, nothing else on stdout: {}",
        run.stdout
    );
    ensure!(snapshot_written(), "no snapshot written: {}", run.stderr);
    ensure!(
        Path::new(WATCHED).join("filler-1").exists()
            && Path::new(WATCHED).join("filler-2").exists(),
        "a non-candidate is gone"
    );
    ensure!(
        Path::new(SRC).join("p-05/test_x.py").exists(),
        "a project's source is gone"
    );
    verdict.assert_clean()
}

/// The stand-in `ccache`: says it started, waits for the test's release
/// file (at most 60 s), then removes the cache's payload, as `ccache
/// --clear` empties it. Started by diskgc under the host lock, which it
/// inherits, so while it waits the reclaim that started it holds the lock.
const SLOW_CCACHE: &str = "#!/bin/sh\n\
: > /home/tester/lock-turns/started\n\
i=0\n\
while [ ! -e /home/tester/lock-turns/release ] && [ $i -lt 1200 ]; do /bin/sleep 0.05; i=$((i+1)); done\n\
/bin/rm -f -- \"$CCACHE_DIR/payload.bin\"\n\
exit 0\n";

const CCACHE_CACHE: &str = "/home/tester/.cache/ccache";

/// The stand-ins directory (`env` and the slow `ccache`) and a cache with
/// a payload; returns the `PATH` to run with.
fn slow_ccache(ctx: &Ctx) -> Result<Vec<(String, String)>, String> {
    ctx.home.mkdirs("lock-turns", 0o755, TESTER)?;
    ctx.home.mkdirs("stand-ins", 0o755, TESTER)?;
    ctx.home.symlink("stand-ins/env", Path::new("/bin/env"))?;
    ctx.home
        .write_file("stand-ins/ccache", SLOW_CCACHE.as_bytes(), 0o755, TESTER)?;
    fill_ccache(ctx)?;
    Ok(vec![(
        "PATH".to_owned(),
        "/home/tester/stand-ins".to_owned(),
    )])
}

fn fill_ccache(ctx: &Ctx) -> TestResult {
    ctx.home.mkdirs(".cache/ccache", 0o755, TESTER)?;
    ctx.home.write_file(
        ".cache/ccache/payload.bin",
        &vec![0x42; MIB as usize],
        0o644,
        TESTER,
    )?;
    Ok(())
}

fn wait_for(path: &Path, what: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        ensure!(Instant::now() < deadline, "{what}: no {}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn ensure_args(timeout: &str) -> Vec<&str> {
    vec![
        "ensure",
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--timeout",
        timeout,
        "--select",
        "profile:ccache",
        "--roots",
        "/home/tester",
        "/home/tester",
    ]
}

const WATCH_CCACHE: [&str; 14] = [
    "watch",
    "--json",
    "--min-free",
    "100%",
    "--target-free",
    "100%",
    "--interval",
    "1s",
    "--timeout",
    "1m",
    "--select",
    "profile:ccache",
    "--roots",
    "/home/tester",
];

fn run_ensure(
    ctx: &Ctx,
    binary: &Path,
    env: &[(String, String)],
    timeout: &str,
) -> Result<Run, String> {
    under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        env,
        binary,
        &ensure_args(timeout),
    )
}

/// TASK-362, `watch` beside `ensure` through the gate-lifted package
/// (docs/reference.md, Watch beside `ensure`): their reclaims take turns
/// through the host lock.
///
/// 1. A `watch` cycle's cleaner (0.2's `ccache` command action, a slow
///    stand-in) holds the host lock it inherited; an `ensure` meanwhile
///    waits for it to its `--timeout` and exits 75, `lock_timeout`,
///    deleting nothing. Released, the cycle ends `exhausted` (`100%` is
///    never met) with a `hold` back-off, and SIGINT then stops `watch` at
///    once with exit 0 and no further record.
/// 2. The reverse: an `ensure` whose cleaner holds the lock; a new
///    `watch`'s first cycle waits for it, and SIGTERM during that wait
///    ends the cycle `interrupted` (its record, nothing deleted)
///    and `watch` with exit 0 (it never took the lock: no "taken" line,
///    no error, no after sample). Released, the `ensure` reclaims.
pub fn release_candidate_watch_and_ensure_take_turns_through_the_host_lock(
    ctx: &Ctx,
) -> TestResult {
    let binary = release_candidate_with(ctx, "ccache")?;
    let env = slow_ccache(ctx)?;
    let started = Path::new("/home/tester/lock-turns/started");
    let release = "lock-turns/release";
    let cache = PathBuf::from(CCACHE_CACHE);
    let oracle = Oracle::arm(Fixture::new().eligible(cache.clone()))?;

    // 1. watch holds the lock; ensure exits 75.
    let mut args = WATCH_CCACHE.to_vec();
    args.push("/home/tester");
    let mut watch = under_test::spawn_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &binary,
        &args,
    )?;
    let pid = traced_pid(&watch, &binary)?;
    wait_for(started, "the watch cycle's cleaner")?;
    ensure!(
        host_lock_held_elsewhere()?,
        "the host lock is free while the watch cycle's cleaner runs"
    );
    let waited = Instant::now();
    let blocked = run_ensure(ctx, &binary, &env, "2s")?;
    let waited = waited.elapsed();
    blocked.expect_code(75)?;
    let blocked_report = report(&blocked)?;
    ensure!(
        blocked_report["status"] == "lock_timeout"
            && blocked_report["actions"]
                .as_array()
                .is_some_and(Vec::is_empty),
        "ensure beside a watch cycle: {blocked_report}"
    );
    ensure!(
        waited >= Duration::from_secs(2)
            && blocked
                .stderr
                .contains("was still held by another reclaim at the deadline"),
        "ensure gave up on the lock before its 2 s timeout, or for another reason \
         ({waited:?}): {}",
        blocked.stderr
    );
    ensure!(
        cache.join("payload.bin").exists(),
        "the payload went before the cleaner was released"
    );
    ctx.home.write_file(release, b"", 0o644, TESTER)?;
    let first = record(&watch.next_line("\"watch\"")?)?;
    ensure!(
        first["status"] == "exhausted"
            && first["watch"]["cycle"] == 1
            && first["watch"]["backoff"]["class"] == "hold",
        "the watch cycle: {first}"
    );
    ensure!(
        first["actions"]
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0]["candidate"] == CCACHE_CACHE),
        "the watch cycle ran the cleaner once on {CCACHE_CACHE}: {first}"
    );
    let int = Instant::now();
    signal(pid, libc::SIGINT)?;
    let first_run = watch.wait()?;
    ensure!(
        int.elapsed() < Duration::from_secs(5),
        "SIGINT in back-off took {:?}",
        int.elapsed()
    );
    first_run.expect_code(0)?;
    ensure!(
        first_run.stderr.contains("stopping on SIGINT"),
        "stderr: {}",
        first_run.stderr
    );
    ensure!(
        first_run.stdout.lines().count() == 1,
        "one record: {}",
        first_run.stdout
    );

    // 2. ensure holds the lock; the watch cycle waits and is interrupted.
    ctx.home.remove_file(release)?;
    ctx.home.remove_file("lock-turns/started")?;
    fill_ccache(ctx)?;
    let holder = under_test::spawn_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &binary,
        &ensure_args("2m"),
    )?;
    wait_for(started, "the ensure's cleaner")?;
    let mut watch = under_test::spawn_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &binary,
        &args,
    )?;
    let pid = traced_pid(&watch, &binary)?;
    // Its first cycle starts at once and waits for the lock.
    std::thread::sleep(Duration::from_secs(2));
    ensure!(
        watch.line_within(Duration::from_millis(100))?.is_none(),
        "a watch cycle ended while ensure held the lock"
    );
    signal(pid, libc::SIGTERM)?;
    let interrupted = record(&watch.next_line("\"watch\"")?)?;
    let second_run = watch.wait()?;
    ensure!(
        interrupted["status"] == "interrupted"
            && interrupted["watch"]["backoff"].is_null()
            && interrupted["actions"].as_array().is_some_and(Vec::is_empty),
        "the cycle waiting for the lock: {interrupted}"
    );
    // It never took the lock (the first watch did, and says so); the
    // signal ended its wait, which a record of an interrupted lock wait
    // carries as its status alone (no error, no after sample).
    ensure!(
        first_run.stderr.contains(" taken after ")
            && second_run.stderr.contains("taking the reclaim lock")
            && !second_run.stderr.contains(" taken after "),
        "the lock waits: first watch: {}\nsecond watch: {}",
        first_run.stderr,
        second_run.stderr
    );
    ensure!(
        interrupted["errors"].as_array().is_some_and(Vec::is_empty)
            && interrupted["avail_after_bytes"].is_null(),
        "the cycle waiting for the lock: {interrupted}"
    );
    second_run.expect_code(0)?;
    ensure!(
        cache.join("payload.bin").exists(),
        "the payload went while ensure's cleaner waited"
    );
    ctx.home.write_file(release, b"", 0o644, TESTER)?;
    let reclaimed = holder.wait()?;
    reclaimed.expect_code(1)?;
    let reclaimed_report = report(&reclaimed)?;
    ensure!(
        reclaimed_report["actions"]
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0]["candidate"] == CCACHE_CACHE),
        "the ensure that held the lock: {reclaimed_report}"
    );
    ensure!(
        !cache.join("payload.bin").exists(),
        "the released cleaner did not run"
    );
    oracle
        .judge_runs(&[&first_run, &blocked, &second_run, &reclaimed])?
        .assert_clean()
}

/// TASK-362: the release's reclaiming `watch` narrows to its catalog as
/// its `ensure` does: a configuration with a user profile, and a
/// `--select` of a built-in 0.2 does not ship (`node-modules`, held out by
/// Mark, TASK-365), are refused with exit 2 before anything on the
/// machine is read (`PATH` does not exist, so a binary without the
/// refusal fails on that instead), and the refusal names the profile.
pub fn release_candidate_watch_refuses_user_profiles_and_unshipped_builtins(
    ctx: &Ctx,
) -> TestResult {
    let binary = binary_path(ctx, "diskgc-release");
    ctx.scratch
        .mkdirs("release-watch/proj/node_modules", 0o755, TESTER)?;
    let keep = ctx.scratch.write_file(
        "release-watch/proj/node_modules/keep",
        b"a refused watch must preserve this",
        0o644,
        TESTER,
    )?;
    ctx.home.write_file(
        "release-watch.yaml",
        b"version: 1\nprofiles:\n  - name: junk\n    find: {roots: [/tmp/release-watch], paths: [proj], type: dir}\n    action: delete\n    examples: {match: [/tmp/release-watch/proj]}\ngroups:\n  - name: g\n    tiers:\n      - name: t\n        select: profile:junk\n",
        0o644,
        TESTER,
    )?;
    let cases: [(&[&str], &str); 2] = [
        (
            &["--config", "/home/tester/release-watch.yaml"],
            "`junk` is a user profile",
        ),
        (
            &[
                "--select",
                "profile:node-modules",
                "--roots",
                "/tmp/release-watch",
            ],
            "`node-modules` is a built-in that diskgc 0.2 does not ship",
        ),
    ];
    let oracle = Oracle::arm(Fixture::new())?;
    let mut runs = Vec::new();
    for (extra, want) in cases {
        let mut args = vec!["watch", "--json", "--min-free", "1G", "--target-free", "2G"];
        args.extend_from_slice(extra);
        args.push("/nonexistent-release-watch");
        let run = ctx.run_as_tester(&binary, &args)?;
        run.expect_code(2)?;
        let error: Value = serde_json::from_str(run.stdout.trim())
            .map_err(|e| format!("refusal object {:?}: {e}", run.stdout))?;
        ensure!(
            error["exit_code"] == 2 && error.to_string().contains(want),
            "{extra:?}: wanted {want:?}: {error}"
        );
        runs.push(run);
    }
    let refs: Vec<&Run> = runs.iter().collect();
    oracle.judge_runs(&refs)?.assert_clean()?;
    ensure!(keep.exists(), "a refused watch mutated the fixture");
    Ok(())
}
