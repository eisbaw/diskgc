//! `watch` as a plain process (TASK-32, TASK-236; docs/testing.md, End-to-end
//! tests: signals and `watch`): the static binary on a small tmpfs of its
//! own, its JSON lines read as a script would, SIGTERM from the harness, and
//! the deletion oracle on every run.
//!
//! Candidates are made now; a test-only clock offset of two hours
//! ([`SandboxClockOffset`]) makes `filter:idle-1h` take them (idle filters
//! use `ctime`, which a fixture cannot backdate).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::differential_suite::SandboxClockOffset;
use crate::oracle::{Fixture, Oracle};
use crate::scratch::Scratch;
use crate::suite::{Ctx, TestResult};
use crate::under_test::Spawned;
use crate::{TESTER, ensure};

/// The watched tmpfs, below the `/tmp` scratch mount.
const WATCHED: &str = "/tmp/watchfs";
const WATCHED_REL: &str = "watchfs";
const MIB: u64 = 1 << 20;
const JUNK: usize = 10;
const JUNK_BYTES: u64 = 4 * MIB;
const CONFIG: &str = "/home/tester/diskgc-watch.yaml";
const SELECT: &str = "profile:watch-junk && filter:idle-1h";
const STATE_DIR: &str = "/home/tester/.local/state/diskgc";

/// A 64 MiB tmpfs at [`WATCHED`] owned by tester (a scratch mount of its
/// own), `JUNK` candidates of 4 MiB each on it, and the configuration
/// naming them.
fn fixture(ctx: &Ctx) -> Result<(Scratch, Vec<PathBuf>), String> {
    ctx.scratch.mkdir(WATCHED_REL, 0o755, TESTER)?;
    let watched = Scratch::mount_sized(WATCHED, "0755", TESTER, "64m")?;
    let blob = vec![0x5a_u8; JUNK_BYTES as usize];
    let mut junk = Vec::new();
    for i in 0..JUNK {
        let dir = watched.mkdir(&format!("junk-{i:02}"), 0o755, TESTER)?;
        watched.write_file(&format!("junk-{i:02}/blob"), &blob, 0o644, TESTER)?;
        junk.push(dir);
    }
    let config = format!(
        "version: 1\nprofiles:\n  - name: watch-junk\n    find: {{roots: [{WATCHED}], paths: [\"junk-*\"], type: dir}}\n    action: delete\n    examples: {{match: [{WATCHED}/junk-00]}}\n"
    );
    ctx.home
        .write_file("diskgc-watch.yaml", config.as_bytes(), 0o644, TESTER)?;
    Ok((watched, junk))
}

fn avail() -> Result<u64, String> {
    let s = rustix::fs::statvfs(WATCHED).map_err(|e| format!("statvfs {WATCHED}: {e}"))?;
    Ok(s.f_bavail * s.f_frsize)
}

fn kib(bytes: u64) -> String {
    format!("{}K", bytes / 1024)
}

/// Writes a non-candidate file of `bytes` on the watched tmpfs.
fn fill(watched: &Scratch, name: &str, bytes: u64) -> Result<PathBuf, String> {
    let content = vec![0x33_u8; bytes as usize];
    watched.write_file(name, &content, 0o644, TESTER)
}

/// The traced `diskgc` below strace.
fn diskgc_pid(spawned: &Spawned) -> Result<i32, String> {
    let tracer = spawned.strace_pid();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let children = fs::read_to_string(format!("/proc/{tracer}/task/{tracer}/children"))
            .unwrap_or_default();
        for pid in children.split_whitespace() {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            if comm.trim() == "diskgc" {
                return pid.parse().map_err(|e| format!("pid {pid}: {e}"));
            }
        }
        ensure!(Instant::now() < deadline, "no diskgc below strace {tracer}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn signal(pid: i32, sig: i32) -> TestResult {
    // SAFETY: kill(2) on the traced diskgc's pid; no memory is shared.
    let sent = unsafe { libc::kill(pid, sig) };
    ensure!(
        sent == 0,
        "signal {sig} to {pid}: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

/// The `nice` field (19th) of a `/proc/.../stat` line.
fn nice_of(stat: &str) -> Option<i64> {
    let close = stat.rfind(')')?;
    stat.get(close + 2..)?.split(' ').nth(16)?.parse().ok()
}

/// `nice` of every task of `pid` by its thread name.
fn nice_by_thread(pid: i32) -> Result<Vec<(String, i64)>, String> {
    let mut out = Vec::new();
    let tasks = fs::read_dir(format!("/proc/{pid}/task")).map_err(|e| format!("tasks: {e}"))?;
    for task in tasks {
        let path = task.map_err(|e| e.to_string())?.path();
        let comm = fs::read_to_string(path.join("comm")).unwrap_or_default();
        let stat = fs::read_to_string(path.join("stat")).unwrap_or_default();
        let nice = nice_of(&stat).ok_or_else(|| format!("no nice in {stat:?}"))?;
        out.push((comm.trim().to_owned(), nice));
    }
    Ok(out)
}

/// A snapshot file in tester's state directory.
fn snapshot_written() -> bool {
    fs::read_dir(STATE_DIR).is_ok_and(|dir| {
        dir.flatten().any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("index-") && !name.ends_with(".lock") && !name.ends_with(".new")
        })
    })
}

/// TASK-32 AC #1 (container half) and the walk as a script sees it: below
/// `--min-free`, one cycle reclaims the oldest candidates up to
/// `--target-free` and writes one JSON line; between the thresholds, no
/// line and no deletion; a drop below `--min-free` again, one more cycle;
/// SIGTERM exits 0 with a snapshot written. The tally runs on a thread of
/// its own at nice 19 while the main thread is not lowered (TASK-236).
pub fn watch_process_reclaims_to_target_then_stays_quiet_and_stops_on_sigterm(
    ctx: &Ctx,
) -> TestResult {
    let (watched, junk) = fixture(ctx)?;
    let _clock = SandboxClockOffset::install(2 * 3600)?;
    let start = avail()?;
    // Below --min-free at once; the goal needs 5 to 6 of the 10 candidates.
    let min = start + 6 * MIB;
    let target = start + 22 * MIB;
    let mut fixture = Fixture::new();
    for dir in &junk {
        fixture = fixture.eligible(dir.clone());
    }
    let oracle = Oracle::arm(fixture)?;
    let (min_s, target_s) = (kib(min), kib(target));
    let mut spawned = ctx.spawn_as_tester(
        &ctx.diskgc(),
        &[
            "watch",
            "--json",
            "--config",
            CONFIG,
            "--min-free",
            &min_s,
            "--target-free",
            &target_s,
            "--interval",
            "1s",
            "--timeout",
            "1m",
            "--select",
            SELECT,
            WATCHED,
        ],
    )?;
    let pid = diskgc_pid(&spawned)?;
    let first = spawned.next_line("\"watch\"")?;
    let first: serde_json::Value =
        serde_json::from_str(&first).map_err(|e| format!("record {first:?}: {e}"))?;
    ensure!(first["status"] == "met", "first cycle: {first}");
    ensure!(first["target_met"] == true, "first cycle: {first}");
    ensure!(first["exit_code"].is_null(), "first cycle: {first}");
    ensure!(first["watch"]["cycle"] == 1, "first cycle: {first}");
    ensure!(first["watch"]["backoff"].is_null(), "first cycle: {first}");
    let reclaimed = first["actions"].as_array().map_or(0, Vec::len);
    ensure!(
        (5..JUNK).contains(&reclaimed),
        "stops at the goal: {reclaimed} actions: {first}"
    );
    let after_first = avail()?;
    ensure!(after_first >= target, "{after_first} < target {target}");
    // TASK-236: the tally's own thread is at nice 19, the main thread not.
    let threads = nice_by_thread(pid)?;
    ensure!(
        threads
            .iter()
            .any(|(name, nice)| name == "diskgc-tally" && *nice == 19),
        "no diskgc-tally thread at nice 19: {threads:?}"
    );
    let main = fs::read_to_string(format!("/proc/{pid}/task/{pid}/stat"))
        .map_err(|e| format!("main thread stat: {e}"))?;
    ensure!(
        nice_of(&main) == Some(0),
        "the main thread, where cycles run, is not at nice 0: {main}"
    );
    // Between the thresholds: a writer takes space, but not below
    // --min-free. Nothing for 5 s.
    let between = after_first - (min + target) / 2;
    fill(&watched, "filler-1", between)?;
    let left: Vec<PathBuf> = junk.iter().filter(|d| d.exists()).cloned().collect();
    ensure!(
        spawned.line_within(Duration::from_secs(5))?.is_none(),
        "a record between the thresholds"
    );
    ensure!(
        left.iter().all(|d| d.exists()),
        "a candidate deleted between the thresholds"
    );
    // Below --min-free again: one more cycle.
    let now = avail()?;
    fill(&watched, "filler-2", now - min + 2 * MIB)?;
    let second = spawned.next_line("\"watch\"")?;
    let second: serde_json::Value =
        serde_json::from_str(&second).map_err(|e| format!("record {second:?}: {e}"))?;
    ensure!(second["watch"]["cycle"] == 2, "second cycle: {second}");
    signal(pid, libc::SIGTERM)?;
    let run = spawned.wait()?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        run.stderr.contains("stopping on SIGTERM"),
        "stderr: {}",
        run.stderr
    );
    ensure!(snapshot_written(), "no snapshot in {STATE_DIR}");
    ensure!(
        run.stdout.lines().count() == 2,
        "two records, nothing else on stdout: {}",
        run.stdout
    );
    ensure!(
        Path::new(WATCHED).join("filler-1").exists()
            && Path::new(WATCHED).join("filler-2").exists(),
        "a non-candidate is gone"
    );
    verdict.assert_clean()
}

/// TASK-32 AC #3: `--tally-only` with available space far below any
/// threshold deletes nothing (deletion oracle with no eligible path),
/// writes no record, and writes its snapshot at the stop. A second
/// `watch` for the same filesystem and user exits 2 after its 10 s wait
/// (TASK-30 AC #1, watch's half).
pub fn watch_tally_only_never_deletes_and_a_second_watch_exits_2(ctx: &Ctx) -> TestResult {
    let (watched, junk) = fixture(ctx)?;
    // Nearly full: a script would reclaim here; tally-only must not.
    let now = avail()?;
    fill(&watched, "filler", now.saturating_sub(MIB))?;
    let _clock = SandboxClockOffset::install(2 * 3600)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let args = [
        "watch",
        "--tally-only",
        "--json",
        "--config",
        CONFIG,
        "--select",
        SELECT,
        WATCHED,
    ];
    let spawned = ctx.spawn_as_tester(&ctx.diskgc(), &args)?;
    let pid = diskgc_pid(&spawned)?;
    // Its lock is taken right after start; the second one waits 10 s for it.
    std::thread::sleep(Duration::from_secs(1));
    let second = ctx.run_as_tester(&ctx.diskgc(), &args)?;
    std::thread::sleep(Duration::from_secs(1));
    signal(pid, libc::SIGTERM)?;
    let first = spawned.wait()?;
    let verdict = oracle.judge_runs(&[&first, &second])?;
    first.expect_code(0)?;
    second.expect_code(2)?;
    ensure!(
        second.stderr.contains("another watch holds index-"),
        "second watch: {}",
        second.stderr
    );
    let refusal: serde_json::Value = serde_json::from_str(second.stdout.trim())
        .map_err(|e| format!("second watch's object {:?}: {e}", second.stdout))?;
    ensure!(refusal["exit_code"] == 2, "{refusal}");
    ensure!(
        first.stdout.is_empty(),
        "tally-only wrote: {}",
        first.stdout
    );
    ensure!(snapshot_written(), "no snapshot in {STATE_DIR}");
    ensure!(
        first.stderr.contains(&format!("written: {JUNK} unit(s)")),
        "the snapshot holds the candidates: {}",
        first.stderr
    );
    ensure!(junk.iter().all(|d| d.exists()), "a candidate is gone");
    verdict.assert_clean()
}

/// TASK-236 (TASK-29 AC #1, container half): while another process holds
/// the host lock, the background tally lists nothing; its snapshot at the
/// stop holds no unit.
pub fn watch_tally_pauses_while_another_process_holds_the_host_lock(ctx: &Ctx) -> TestResult {
    let _watched = fixture(ctx)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let lock =
        fs::File::open(crate::suite::HOST_LOCK).map_err(|e| format!("open the host lock: {e}"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|e| format!("hold the host lock: {e}"))?;
    let spawned = ctx.spawn_as_tester(
        &ctx.diskgc(),
        &[
            "watch",
            "--tally-only",
            "--config",
            CONFIG,
            "--select",
            SELECT,
            WATCHED,
        ],
    )?;
    let pid = diskgc_pid(&spawned)?;
    std::thread::sleep(Duration::from_secs(3));
    signal(pid, libc::SIGTERM)?;
    let run = spawned.wait()?;
    drop(lock);
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        run.stderr
            .contains("background tally paused: the host lock is held"),
        "stderr: {}",
        run.stderr
    );
    ensure!(
        run.stderr.contains("written: 0 unit(s)"),
        "the tally listed while the lock was held: {}",
        run.stderr
    );
    verdict.assert_clean()
}

/// Whether `pid` has exited (gone, or a zombie its parent has not reaped).
fn exited(pid: i32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rfind(')')
            .and_then(|close| stat.get(close + 2..close + 3))
            .is_some_and(|state| state == "Z" || state == "X"),
        Err(_) => true,
    }
}

/// Whether `pid` is alive and not a zombie.
fn alive(pid: i32) -> bool {
    !exited(pid)
}

/// Whether some other process holds the host lock: a non-blocking
/// exclusive `flock` on it fails with `EWOULDBLOCK`.
fn host_lock_held_elsewhere() -> Result<bool, String> {
    let lock =
        fs::File::open(crate::suite::HOST_LOCK).map_err(|e| format!("open the host lock: {e}"))?;
    match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(false),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Err(e) => Err(format!("flock the host lock: {e}")),
    }
}

/// TASK-257 (docs/reference.md, Watch signals): a second SIGTERM during
/// the stop skips the rest of a running child's 10 s SIGTERM grace and
/// the snapshot write, and `watch` exits 0 at once. The child, a cleaner
/// that ignores SIGTERM, is left running (never killed unawaited) and
/// keeps the host lock it inherited, so no other reclaim starts beside
/// it. The cycle's record (`interrupted`) is still written.
pub fn watch_second_signal_leaves_a_stubborn_child_holding_the_host_lock(ctx: &Ctx) -> TestResult {
    let first = ctx.scratch.mkdir("stubborn-watch-first", 0o700, TESTER)?;
    let removed = ctx
        .scratch
        .write_file("stubborn-watch-first/one", b"one", 0o600, TESTER)?;
    let kept = ctx
        .scratch
        .write_file("stubborn-watch-first/two", b"two", 0o600, TESTER)?;
    let marker = ctx.scratch.path("stubborn-watch-ready");
    let cleaner = ctx.scratch.write_file(
        "stubborn-watch-cleaner",
        format!(
            "#!/bin/sh\ntrap 'printf TERM > {}' TERM\n/bin/rm -- \"$1/one\"\nprintf '%s %s\\n' \"$PPID\" \"$$\" > {}\nwhile :; do /bin/sleep 1 || :; done\n",
            ctx.scratch.path("stubborn-watch-term-seen").display(),
            marker.display()
        )
        .as_bytes(),
        0o755,
        TESTER,
    )?;
    let config = format!(
        "version: 1\nprofiles:\n  - name: stubborn-watch\n    find: {{roots: [/tmp], paths: [stubborn-watch-first], type: dir}}\n    action: {{command: [{}, \"{{path}}\"]}}\n    examples: {{match: [/tmp/stubborn-watch-first]}}\n",
        cleaner.display()
    );
    ctx.home.write_file(
        "diskgc-stubborn-watch.yaml",
        config.as_bytes(),
        0o644,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new().eligible(first.clone()))?;
    let spawned = ctx.spawn_as_tester(
        &ctx.diskgc(),
        &[
            "watch",
            "--json",
            "--config",
            "/home/tester/diskgc-stubborn-watch.yaml",
            "--min-free",
            "100%",
            "--target-free",
            "100%",
            "--interval",
            "1s",
            "--timeout",
            "1m",
            "--select",
            "profile:stubborn-watch",
            "/tmp",
        ],
    )?;
    // The cleaner says who its parent (diskgc) and itself are.
    let deadline = Instant::now() + Duration::from_secs(20);
    let (pid, child) = loop {
        if let Ok(text) = fs::read_to_string(&marker)
            && let Some((parent, own)) = text.trim().split_once(' ')
        {
            let parse = |s: &str| s.parse::<i32>().map_err(|e| format!("{text:?}: {e}"));
            break (parse(parent)?, parse(own)?);
        }
        ensure!(Instant::now() < deadline, "the cleaner never started");
        std::thread::sleep(Duration::from_millis(10));
    };
    ensure!(
        !removed.exists() && kept.exists(),
        "the cleaner did not reach its partial deletion"
    );
    signal(pid, libc::SIGTERM)?;
    // The child gets its SIGTERM (and ignores it): the grace has begun.
    let term_seen = ctx.scratch.path("stubborn-watch-term-seen");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !term_seen.exists() {
        ensure!(Instant::now() < deadline, "the child never got SIGTERM");
        std::thread::sleep(Duration::from_millis(10));
    }
    ensure!(alive(pid), "watch ended before the second signal");
    let second = Instant::now();
    signal(pid, libc::SIGTERM)?;
    let deadline = second + Duration::from_secs(4);
    while !exited(pid) {
        ensure!(
            Instant::now() < deadline,
            "watch did not exit within 4 s of the second signal (the grace is 10 s)"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let left_running = alive(child);
    let lock_held = host_lock_held_elsewhere();
    // Let the run end: the child is the harness's to stop (strace follows
    // it, so the run's records end with it).
    // SAFETY: kill(2) on the cleaner's process group (diskgc made it its
    // own group); no memory is shared.
    unsafe { libc::kill(-child, libc::SIGKILL) };
    let run = spawned.wait()?;
    let verdict = oracle.judge(&run)?;
    ensure!(left_running, "the child was not left running");
    ensure!(
        lock_held?,
        "the host lock was free while the child that inherited it ran"
    );
    run.expect_code(0)?;
    ensure!(
        run.stderr.contains("second signal"),
        "stderr does not name the second signal: {}",
        run.stderr
    );
    ensure!(!snapshot_written(), "a snapshot was written in {STATE_DIR}");
    let records: Vec<&str> = run.stdout.lines().collect();
    ensure!(records.len() == 1, "one record: {}", run.stdout);
    let record: serde_json::Value =
        serde_json::from_str(records[0]).map_err(|e| format!("record {records:?}: {e}"))?;
    ensure!(record["status"] == "interrupted", "record: {record}");
    ensure!(kept.exists(), "the cleaner's second file is gone");
    verdict.assert_clean()
}
