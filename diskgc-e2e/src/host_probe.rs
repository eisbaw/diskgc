//! Host probes: drive `LinuxHost` and the core's path traversal as the code
//! under test (`tester`, traced) and print what they did, one line per
//! result, for the `host_*` container tests ([`crate::host_suite`]).
//!
//! Results are addressed by argument index, not by path, because the
//! fixtures use paths with spaces and tabs. Line formats:
//!
//! - `I ok mnt=<mount id> dev=<maj:min> ino=<inode>`
//! - `I err <reason> <tag> -- <message>`: `reason` is the skip reason
//!   (`unsafe_path`, `unsupported_fs`, `missing`, `error`), `tag` names the
//!   cause (see [`tag`]).
//! - `host-unlink`: `I ok` or `I err <cause> -- <message>`, `cause` being
//!   the errno name, `invalid` or `sandbox_gate` ([`cause_tag`]).

use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use std::ffi::OsStr;

use diskgc_core::host::{
    Access, Base, Cause, Delete, Host, HostError, LockMode, MonoTime, Pid, ProcError, ProcLink,
    Stat, TryLock, Unlink, WaitLock,
};
use diskgc_core::resolve::{MountScreenError, ResolveError, Unsafe, Want, open_beneath, resolve};
use diskgc_linux::{LinuxFd, LinuxHost};

use crate::lock_release::within_release_bound;

/// Runs host probe `cmd` with `args`.
pub fn main(cmd: &str, args: &[String]) -> Result<(), String> {
    let host = LinuxHost::new().map_err(|e| format!("host setup: {e}"))?;
    match (cmd, args) {
        ("host-resolve", [want, paths @ ..]) => resolve_paths(&host, want, paths),
        ("host-beneath", [root, want, rels @ ..]) => beneath(&host, root, want, rels),
        ("host-read-fifo", [root, rel]) => read_fifo(&host, root, rel),
        ("host-stat", paths) => stat_paths(&host, paths),
        ("host-statvfs", [path]) => statvfs(&host, path),
        ("host-lock", [how, path]) if how == "try" => lock_try(&host, path),
        ("host-lock", [how, path, millis]) if how == "wait" => lock_wait(&host, path, millis, None),
        ("host-lock", [how, path, millis, busy_marker]) if how == "wait" => {
            lock_wait(&host, path, millis, Some(busy_marker))
        }
        ("host-lock", [how, path]) if how == "close-semantics" => close_semantics(&host, path),
        ("host-proc", [pid]) => proc_readers(&host, pid),
        ("host-proc-replaced", []) => proc_replaced_after_startup(&host),
        ("host-pidfd-release", []) => pidfd_release(&host),
        ("host-inuse-fork-churn", [candidate]) => inuse_fork_churn(&host, candidate),
        ("host-inuse-sustained-churn", [candidate]) => inuse_sustained_churn(&host, candidate),
        ("host-inuse-index-memory", []) => inuse_index_memory(&host),
        ("host-kernel", []) => kernel(&host),
        ("host-maps-watch", [pid, want, millis]) => maps_watch(&host, pid, want, millis),
        ("host-unlink", [dir, entries @ ..]) => unlink(&host, dir, entries),
        _ => Err(format!("bad host probe arguments: {cmd} {args:?}")),
    }
}

/// Measures the retained scan while a path-heavy process is alive in this
/// disposable container. The index figure is a lower bound; VmRSS includes
/// all of the scan and the probe process.
fn inuse_index_memory(host: &LinuxHost) -> Result<(), String> {
    use diskgc_core::inuse::{Options, scan};

    let before_kib = self_status_kib("VmRSS:")?;
    let started = Instant::now();
    let scan = scan(
        host,
        &Options {
            users: &[1000],
            sockets: false,
            pid_lookup: false,
            pytest_facts: false,
            project_facts: false,
        },
        None,
    )
    .map_err(|e| format!("scan for index memory: {e}"))?;
    let during_kib = self_status_kib("VmRSS:")?;
    let peak_kib = self_status_kib("VmHWM:")?;
    let scan_ms = started.elapsed().as_millis();
    let gather_ms = scan.took().as_millis();
    println!(
        "paths={} index_minimum_bytes={} rss_before_kib={} rss_with_scan_kib={} peak_rss_kib={} scan_ms={} gather_ms={} post_gather_ms={}",
        scan.environ_paths().len(),
        scan.environ_index_minimum_bytes(),
        before_kib,
        during_kib,
        peak_kib,
        scan_ms,
        gather_ms,
        scan_ms.saturating_sub(gather_ms)
    );
    Ok(())
}

fn self_status_kib(key: &str) -> Result<usize, String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("read own status: {e}"))?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .ok_or_else(|| format!("missing {key} in own status"))?;
    value
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("empty {key}"))?
        .parse()
        .map_err(|e| format!("parse {key}: {e}"))
}

/// Pin a child while it blocks on a pipe, then reap it and check the same
/// held identity. The VM's separate syscall probe establishes that its 6.6
/// kernel reports POLLIN without HUP after reap, so LinuxHost must use its
/// signal-0 fallback for the final `true` result there.
fn pidfd_release(host: &LinuxHost) -> Result<(), String> {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "IFS= read -r _ || exit 0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn held pidfd child: {e}"))?;

    let result = (|| -> Result<(), String> {
        if child
            .try_wait()
            .map_err(|e| format!("check held child: {e}"))?
            .is_some()
        {
            return Err("pidfd child exited before identity pin".to_owned());
        }
        let pid = Pid(child.id());
        let identity = host
            .proc_identity(pid)
            .map_err(|e| format!("pin live child {pid}: {e}"))?;
        let live_released = host
            .proc_identity_released(&identity)
            .map_err(|e| format!("check live child {pid}: {e}"))?;
        if live_released {
            return Err(format!("live child {pid} was reported released"));
        }

        // EOF releases the blocked child. Keep `identity` alive after waitpid
        // so the result cannot be inferred from a fresh PID lookup.
        drop(child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("wait for pidfd child {pid}: {e}"))?
            {
                break status;
            }
            if Instant::now() >= deadline {
                return Err(format!("pidfd child {pid} did not exit within 5 seconds"));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        if !status.success() {
            return Err(format!("pidfd child {pid} exited with {status}"));
        }
        let reaped_released = host
            .proc_identity_released(&identity)
            .map_err(|e| format!("check reaped child {pid}: {e}"))?;
        if !reaped_released {
            return Err(format!("reaped child {pid} was not reported released"));
        }
        println!("live_released={live_released} reaped_released={reaped_released}");
        Ok(())
    })();

    // On failure, release the pipe and bound the kill/reap cleanup.
    drop(child.stdin.take());
    if child
        .try_wait()
        .map_err(|e| format!("clean up pidfd child: {e}"))?
        .is_none()
    {
        let pid = child.id();
        if let Err(error) = child.kill() {
            // It may have exited between the first try_wait and kill.
            if child
                .try_wait()
                .map_err(|e| format!("check pidfd child {pid} after failed kill: {e}"))?
                .is_none()
            {
                return Err(format!(
                    "kill pidfd child {pid} after probe failure: {error}; probe result: {result:?}"
                ));
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child
                .try_wait()
                .map_err(|e| format!("reap killed pidfd child {pid}: {e}"))?
                .is_some()
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "killed pidfd child {pid} was not reaped within 5 seconds; probe result: {result:?}"
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    result
}

/// An interval on CLOCK_BOOTTIME: a measured scan, or one child's
/// spawn-and-wait as its worker saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: MonoTime,
    end: MonoTime,
}

impl Span {
    fn new(start: MonoTime, end: MonoTime) -> Self {
        assert!(
            start <= end,
            "span ends before it starts: {start:?} > {end:?}"
        );
        Self { start, end }
    }

    fn length(self) -> Duration {
        self.end.saturating_since(self.start)
    }

    /// Whether `inner` lies entirely within this span, ends included.
    fn contains(self, inner: Span) -> bool {
        self.start <= inner.start && inner.end <= self.end
    }
}

/// How many of `children` lie entirely within `scan`: each such child
/// appeared and was reaped while the scanner was listing processes.
fn children_within(scan: Span, children: &[Span]) -> usize {
    children
        .iter()
        .filter(|child| scan.contains(**child))
        .count()
}

/// The same CLOCK_BOOTTIME representation as LinuxHost::now, so child spans
/// can be compared with Scan::started and Scan::took.
fn boot_time() -> MonoTime {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    MonoTime(Duration::new(
        u64::try_from(t.tv_sec).expect("CLOCK_BOOTTIME seconds are non-negative"),
        u32::try_from(t.tv_nsec).expect("tv_nsec is below one second"),
    ))
}

/// Measure the real process scanner while short-lived children appear, then
/// prove that a scan after all children are reaped can clear an unused tree.
/// This runs only in the disposable E2E container.
///
/// Setup condition: for at least one worker count, a whole child
/// spawn-and-wait span lies inside a measured scan, so a process appeared
/// and vanished while the scanner listed. Whether that happens depends on a
/// child's lifetime relative to a scan, which host load stretches unevenly
/// (TASK-197: in a swapping host's 5.8 VM, ~50 ms children against ~60 ms
/// scans fitted none in the former fixed 500 ms window). So workers keep
/// spawning and the probe keeps scanning until one scan contains a whole
/// child, bounded per worker count by OVERLAP_BUDGET and MAX_SCAN_SAMPLES and
/// per worker by MAX_LAUNCHES. If no worker count was exercised within those
/// bounds the probe fails with a "not exercised" error, distinct from a
/// safety failure: a pass without the condition would prove nothing.
///
/// Safety assertions: no sampled scan and no checker result that ended
/// unsettled clears the candidate; after the storm, the unused candidate
/// clears.
fn inuse_fork_churn(host: &LinuxHost, candidate: &str) -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use diskgc_core::engine::{Candidate, Rules, walk};
    use diskgc_core::inuse::{Checker, FreshAttempts, Options, scan};
    use diskgc_core::resolve::{Want, resolve};

    /// Sampling time per worker count. An unloaded host fits a child in a
    /// scan within milliseconds; a loaded VM with ~60 ms scans gets ~130
    /// scans here. Three counts bound the probe's sampling to ~24 s.
    const OVERLAP_BUDGET: Duration = Duration::from_secs(8);
    /// Bounds the scans per worker count when scans take under 2 ms.
    const MAX_SCAN_SAMPLES: usize = 4096;
    /// Bounds each worker's children and the span log; an unloaded host
    /// spawns roughly one /bin/true per millisecond per worker.
    const MAX_LAUNCHES: usize = 8192;
    /// Time for every worker's first launch before sampling starts.
    const READY_WAIT: Duration = Duration::from_secs(5);

    /// One sampled scan, reduced to what the report and the assertions use,
    /// so thousands of samples do not retain thousands of process indexes.
    struct Sample {
        span: Span,
        listings: usize,
        unsettled: bool,
        blocked: bool,
        launched_during: usize,
        completed_during: usize,
    }

    let path = Path::new(candidate);
    let parent_path = path.parent().ok_or("candidate has no parent")?;
    let parent = resolve(host, parent_path, Want::Directory).map_err(|e| e.to_string())?;
    let candidate = Candidate::new(&parent, path).ok_or("candidate is not beneath its parent")?;
    let uid = rustix::process::geteuid().as_raw();
    let users = [uid];
    let rules = Rules::new(&users);
    let record = walk(host, &candidate, &rules, None)
        .map_err(|e| format!("walk unused candidate: {e:?}"))?;
    let options = Options::from_rules(&rules);
    let mut exercised_counts = 0;

    for workers in [0, 1, 4, 8] {
        let stop = Arc::new(AtomicBool::new(false));
        let launched = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let children = Arc::new(Mutex::new(Vec::<Span>::new()));
        let worker_deadline = Instant::now() + READY_WAIT + OVERLAP_BUDGET + Duration::from_secs(1);
        let mut threads = Vec::new();
        for _ in 0..workers {
            let stop = Arc::clone(&stop);
            let launched = Arc::clone(&launched);
            let completed = Arc::clone(&completed);
            let children = Arc::clone(&children);
            threads.push(std::thread::spawn(move || -> Result<(), String> {
                for _ in 0..MAX_LAUNCHES {
                    if stop.load(Ordering::Relaxed) || Instant::now() >= worker_deadline {
                        break;
                    }
                    let before_spawn = boot_time();
                    let mut child = std::process::Command::new("/bin/true")
                        .spawn()
                        .map_err(|e| format!("spawn /bin/true: {e}"))?;
                    launched.fetch_add(1, Ordering::SeqCst);
                    let status = child
                        .wait()
                        .map_err(|e| format!("wait for /bin/true: {e}"))?;
                    let after_wait = boot_time();
                    if !status.success() {
                        return Err(format!("/bin/true exited with {status}"));
                    }
                    children
                        .lock()
                        .map_err(|_| "fork span lock poisoned".to_owned())?
                        .push(Span::new(before_spawn, after_wait));
                    completed.fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            }));
        }
        let counts = || {
            (
                launched.load(Ordering::SeqCst),
                completed.load(Ordering::SeqCst),
            )
        };
        if workers > 0 {
            let ready_by = Instant::now() + READY_WAIT;
            while counts().0 < workers && Instant::now() < ready_by {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        let overlap_deadline = Instant::now() + OVERLAP_BUDGET;
        let take_sample = || -> Result<Sample, String> {
            let before = counts();
            let measured = scan(host, &options, None)
                .map_err(|e| format!("scan with {workers} workers: {e}"))?;
            let after = counts();
            let start = measured.started();
            let end = start
                .checked_add(measured.took())
                .expect("scan end fits in CLOCK_BOOTTIME");
            Ok(Sample {
                span: Span::new(start, end),
                listings: measured.listings(),
                unsettled: measured
                    .notes()
                    .iter()
                    .any(|note| note.starts_with("process scan unsettled:")),
                blocked: measured.check(&candidate, &record).is_err(),
                launched_during: after.0.saturating_sub(before.0),
                completed_during: after.1.saturating_sub(before.1),
            })
        };

        // Every error below is held until the workers are stopped and
        // joined, so no worker outlives the worker count it belongs to.
        let mut failure = None;
        let mut samples = Vec::new();
        let mut checked = None;
        match take_sample() {
            Ok(sample) => samples.push(sample),
            Err(e) => failure = Some(e),
        }
        let mut checker = Checker::new(&rules, Duration::ZERO);
        let before_check = counts();
        if failure.is_none() {
            checked = Some(checker.check(host, &candidate, &record, &mut FreshAttempts::new(None)));
        }
        let after_check = counts();
        // Early stop only: a worker pushes its span after the wait, so a
        // span can land after its scan was looked at. Looking at the last
        // two samples catches most late spans; the exact count is made
        // over every sample after the workers are joined.
        while failure.is_none()
            && workers > 0
            && samples.len() < MAX_SCAN_SAMPLES
            && Instant::now() < overlap_deadline
            && counts().1 < workers * MAX_LAUNCHES
        {
            let witnessed = match children.lock() {
                Ok(children) => samples
                    .iter()
                    .rev()
                    .take(2)
                    .any(|sample| children_within(sample.span, &children) > 0),
                Err(_) => {
                    failure = Some("fork span lock poisoned".to_owned());
                    break;
                }
            };
            if witnessed {
                break;
            }
            match take_sample() {
                Ok(sample) => samples.push(sample),
                Err(e) => failure = Some(e),
            }
        }
        if failure.is_none()
            && let Some(sample) = samples.iter().find(|s| s.unsettled && !s.blocked)
        {
            failure = Some(format!(
                "{workers}-worker scan was unsettled but cleared the candidate ({} listings)",
                sample.listings
            ));
        }
        stop.store(true, Ordering::Relaxed);
        let mut worker_error = None;
        for thread in threads {
            let outcome = thread
                .join()
                .map_err(|_| "fork worker panicked".to_owned())?;
            if let Err(error) = outcome {
                worker_error.get_or_insert(error);
            }
        }
        if let Some(error) = failure.or(worker_error) {
            return Err(error);
        }
        let (launched, completed) = counts();
        if workers > 0 && launched < workers {
            return Err(format!(
                "only {launched} child launches occurred with {workers} workers"
            ));
        }
        let children = children
            .lock()
            .map_err(|_| "fork span lock poisoned".to_owned())?;
        let witness = samples
            .iter()
            .map(|sample| (sample, children_within(sample.span, &children)))
            .find(|(_, within)| *within > 0);
        let (measured, events_within_scan) = witness.unwrap_or((&samples[0], 0));
        let exercised = workers > 0 && events_within_scan > 0;
        exercised_counts += usize::from(exercised);

        let checker_blocked = checked
            .expect("the checker ran when the first sample succeeded")
            .map_err(|e| format!("checker with {workers} workers: {e}"))?
            .is_err();
        let notes = checker.latest().expect("checker made a scan").notes();
        let retried = notes.iter().any(|note| note.contains("retried after"));
        let checker_unsettled = notes
            .iter()
            .any(|note| note.starts_with("process scan unsettled:"));
        if checker_unsettled && !checker_blocked {
            return Err(format!(
                "{workers}-worker checker ended unsettled but cleared the candidate"
            ));
        }
        // Whether a fit was possible at all: a child shorter than the
        // longest scan. These explain a "not exercised" outcome.
        let child_ms_min = children
            .iter()
            .map(|child| child.length().as_millis())
            .min()
            .map_or("-".to_owned(), |ms| ms.to_string());
        let scan_ms_max = samples
            .iter()
            .map(|sample| sample.span.length().as_millis())
            .max()
            .expect("at least one sample");
        println!(
            "workers={workers} exercised={} samples={} launched={launched} completed={completed} events_within_scan={events_within_scan} child_ms_min={child_ms_min} scan_ms_max={scan_ms_max} launched_during_scan={} completed_during_scan={} launched_during_check={} completed_during_check={} listings={} took_ms={} unsettled={} blocked={} checker_retried={retried} checker_unsettled={checker_unsettled} checker_blocked={checker_blocked}",
            yes(exercised),
            samples.len(),
            measured.launched_during,
            measured.completed_during,
            after_check.0.saturating_sub(before_check.0),
            after_check.1.saturating_sub(before_check.1),
            measured.listings,
            measured.span.length().as_millis(),
            measured.unsettled,
            measured.blocked,
        );
    }
    if exercised_counts == 0 {
        return Err(format!(
            "not exercised: no worker count put an entire child spawn-and-wait \
             interval inside a measured in-use scan within {OVERLAP_BUDGET:?} or \
             {MAX_SCAN_SAMPLES} scans per count, so the unsettled-scan assertions \
             were not tested (compare child_ms_min with scan_ms_max above)"
        ));
    }

    let settled = scan(host, &options, None).map_err(|e| format!("post-storm scan: {e}"))?;
    let result = settled.check(&candidate, &record);
    println!(
        "post_storm listings={} took_ms={} clear={}",
        settled.listings(),
        settled.took().as_millis(),
        result.is_ok()
    );
    result.map_err(|e| format!("unused candidate remained blocked after fork storm: {e:?}"))
}

/// Run a continuous, bounded fork storm while repeatedly asking the real
/// checker about one unused candidate. This is a load measurement, so exact
/// counts are reported rather than assumed; an unsettled scan must always
/// block, and the candidate must clear when the storm ends.
fn inuse_sustained_churn(host: &LinuxHost, candidate: &str) -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    use diskgc_core::engine::{Candidate, Rules, walk};
    use diskgc_core::inuse::{Checker, FreshAttempts, MAX_LISTINGS, Options, scan};
    use diskgc_core::resolve::{Want, resolve};

    const WORKERS: usize = 8;
    const WINDOW: Duration = Duration::from_secs(30);

    let path = Path::new(candidate);
    let parent_path = path.parent().ok_or("candidate has no parent")?;
    let parent = resolve(host, parent_path, Want::Directory).map_err(|e| e.to_string())?;
    let candidate = Candidate::new(&parent, path).ok_or("candidate is not beneath its parent")?;
    let users = [rustix::process::geteuid().as_raw()];
    let rules = Rules::new(&users);
    let record = walk(host, &candidate, &rules, None)
        .map_err(|e| format!("walk unused candidate: {e:?}"))?;
    let options = Options::from_rules(&rules);
    let baseline = scan(host, &options, None).map_err(|e| format!("baseline scan: {e}"))?;
    baseline
        .check(&candidate, &record)
        .map_err(|e| format!("unused candidate blocked before load: {e:?}"))?;

    let start = Arc::new(Barrier::new(WORKERS + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();
    for _ in 0..WORKERS {
        let start = Arc::clone(&start);
        let stop = Arc::clone(&stop);
        let completed = Arc::clone(&completed);
        threads.push(std::thread::spawn(move || -> Result<(), String> {
            start.wait();
            while !stop.load(Ordering::Relaxed) {
                let status = Command::new("/bin/true")
                    .status()
                    .map_err(|e| format!("spawn/wait /bin/true: {e}"))?;
                if !status.success() {
                    return Err(format!("/bin/true exited with {status}"));
                }
                completed.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }));
    }
    let started = Instant::now();
    let until = started + WINDOW;
    start.wait();
    let mut checker = Checker::new(&rules, Duration::ZERO);
    let mut samples = 0usize;
    let mut unsettled = 0usize;
    let mut blocked = 0usize;
    let mut progressed = 0usize;
    let mut growth_limited = 0usize;
    let mut max_listings = 0usize;
    let mut max_ms = 0u128;
    let mut scan_error = None;
    let growth_limit_reason =
        format!("after {MAX_LISTINGS} listings that brought new process incarnations");
    while Instant::now() < until {
        let before = completed.load(Ordering::Relaxed);
        let result = checker.check(host, &candidate, &record, &mut FreshAttempts::new(None));
        let after = completed.load(Ordering::Relaxed);
        match result {
            Ok(judgement) => {
                let latest = checker.latest().expect("checker made a scan");
                let notes = latest.notes();
                let incomplete = notes
                    .iter()
                    .any(|note| note.starts_with("process scan unsettled:"));
                if incomplete && judgement.is_ok() {
                    scan_error = Some("unsettled loaded scan cleared a candidate".to_owned());
                    break;
                }
                samples += 1;
                unsettled += usize::from(incomplete);
                blocked += usize::from(judgement.is_err());
                growth_limited += usize::from(notes.iter().any(|note| {
                    note.starts_with("process scan unsettled:")
                        && note.contains(&growth_limit_reason)
                }));
                max_listings = max_listings.max(latest.listings());
                max_ms = max_ms.max(latest.took().as_millis());
                // Diagnostic only: completion can race the check's return.
                progressed += after.saturating_sub(before);
            }
            Err(e) => {
                scan_error = Some(format!("checker under load: {e}"));
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, Ordering::Relaxed);
    let mut worker_error = None;
    for thread in threads {
        match thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                worker_error.get_or_insert(error);
            }
            Err(_) => {
                worker_error.get_or_insert("fork worker panicked".to_owned());
            }
        }
    }
    if let Some(error) = worker_error.or(scan_error) {
        return Err(error);
    }
    let elapsed = started.elapsed();
    let children = completed.load(Ordering::Relaxed);
    if elapsed < WINDOW || samples == 0 || children < WORKERS || unsettled == 0 {
        return Err(format!(
            "load was not sustained or never unsettled: elapsed={elapsed:?} samples={samples} children={children} unsettled={unsettled}"
        ));
    }
    if growth_limited == 0 {
        return Err(format!(
            "loaded scanner did not exercise the {MAX_LISTINGS}-growth-listing limit: max_listings={max_listings}"
        ));
    }
    let after = scan(host, &options, None).map_err(|e| format!("post-load scan: {e}"))?;
    after
        .check(&candidate, &record)
        .map_err(|e| format!("unused candidate remained blocked after load: {e:?}"))?;
    println!(
        "elapsed_ms={} workers={WORKERS} children={children} samples={samples} unsettled={unsettled} blocked={blocked} growth_limited={growth_limited} completed_during_checks={progressed} max_listings={max_listings} max_scan_ms={max_ms} post_load_clear=true",
        elapsed.as_millis()
    );
    Ok(())
}

/// In a disposable container subprocess, replace the pathname after the
/// host has pinned procfs. Its readers must still use the original mount.
fn proc_replaced_after_startup(host: &LinuxHost) -> Result<(), String> {
    use rustix::mount::{
        MountFlags, MountPropagationFlags, UnmountFlags, mount, mount_change, unmount,
    };

    let own = Pid(std::process::id());
    if !host.pids().map_err(|e| e.to_string())?.contains(&own) {
        return Err("original procfs does not list this probe".into());
    }
    // SAFETY: this probe is a fresh single-threaded subprocess, and the
    // mount operations must not be visible in the test harness namespace.
    unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWNS) }
        .map_err(|e| format!("unshare mount namespace: {e}"))?;
    mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(|e| format!("make mount namespace private: {e}"))?;
    // This procfs mount belongs to the probe's private namespace, so it
    // can be lazily detached even if the container's inherited mounts are
    // locked against unmounting by the runtime.
    mount("proc", "/proc", "proc", MountFlags::empty(), None)
        .map_err(|e| format!("mount private procfs: {e}"))?;
    let pinned = LinuxHost::new().map_err(|e| format!("open private procfs: {e}"))?;
    mount("tmpfs", "/proc", "tmpfs", MountFlags::empty(), None)
        .map_err(|e| format!("mount tmpfs over /proc: {e}"))?;
    if std::fs::exists("/proc/self/status").map_err(|e| format!("check overmount: {e}"))? {
        return Err("tmpfs overmount did not hide /proc/self/status".into());
    }
    let refused = LinuxHost::new().expect_err("new host must refuse a non-procfs /proc");
    if !refused.to_string().contains("not procfs") {
        return Err(format!("wrong refusal for non-procfs /proc: {refused}"));
    }
    if !host.pids().map_err(|e| e.to_string())?.contains(&own) {
        return Err("held procfs stopped listing this probe after overmount".into());
    }
    if !pinned.pids().map_err(|e| e.to_string())?.contains(&own) {
        return Err("private procfs stopped listing this probe after overmount".into());
    }
    unmount("/proc", UnmountFlags::DETACH).map_err(|e| format!("lazily detach overmount: {e}"))?;
    if !std::fs::exists("/proc/self/status").map_err(|e| format!("check exposed procfs: {e}"))? {
        return Err("private procfs did not reappear after lazy detach".into());
    }
    if !host.pids().map_err(|e| e.to_string())?.contains(&own) {
        return Err("held procfs stopped listing this probe after lazy detach".into());
    }
    unmount("/proc", UnmountFlags::DETACH)
        .map_err(|e| format!("lazily detach pinned procfs: {e}"))?;
    if !std::fs::exists("/proc/self/status").map_err(|e| format!("check base procfs: {e}"))? {
        return Err("container procfs did not reappear after lazy detach".into());
    }
    if !pinned.pids().map_err(|e| e.to_string())?.contains(&own) {
        return Err("detached procfs stopped listing this probe".into());
    }
    println!("held_procfs=true overmount_refused=true detached_procfs_readable=true");
    Ok(())
}

/// A short, stable name for why a host call failed.
pub fn cause_tag(e: &HostError) -> String {
    match e.cause {
        Cause::Errno(errno) => errno
            .name()
            .map_or_else(|| format!("errno{}", errno.0), str::to_owned),
        Cause::Invalid => "invalid".to_owned(),
        Cause::SandboxGate => "sandbox_gate".to_owned(),
    }
}

/// `host-unlink DIR NAME:file|dir...`: resolves `DIR`, then for each entry
/// checks its identity (`statx` of the name, as the deletion engine does
/// right before every unlink) and unlinks it through `LinuxHost`, so through
/// the pre-release sandbox gate.
fn unlink(host: &LinuxHost, dir: &str, entries: &[String]) -> Result<(), String> {
    let dir = resolve(host, Path::new(dir), Want::Directory).map_err(|e| e.to_string())?;
    for (i, entry) in entries.iter().enumerate() {
        let (name, what) = match entry.rsplit_once(':') {
            Some((name, "file")) => (name, Unlink::NonDir),
            Some((name, "dir")) => (name, Unlink::Dir),
            _ => {
                return Err(format!(
                    "entry must be NAME:file or NAME:dir, not {entry:?}"
                ));
            }
        };
        let unlinked = host
            .statx(Base::Dir(&dir.fd), Path::new(name))
            .and_then(|_| host.unlinkat(&dir.fd, OsStr::new(name), what));
        match unlinked {
            Ok(()) => println!("{i} ok"),
            Err(e) => println!("{i} err {} -- {e}", cause_tag(&e)),
        }
    }
    Ok(())
}

fn want(text: &str) -> Result<Want, String> {
    match text {
        "dir" => Ok(Want::Directory),
        "any" => Ok(Want::Any),
        other => Err(format!("want must be dir or any, not {other:?}")),
    }
}

/// A short, stable name for why resolution failed.
pub fn tag(e: &ResolveError) -> &'static str {
    match e {
        ResolveError::UnsafePath { why, .. }
        | ResolveError::Screened {
            why: MountScreenError::Unsafe { why, .. },
            ..
        } => match why {
            Unsafe::NotAbsolute => "not_absolute",
            Unsafe::NotNormal => "not_normal",
            Unsafe::Symlink => "symlink",
            Unsafe::CrossesMount => "crosses_mount",
            Unsafe::NoMount => "no_mount",
            Unsafe::AmbiguousMounts(_) => "ambiguous_mounts",
            Unsafe::MountMismatch { .. } => "mount_mismatch",
            Unsafe::LeftMount { .. } => "left_mount",
            Unsafe::Unplaceable(_) => "unplaceable",
        },
        ResolveError::Screened {
            why: MountScreenError::Networked { .. },
            ..
        } => "networked",
        ResolveError::Screened {
            why: MountScreenError::PrivateTmp { .. },
            ..
        } => "private_tmp",
        ResolveError::UnsupportedFs { .. } => "unanswered",
        ResolveError::Missing { .. } => "missing",
        ResolveError::Host(_) => "host",
    }
}

fn ok_line(i: usize, stat: &Stat) -> String {
    let opt = |v: Option<u64>| v.map_or("none".to_owned(), |v| v.to_string());
    format!(
        "{i} ok mnt={} dev={} ino={}",
        opt(stat.mnt_id),
        stat.dev,
        opt(stat.ino)
    )
}

fn err_line(i: usize, e: &ResolveError) -> String {
    format!("{i} err {} {} -- {e}", e.transcript_label(), tag(e))
}

fn resolve_paths(host: &LinuxHost, want_text: &str, paths: &[String]) -> Result<(), String> {
    let want = want(want_text)?;
    for (i, path) in paths.iter().enumerate() {
        match resolve(host, Path::new(path), want) {
            Ok(r) => println!("{}", ok_line(i, &r.stat)),
            Err(e) => println!("{}", err_line(i, &e)),
        }
    }
    Ok(())
}

fn beneath(host: &LinuxHost, root: &str, want_text: &str, rels: &[String]) -> Result<(), String> {
    let want = want(want_text)?;
    let root = resolve(host, Path::new(root), Want::Directory).map_err(|e| e.to_string())?;
    for (i, rel) in rels.iter().enumerate() {
        match open_beneath(
            host,
            &root.fd,
            root.mount.id,
            Path::new(rel),
            Access::Path,
            want,
        ) {
            Ok((_, stat)) => println!("{}", ok_line(i, &stat)),
            Err(e) => println!("{}", err_line(i, &e)),
        }
    }
    Ok(())
}

/// Exercises the rare `Access::Read, Want::Any` opening with a FIFO whose
/// writer is absent. The alarm bounds a regression that omits O_NONBLOCK:
/// a blocking open must fail the test promptly instead of consuming the
/// whole container test budget.
fn read_fifo(host: &LinuxHost, root: &str, rel: &str) -> Result<(), String> {
    let root = resolve(host, Path::new(root), Want::Directory).map_err(|e| e.to_string())?;
    // SAFETY: this probe runs alone in a disposable process; SIGALRM's
    // default action terminates a blocked open after two seconds.
    unsafe { libc::alarm(2) };
    let (fd, stat) = open_beneath(
        host,
        &root.fd,
        root.mount.id,
        Path::new(rel),
        Access::Read,
        Want::Any,
    )
    .map_err(|e| e.to_string())?;
    unsafe { libc::alarm(0) };
    if stat.file_type != Some(diskgc_core::host::FileType::Fifo) {
        return Err(format!("expected a FIFO, found {:?}", stat.file_type));
    }
    let flags = rustix::fs::fcntl_getfl(fd.as_fd()).map_err(|e| format!("fcntl FIFO: {e}"))?;
    // F_GETFL does not retain O_NOCTTY, which affects only the open itself.
    println!("nonblock={}", flags.contains(rustix::fs::OFlags::NONBLOCK));
    Ok(())
}

/// `I mnt_id=<id|none> mount_root=<bool|none> btime=<some|none>`
fn stat_paths(host: &LinuxHost, paths: &[String]) -> Result<(), String> {
    for (i, path) in paths.iter().enumerate() {
        let r = resolve(host, Path::new(path), Want::Any).map_err(|e| e.to_string())?;
        let s = &r.stat;
        println!(
            "{i} mnt_id={} mount_root={} btime={}",
            s.mnt_id.map_or("none".to_owned(), |v| v.to_string()),
            s.mount_root.map_or("none".to_owned(), |v| v.to_string()),
            if s.btime.is_some() { "some" } else { "none" },
        );
    }
    Ok(())
}

fn statvfs(host: &LinuxHost, path: &str) -> Result<(), String> {
    let r = resolve(host, Path::new(path), Want::Directory).map_err(|e| e.to_string())?;
    let space = host.statvfs(&r.fd).map_err(|e| e.to_string())?;
    println!("avail={} size={}", space.avail_bytes, space.size_bytes);
    Ok(())
}

/// Opens `path` readable for `flock`: its parent is resolved, then the name
/// is opened beneath it (the way diskgc reaches the host lock).
fn open_for_lock(host: &LinuxHost, path: &str) -> Result<LinuxFd, String> {
    let path = Path::new(path);
    let (parent, name) = path
        .parent()
        .zip(path.file_name())
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    let dir = resolve(host, parent, Want::Directory).map_err(|e| e.to_string())?;
    let (fd, _) = open_beneath(
        host,
        &dir.fd,
        dir.mount.id,
        Path::new(name),
        Access::Read,
        Want::Any,
    )
    .map_err(|e| e.to_string())?;
    Ok(fd)
}

fn lock_try(host: &LinuxHost, path: &str) -> Result<(), String> {
    let fd = open_for_lock(host, path)?;
    match host
        .flock_try(fd, LockMode::Exclusive)
        .map_err(|e| e.to_string())?
    {
        TryLock::Acquired(_) => println!("acquired"),
        TryLock::Busy(_) => println!("busy"),
    }
    Ok(())
}

fn lock_wait(
    host: &LinuxHost,
    path: &str,
    millis: &str,
    busy_marker: Option<&str>,
) -> Result<(), String> {
    let millis: u64 = millis
        .parse()
        .map_err(|_| format!("bad millis {millis:?}"))?;
    let mut fd = open_for_lock(host, path)?;
    if let Some(marker) = busy_marker {
        // The marker lets the harness release the holder only after this
        // separate fd has observed contention. Keep the same unlocked fd.
        fd = match host
            .flock_try(fd, LockMode::Exclusive)
            .map_err(|e| e.to_string())?
        {
            TryLock::Busy(fd) => fd,
            TryLock::Acquired(_) => return Err("lock was free before the wait began".into()),
        };
        #[expect(
            clippy::disallowed_methods,
            reason = "the busy marker the harness asked this probe to write on scratch"
        )]
        std::fs::write(marker, b"busy\n")
            .map_err(|e| format!("write busy marker {marker}: {e}"))?;
    }
    let started = Instant::now();
    let deadline = host.now().checked_add(Duration::from_millis(millis));
    let outcome = match host
        .flock_wait(fd, LockMode::Exclusive, deadline)
        .map_err(|e| e.to_string())?
    {
        WaitLock::Acquired(_) => "Acquired",
        WaitLock::DeadlinePassed(_) => "DeadlinePassed",
        WaitLock::Interrupted(_) => "Interrupted",
    };
    println!("{outcome} after_ms={}", started.elapsed().as_millis());
    Ok(())
}

/// Whether `/proc/locks` lists a granted `FLOCK` on the inode of `stat`.
fn flock_listed(stat: &Stat) -> Result<bool, String> {
    let ino = stat.ino.ok_or("inode not answered")?;
    let want = format!("{:02x}:{:02x}:{ino}", stat.dev.major, stat.dev.minor);
    let locks =
        std::fs::read_to_string("/proc/locks").map_err(|e| format!("read /proc/locks: {e}"))?;
    Ok(locks.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Granted locks: "N: FLOCK ADVISORY WRITE pid dev:ino start end";
        // waiters have "->" as the second field.
        fields.get(1) == Some(&"FLOCK") && fields.get(5) == Some(&want.as_str())
    }))
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// Whether `/proc/locks` stops listing the lock on `stat` within the
/// release bound.
fn flock_released(stat: &Stat) -> Result<bool, String> {
    within_release_bound(|| flock_listed(stat).map(|listed| !listed))
}

/// The host lock's rule (docs/safety.md, Host lock): a lock is released
/// only by closing its fd, so a child that inherited the fd keeps the lock
/// after diskgc closes its own copy, and it goes when the last copy closes.
/// A `flock(LOCK_UN)` anywhere on the way (e.g. in a drop) would release it
/// for the child too, and `held_after_parent_close` would say `no`.
fn close_semantics(host: &LinuxHost, path: &str) -> Result<(), String> {
    // Plain case: closing the only fd releases the lock.
    let fd = open_for_lock(host, path)?;
    let stat = host
        .statx(diskgc_core::host::Base::Dir(&fd), Path::new(""))
        .map_err(|e| e.to_string())?;
    let TryLock::Acquired(fd) = host
        .flock_try(fd, LockMode::Exclusive)
        .map_err(|e| e.to_string())?
    else {
        return Err(format!("{path} is locked by someone else"));
    };
    println!("held_while_open={}", yes(flock_listed(&stat)?));
    drop(fd);
    println!("released_on_close={}", yes(flock_released(&stat)?));

    // Inherited case: a child gets the fd (close-on-exec cleared on this
    // one fd, as diskgc will do for the host lock) and outlives our copy.
    let fd = open_for_lock(host, path)?;
    let TryLock::Acquired(fd) = host
        .flock_try(fd, LockMode::Exclusive)
        .map_err(|e| e.to_string())?
    else {
        return Err(format!("{path} is locked by someone else"));
    };
    rustix::io::fcntl_setfd(fd.fd().as_fd(), rustix::io::FdFlags::empty())
        .map_err(|e| format!("clear FD_CLOEXEC: {e}"))?;
    let mut child = std::process::Command::new("/bin/cat")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn /bin/cat: {e}"))?;
    drop(fd);
    println!("held_after_parent_close={}", yes(flock_listed(&stat)?));
    drop(child.stdin.take());
    child.wait().map_err(|e| format!("wait for cat: {e}"))?;
    println!("released_after_last_close={}", yes(flock_released(&stat)?));
    Ok(())
}

fn proc_result<T>(what: &str, r: Result<T, ProcError>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(ProcError::Gone) => {
            println!("{what} gone");
            None
        }
        Err(ProcError::Denied(_)) => {
            println!("{what} denied");
            None
        }
        Err(ProcError::Failed(e)) => {
            println!("{what} failed {e}");
            None
        }
        Err(ProcError::Unsettled(e)) => {
            println!("{what} unsettled {e}");
            None
        }
        Err(ProcError::Exhausted(e)) => {
            println!("{what} exhausted {e}");
            None
        }
    }
}

/// Prints what the `/proc` readers return for `pid`:
/// `fd N <dev:ino>`, `cwd|root|exe <dev:ino>`, `map <dev:ino>` (file
/// mappings only), `env NAME=VALUE`, or `<what> gone|denied|failed|unsettled
/// ...`.
fn proc_readers(host: &LinuxHost, pid: &str) -> Result<(), String> {
    let pid = Pid(pid.parse().map_err(|_| format!("bad pid {pid:?}"))?);
    let id = |s: &Stat| {
        s.dev_ino()
            .map_or("unanswered".to_owned(), |d| d.to_string())
    };
    if let Some(fds) = proc_result("fds", host.proc_fds(pid)) {
        for fd in fds {
            println!("fd {} {}", fd.fd, id(&fd.target.stat));
        }
    }
    for which in [ProcLink::Cwd, ProcLink::Root, ProcLink::Exe] {
        if let Some(targets) = proc_result(which.name(), host.proc_links(pid, which)) {
            for t in targets {
                println!("{} {}", which.name(), id(&t.stat));
            }
        }
    }
    if let Some(maps) = proc_result("maps", host.proc_maps(pid)) {
        for m in maps.iter().filter(|m| m.ino != 0) {
            println!("map {}:{}", m.dev, m.ino);
        }
    }
    if let Some(env) = proc_result("environ", host.proc_environ(pid)) {
        for (name, value) in env.vars() {
            println!("env {}={}", name.to_string_lossy(), value.to_string_lossy());
        }
    }
    Ok(())
}

fn kernel(host: &LinuxHost) -> Result<(), String> {
    let release = host.kernel_release().map_err(|e| e.to_string())?;
    match diskgc_core::kernel::check_release(&release) {
        Ok(v) => println!("release={release} supported={v}"),
        Err(e) => println!("release={release} refused -- {e}"),
    }
    Ok(())
}

/// `hold FILE CWD`: a stand-in for a process using files (a fixture, not
/// code under test): keeps FILE open, makes CWD its working directory,
/// prints `ready`, then waits until its stdin closes.
pub fn hold(file: &Path, cwd: &Path) -> Result<(), String> {
    let _held = std::fs::File::open(file).map_err(|e| format!("open {}: {e}", file.display()))?;
    std::env::set_current_dir(cwd).map_err(|e| format!("chdir {}: {e}", cwd.display()))?;
    println!("ready");
    std::io::stdout()
        .flush()
        .map_err(|e| format!("flush stdout: {e}"))?;
    let mut line = String::new();
    while std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("read stdin: {e}"))?
        > 0
    {
        line.clear();
    }
    Ok(())
}

/// `hold-thread FILE CWD MODE`: a fixture process whose use of FILE and CWD
/// is visible only through a thread other than the main one. A worker
/// thread opens FILE and makes CWD its working directory, then:
///
/// - `main-exits`: the main thread ends (`pthread_exit`), leaving a zombie
///   thread-group leader whose `/proc/<pid>/fd` is empty and whose `cwd`
///   gives `ENOENT`;
/// - `unshare`: the worker first takes its own fd table and filesystem
///   state (`unshare(CLONE_FILES | CLONE_FS)`), so the main thread's
///   entries never show FILE or CWD.
///
/// Prints `ready` once that state holds, and exits the whole process when
/// its stdin closes.
pub fn hold_thread(file: &Path, cwd: &Path, mode: &str) -> Result<(), String> {
    let unshare = match mode {
        "main-exits" => false,
        "unshare" => true,
        other => {
            return Err(format!(
                "hold-thread mode must be main-exits or unshare, not {other:?}"
            ));
        }
    };
    let main_tid = rustix::process::getpid().as_raw_nonzero().get();
    let (file, cwd) = (file.to_owned(), cwd.to_owned());
    let (opened_tx, opened_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let worker = std::thread::spawn(move || {
        let setup = (|| -> Result<std::fs::File, String> {
            if unshare {
                // SAFETY: this thread uses no fd that another thread owns.
                unsafe {
                    rustix::thread::unshare_unsafe(
                        rustix::thread::UnshareFlags::FILES | rustix::thread::UnshareFlags::FS,
                    )
                }
                .map_err(|e| format!("unshare: {e}"))?;
            }
            let held =
                std::fs::File::open(&file).map_err(|e| format!("open {}: {e}", file.display()))?;
            std::env::set_current_dir(&cwd).map_err(|e| format!("chdir {}: {e}", cwd.display()))?;
            Ok(held)
        })();
        let held = match setup {
            Ok(held) => held,
            Err(e) => {
                let _ = opened_tx.send(Err(e));
                return;
            }
        };
        let _ = opened_tx.send(Ok(()));
        if !unshare && let Err(e) = wait_for_zombie(main_tid) {
            eprintln!("hold-thread: {e}");
            std::process::exit(crate::exit::FAIL);
        }
        println!("ready");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        while std::io::stdin().lock().read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
        drop(held);
        std::process::exit(crate::exit::PASS);
    });
    opened_rx
        .recv()
        .map_err(|_| "worker ended before opening".to_owned())??;
    if unshare {
        worker.join().map_err(|_| "worker panicked".to_owned())?;
        return Ok(());
    }
    // SAFETY: ends only the main thread; on musl (the e2e build) this runs
    // no unwinding through Rust frames. The worker exits the process.
    unsafe { libc::pthread_exit(std::ptr::null_mut()) }
}

/// Waits (up to 5 s) until task `tid` of this process is a zombie.
fn wait_for_zombie(tid: i32) -> Result<(), String> {
    wait_for_zombie_within(tid, Duration::from_secs(5))
}

/// `host-maps-watch PID DEV:INO MILLIS`: calls `proc_maps(PID)` in a loop
/// for MILLIS ms and counts the outcomes, and, independently, how often the
/// thread-group leader was seen exiting (`PF_EXITING`, not yet a zombie).
/// Prints one line: `calls= with= without= gone= failed= unsettled= denied=
/// leader_exiting_seen=`, where `with`/`without` count `Ok` results that do
/// or do not contain a mapping of DEV:INO.
fn maps_watch(host: &LinuxHost, pid: &str, want: &str, millis: &str) -> Result<(), String> {
    let pid_n: u32 = pid.parse().map_err(|_| format!("bad pid {pid:?}"))?;
    let millis: u64 = millis
        .parse()
        .map_err(|_| format!("bad millis {millis:?}"))?;
    let leader_stat = format!("/proc/{pid_n}/task/{pid_n}/stat");
    let (mut calls, mut with, mut without, mut gone, mut failed, mut unsettled) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut denied, mut exiting) = (0u64, 0u64);
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(millis) {
        if let Ok(stat) = std::fs::read_to_string(&leader_stat)
            && let Some((_, rest)) = stat.rsplit_once(')')
        {
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let flags: u64 = fields.get(6).and_then(|f| f.parse().ok()).unwrap_or(0);
            if fields.first() != Some(&"Z") && flags & 0x4 != 0 {
                exiting += 1;
            }
        }
        calls += 1;
        match host.proc_maps(Pid(pid_n)) {
            Ok(maps) if maps.iter().any(|m| format!("{}:{}", m.dev, m.ino) == want) => with += 1,
            Ok(_) => without += 1,
            Err(ProcError::Gone) => gone += 1,
            Err(ProcError::Failed(_) | ProcError::Exhausted(_)) => failed += 1,
            Err(ProcError::Unsettled(_)) => unsettled += 1,
            Err(ProcError::Denied(_)) => denied += 1,
        }
    }
    println!(
        "calls={calls} with={with} without={without} gone={gone} failed={failed} \
         unsettled={unsettled} denied={denied} leader_exiting_seen={exiting}"
    );
    Ok(())
}

/// `hold-exit-slowly`: a fixture process whose main thread exits slowly
/// while a worker keeps the process (and its memory map) alive. The worker
/// takes its own fd table first (`unshare(CLONE_FILES)`); the main thread
/// then opens as many files as RLIMIT_NOFILE allows (up to 400,000) in its
/// now private table, prints `ready N`, and on a line from stdin ends
/// itself (`pthread_exit`). Closing those files at exit keeps it in
/// `PF_EXITING`, with its memory map already released, for a while. The
/// worker exits the process when stdin closes.
pub fn hold_exit_slowly() -> Result<(), String> {
    let main_tid = rustix::process::getpid().as_raw_nonzero().get();
    let (unshared_tx, unshared_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::spawn(move || {
        // SAFETY: this thread uses no fd that another thread owns.
        let unshared =
            unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::FILES) }
                .map_err(|e| format!("unshare: {e}"));
        let ok = unshared.is_ok();
        let _ = unshared_tx.send(unshared);
        if !ok {
            return;
        }
        if let Err(e) = wait_for_zombie_within(main_tid, Duration::from_secs(60)) {
            eprintln!("hold-exit-slowly: {e}");
            std::process::exit(crate::exit::FAIL);
        }
        let mut line = String::new();
        while std::io::stdin().lock().read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
        std::process::exit(crate::exit::PASS);
    });
    unshared_rx
        .recv()
        .map_err(|_| "worker ended before unsharing".to_owned())??;

    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    let hard = limit.maximum.unwrap_or(1 << 20);
    rustix::process::setrlimit(
        rustix::process::Resource::Nofile,
        rustix::process::Rlimit {
            current: Some(hard),
            maximum: limit.maximum,
        },
    )
    .map_err(|e| format!("raise RLIMIT_NOFILE to {hard}: {e}"))?;
    let count = hard.saturating_sub(64).min(400_000);
    let mut files = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for _ in 0..count {
        files.push(std::fs::File::open("/dev/null").map_err(|e| format!("open /dev/null: {e}"))?);
    }
    println!("ready {}", files.len());
    std::io::stdout()
        .flush()
        .map_err(|e| format!("flush stdout: {e}"))?;
    let mut go = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut go)
        .map_err(|e| format!("read stdin: {e}"))?;
    // SAFETY: ends only the main thread; on musl (the e2e build) no
    // unwinding runs through Rust frames, so `files` stays open until the
    // kernel closes them in this thread's exit.
    unsafe { libc::pthread_exit(std::ptr::null_mut()) }
}

/// Waits until task `tid` of this process is a zombie, polling every 10 ms.
fn wait_for_zombie_within(tid: i32, limit: Duration) -> Result<(), String> {
    let path = format!("/proc/self/task/{tid}/stat");
    let started = Instant::now();
    while started.elapsed() < limit {
        let stat = std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next());
        if state == Some("Z") {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(format!(
        "task {tid} did not become a zombie within {limit:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start_ms: u64, end_ms: u64) -> Span {
        Span::new(
            MonoTime(Duration::from_millis(start_ms)),
            MonoTime(Duration::from_millis(end_ms)),
        )
    }

    #[test]
    fn only_a_child_wholly_inside_the_scan_counts_ends_included() {
        let scan = span(100, 160);
        let children = [
            span(100, 160), // exactly the scan: spawned and reaped inside it
            span(110, 150), // inside
            span(90, 120),  // spawned before the scan started
            span(150, 170), // reaped after the scan ended
            span(90, 170),  // spans the whole scan
            span(10, 20),   // before
            span(200, 210), // after
        ];
        assert_eq!(children_within(scan, &children), 2);
        assert_eq!(children_within(scan, &children[2..]), 0);
        assert_eq!(children_within(scan, &[]), 0);
    }

    #[test]
    #[should_panic(expected = "span ends before it starts")]
    fn a_backwards_span_is_a_programming_error() {
        span(20, 10);
    }
}
