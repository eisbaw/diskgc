//! `watch` (docs/reference.md, `watch`; TASK-32): the long-running form of
//! `ensure`. It samples one filesystem every `--interval` and, whenever
//! available space is below `--min-free` and no back-off holds, runs one
//! reclaim cycle up to `--target-free`, on a live index that a background
//! tally keeps fresh. `--tally-only` runs only the tally and the snapshot
//! writes.
//!
//! Composed bottom-up:
//!
//! - [`backoff`]: when the next cycle may start after one that ended below
//!   `--min-free` (pure);
//! - [`live`]: the live index shared by the background tally and the
//!   cycles, on a thread of its own on a real host ([`live::Threaded`])
//!   or inline on the model ([`live::Inline`]);
//! - [`Writer`]: the single-writer lock, held for the process's life, and
//!   the snapshot writes every [`SNAPSHOT_PERIOD`] and at stop;
//! - [`start`]: the start sequence (docs/reference.md, Watch start), whose
//!   first failure exits with its code and deletes nothing;
//! - [`run`]: the loop of samples, cycles and snapshot ticks until a
//!   signal.
//!
//! A cycle is `ensure`'s reclaim, not a copy of it: the caller passes it
//! in (`cycle`, [`crate::invocation::Prepared::execute`] on
//! [`crate::snapshot::RunIndex::live`]), so this module never holds a
//! [`crate::host::Delete`] and cannot delete. Its decisions, its checks
//! and its report are `ensure`'s; only the index (live, no synchronous
//! tally, no write) and the record (`exit_code` `null`, the `watch`
//! field) differ.
//!
//! Known gaps against the contract, each filed: a second signal during the
//! stop does not skip the snapshot write (TASK-257); the report's `index`
//! block stays `null` (TASK-244 defines it for `ensure` and `watch`
//! alike); a mount stacked on `PATH` is detected right before a cycle, not
//! inside the executor's own measurement, so a mount made between the two
//! is not seen by this check (TASK-259).

pub mod backoff;
pub mod live;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::executor::{self, timed_report};
use crate::host::{Delete, Dev, FsKey, Host, LockMode, Locked, MonoTime, WaitLock};
use crate::hostlock::{self, LockError};
use crate::invocation::{ConfigFile, EnsureOptions, Platform, Prepared, prepare};
use crate::report::json::Json;
use crate::report::{ErrorCause, ErrorEntry, Report, Signal, Status};
use crate::resolve::{ResolveError, Want, resolve_unscreened};
use crate::snapshot::{self, Guard, Held, Reclaimed, RunIndex};
use crate::tally::TallyState;
use crate::tally::listing::TallySource;
use crate::units::{Size, format_bytes};

use backoff::{Backoff, backs_off, classify};
use live::LiveIndex;

/// Where each cycle's JSON line goes (`--json`): a failure ends `watch`
/// at once ([`Ended::OutputFailed`]).
pub type Record<'a> = &'a mut dyn FnMut(&str) -> Result<(), String>;

/// One reclaim cycle on the live index, its progress lines to the log
/// ([`reclaim_cycle`]).
pub type Cycle<'a> = &'a mut dyn FnMut(&mut TallyState, &mut dyn FnMut(&str)) -> Report;

/// How often the snapshot is written (and, without the single-writer
/// lock, how often it is tried again).
pub const SNAPSHOT_PERIOD: Duration = Duration::from_secs(5 * 60);
/// How long the start waits for the single-writer lock: an `ensure` holds
/// it only while writing.
pub const START_LOCK_WAIT: Duration = Duration::from_secs(10);

/// What a `watch` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Reclaim below `--min-free` up to `--target-free`, sampling every
    /// `interval`; each cycle's deadline is `timeout` from its start.
    Reclaim {
        min_free: Size,
        target_free: Size,
        interval: Duration,
        timeout: Duration,
    },
    /// `--tally-only`: never sample for a goal, never take the host lock,
    /// never delete. `min_free` only bounds the snapshot write.
    TallyOnly { min_free: Option<Size> },
}

/// The goal of a reclaiming `watch`, in bytes (resolved at start).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Goal {
    pub min_free: u64,
    pub target_free: u64,
    pub interval: Duration,
}

/// The filesystem watched: `PATH` as reached at start, whose descriptor is
/// what every sample reads (a mount later stacked on `PATH` does not
/// change it), its device, the device mountinfo names for its mount (the
/// snapshot's space guard), and its size at start.
#[derive(Debug)]
pub struct Watched<F> {
    pub fd: F,
    pub path: PathBuf,
    pub mount: PathBuf,
    pub dev: Dev,
    pub fs: Dev,
    pub size_bytes: u64,
}

/// Why `watch` did not start: its exit code (0 for a signal during the
/// start's wait), and for 2 and 3 the report a `--json` start failure
/// prints.
#[derive(Debug)]
pub struct Refused {
    pub exit_code: u8,
    pub report: Option<Box<Report>>,
}

impl Refused {
    fn with(status: Status, cause: ErrorCause, path: &Path, why: String) -> Self {
        let mut report = Report::new(status, Some(path), false);
        report.errors.push(ErrorEntry::run(cause, why));
        let exit_code = match status {
            Status::UsageError => 2,
            _ => 3,
        };
        Self {
            exit_code,
            report: Some(Box::new(report)),
        }
    }

    fn usage(cause: ErrorCause, path: &Path, why: String) -> Self {
        Self::with(Status::UsageError, cause, path, why)
    }

    fn stopped() -> Self {
        Self {
            exit_code: 0,
            report: None,
        }
    }
}

/// A started `watch`: everything [`run`] needs.
#[derive(Debug)]
pub struct Started<F> {
    pub watched: Watched<F>,
    /// `None` under `--tally-only`.
    pub goal: Option<Goal>,
    /// `watch`'s decisions, with the cycle's goal and deadline.
    pub prepared: Prepared,
    pub writer: Writer<F>,
    /// The live index, seeded from a valid snapshot.
    pub state: TallyState,
    /// The background tally's sources.
    pub sources: Vec<TallySource>,
}

/// The start sequence (docs/reference.md, Watch start), in its order; the
/// first failure is the refusal. `options` carries `PATH`, `--select`,
/// `--roots` and `--allow-root` as for `ensure` (its goal fields are
/// unused); `state_dir` names the state directory for the log.
pub fn start<H: Delete>(
    host: &H,
    mode: &Mode,
    options: &EnsureOptions,
    config: &ConfigFile,
    platform: &Platform<'_>,
    state_dir: Option<&Path>,
    log: &mut dyn FnMut(&str),
) -> Result<Started<H::Fd>, Refused> {
    let path = options.path.as_path();
    let refuse = |(cause, why): (ErrorCause, String)| Refused::usage(cause, path, why);
    // 1, and privileges: the configuration, the selection, root.
    let prepared = prepare(host, options, config, platform, log).map_err(refuse)?;
    // 2. The pre-release sandbox gate, unless nothing can be deleted.
    if let Mode::Reclaim { .. } = mode
        && let Err(gate) = host.may_delete()
    {
        return Err(Refused::usage(
            ErrorCause::Usage,
            path,
            format!(
                "a reclaiming watch cannot run here: {gate} (watch --tally-only deletes \
                 nothing; ensure --dry-run previews a cycle)"
            ),
        ));
    }
    // 3. PATH, reached safely and held.
    let at = resolve_unscreened(host, path, Want::Directory).map_err(|e| {
        let why = format!("PATH {}: {e}", path.display());
        match e {
            ResolveError::Host(_) => Refused::with(Status::Error, ErrorCause::Host, path, why),
            _ => Refused::usage(ErrorCause::Usage, path, why),
        }
    })?;
    let first = host.statvfs(&at.fd);
    let size_bytes = first.as_ref().map_or(0, |s| s.size_bytes);
    let bytes = |size: Size, flag: &str| -> Result<u64, Refused> {
        match (size, &first) {
            (Size::Bytes(b), _) => Ok(b),
            (_, Ok(space)) => size.bytes(space.size_bytes).map_err(|e| {
                Refused::usage(ErrorCause::Usage, path, format!("{flag} {size}: {e}"))
            }),
            (_, Err(e)) => Err(Refused::with(
                Status::Error,
                ErrorCause::Host,
                path,
                format!(
                    "{flag} {size}: a percentage needs the filesystem's size, and sampling \
                     PATH {} failed: {e}",
                    path.display()
                ),
            )),
        }
    };
    let goal = match mode {
        Mode::Reclaim {
            min_free,
            target_free,
            interval,
            ..
        } => {
            let min_free = bytes(*min_free, "--min-free")?;
            let target_free = bytes(*target_free, "--target-free")?;
            if target_free < min_free {
                return Err(Refused::usage(
                    ErrorCause::Usage,
                    path,
                    format!(
                        "--target-free ({target_free} bytes) is below --min-free ({min_free} \
                         bytes)"
                    ),
                ));
            }
            Some(Goal {
                min_free,
                target_free,
                interval: *interval,
            })
        }
        Mode::TallyOnly { .. } => None,
    };
    let guard_min = match mode {
        Mode::Reclaim { .. } => goal.map(|g| g.min_free),
        Mode::TallyOnly { min_free } => min_free.map(|m| bytes(m, "--min-free")).transpose()?,
    };
    // 4. The host lock file.
    match (hostlock::readiness(host), mode) {
        (Ok(()), _) => {}
        (
            Err(e @ (LockError::Missing { .. } | LockError::Unusable { .. })),
            Mode::Reclaim { .. },
        ) => {
            return Err(Refused::usage(
                ErrorCause::Provisioning,
                path,
                format!("a watch that can never reclaim does not start: {e}"),
            ));
        }
        (Err(e), Mode::TallyOnly { .. }) => log(&format!(
            "warning: {e} (--tally-only only polls it: the tally pauses while it is held)"
        )),
        (Err(e), Mode::Reclaim { .. }) => log(&format!(
            "warning: {e}; each cycle tries it again (a cycle that cannot take it is an error \
             and backs off)"
        )),
    }
    // 5. Supplied roots.
    prepared
        .refuse_missing_roots(host, options)
        .map_err(refuse)?;
    let (tiers, timeouts) = prepared.tiers();
    if let Mode::Reclaim { timeout, .. } = mode {
        for (tier, own) in tiers.iter().zip(&timeouts) {
            if *own > *timeout {
                log(&format!(
                    "warning: tier `{}`: its timeout {} s is longer than --timeout {} s; the \
                     cycle's deadline cuts it",
                    tier.name,
                    own.as_secs(),
                    timeout.as_secs()
                ));
            }
        }
    }
    let sources = executor::tally_sources(tiers.iter().copied());
    // 6. The single-writer lock.
    let (key, fallback) = snapshot::fs_key(host, at.stat.dev);
    if let Some(why) = fallback {
        log(&format!(
            "index: filesystem {}: {why}; its snapshot is keyed by device number",
            at.stat.dev
        ));
    }
    let guard = Guard {
        fs: at.mount.dev,
        min_free: guard_min,
    };
    let writer = Writer::start(host, key, guard, state_dir, log).map_err(|e| match e {
        StartLock::Held(why) => Refused::usage(ErrorCause::Usage, path, why),
        StartLock::Interrupted => Refused::stopped(),
    })?;
    // 7. The seed.
    let mut state = TallyState::new(crate::index::Index::new(crate::index::BUDGET_BYTES));
    if writer.has_lock() {
        let bound = sources
            .iter()
            .map(|s| (&*s.finder.profile, s.finder.profile_hash))
            .collect();
        let mut seed = RunIndex::watch_seed();
        let run = Reclaimed {
            dev: at.stat.dev,
            fs: at.mount.dev,
            min_free: guard_min.unwrap_or(0),
            deadline: None,
        };
        seed.seed(host, run, &bound, log);
        state = seed.state;
    }
    let prepared = match (mode, goal) {
        (Mode::Reclaim { timeout, .. }, Some(goal)) => {
            prepared.with_goal(goal.min_free, goal.target_free, *timeout)
        }
        _ => prepared,
    };
    let stats = state.index.stats();
    let config = prepared
        .config_path()
        .map_or_else(|| "none (--select)".to_owned(), |p| p.display().to_string());
    let state_dir = state_dir.map_or_else(|| "none".to_owned(), |d| d.display().to_string());
    log(&match goal {
        Some(g) => format!(
            "watch {}: --min-free {}, --target-free {}, every {} s; config {config}; state \
             directory {state_dir}; index starts with {} unit(s)",
            at.mount.mount_point.display(),
            format_bytes(g.min_free),
            format_bytes(g.target_free),
            g.interval.as_secs(),
            stats.singles + stats.blocks,
        ),
        None => format!(
            "watch --tally-only {}: {} source(s); config {config}; state directory \
             {state_dir}; index starts with {} unit(s)",
            at.mount.mount_point.display(),
            sources.len(),
            stats.singles + stats.blocks,
        ),
    });
    Ok(Started {
        watched: Watched {
            path: path.to_path_buf(),
            mount: at.mount.mount_point.clone(),
            dev: at.stat.dev,
            fs: at.mount.dev,
            size_bytes,
            fd: at.fd,
        },
        goal,
        prepared,
        writer,
        state,
        sources,
    })
}

/// Why the single-writer lock was not taken at start.
#[derive(Debug)]
pub enum StartLock {
    /// Still held after [`START_LOCK_WAIT`]: another `watch` (exit 2).
    Held(String),
    /// A signal during the wait (exit 0, nothing written).
    Interrupted,
}

/// The snapshot's single-writer lock, held for the life of the process,
/// and the writes under it (docs/architecture.md, Snapshot). Without the
/// lock (the state directory or the lock file could not be made), nothing
/// is loaded or written, and every tick tries again; a lock then found
/// held is one log line.
#[derive(Debug)]
pub struct Writer<F> {
    key: FsKey,
    held: Option<(Locked<F>, Dev)>,
    guard: Guard,
    /// Whether a tick found the lock held by someone else (said once).
    said_held: bool,
}

impl<F> Writer<F> {
    pub fn has_lock(&self) -> bool {
        self.held.is_some()
    }
}

impl<F: std::fmt::Debug> Writer<F> {
    /// Takes the lock, waiting at most [`START_LOCK_WAIT`].
    pub fn start<H: Host<Fd = F>>(
        host: &H,
        key: FsKey,
        guard: Guard,
        state_dir: Option<&Path>,
        log: &mut dyn FnMut(&str),
    ) -> Result<Self, StartLock> {
        let mut writer = Self {
            key,
            held: None,
            guard,
            said_held: false,
        };
        let lock = match host.state_lock(&writer.key) {
            Ok(lock) => lock,
            Err(e) => {
                log(&format!(
                    "index: no state directory or lock file ({e}): no snapshot is loaded or \
                     written; trying again every {} min",
                    SNAPSHOT_PERIOD.as_secs() / 60
                ));
                return Ok(writer);
            }
        };
        let until = host.now().checked_add(START_LOCK_WAIT);
        match host.flock_wait(lock.fd, LockMode::Exclusive, until) {
            Ok(WaitLock::Acquired(locked)) => writer.held = Some((locked, lock.dir_dev)),
            Ok(WaitLock::DeadlinePassed(_)) => {
                return Err(StartLock::Held(format!(
                    "another watch holds {} for this filesystem and user (state directory {}): \
                     one watch per filesystem and state directory",
                    writer.key.lock_name(),
                    state_dir.map_or_else(|| "?".to_owned(), |d| d.display().to_string())
                )));
            }
            Ok(WaitLock::Interrupted(_)) => return Err(StartLock::Interrupted),
            Err(e) => log(&format!(
                "index: the single-writer lock {}: {e}; no snapshot is loaded or written; \
                 trying again every {} min",
                writer.key.lock_name(),
                SNAPSHOT_PERIOD.as_secs() / 60
            )),
        }
        Ok(writer)
    }

    /// One snapshot write of `state` (a tick, or the stop). Without the
    /// lock, tries to take it first, without waiting.
    pub fn write<H: Host<Fd = F>>(
        &mut self,
        host: &H,
        state: &TallyState,
        log: &mut dyn FnMut(&str),
    ) {
        if self.held.is_none() && !self.try_lock(host, log) {
            return;
        }
        let Some((locked, dir_fs)) = &self.held else {
            unreachable!("locked above")
        };
        let bytes = match snapshot::encode(host, state, &self.key) {
            Ok(bytes) => bytes,
            Err(why) => {
                return log(&format!(
                    "index: snapshot {} not written: {why}",
                    self.key.snapshot_name()
                ));
            }
        };
        let held = Held {
            key: &self.key,
            locked,
            dir_fs: *dir_fs,
        };
        snapshot::write_held(host, state, held, self.guard, &bytes, &|_| None, log);
    }

    fn try_lock<H: Host<Fd = F>>(&mut self, host: &H, log: &mut dyn FnMut(&str)) -> bool {
        let lock = match host.state_lock(&self.key) {
            Ok(lock) => lock,
            Err(_) => return false, // said at start
        };
        match host.flock_try(lock.fd, LockMode::Exclusive) {
            Ok(crate::host::TryLock::Acquired(locked)) => {
                log(&format!(
                    "index: single-writer lock {} taken; snapshots are written from now on",
                    self.key.lock_name()
                ));
                self.held = Some((locked, lock.dir_dev));
                true
            }
            Ok(crate::host::TryLock::Busy(_)) => {
                if !std::mem::replace(&mut self.said_held, true) {
                    log(&format!(
                        "index: {} is held by another writer; this watch writes no snapshot \
                         while it is",
                        self.key.lock_name()
                    ));
                }
                false
            }
            Err(e) => {
                log(&format!(
                    "index: the single-writer lock {}: {e}",
                    self.key.lock_name()
                ));
                false
            }
        }
    }
}

/// How [`run`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// A signal stopped it (exit 0).
    Stopped(Signal),
    /// Writing a record to stdout failed (exit 3, at once).
    OutputFailed(String),
}

/// Runs a cycle on the live index: `ensure`'s reclaim
/// ([`Prepared::execute`] on [`RunIndex::live`]), its progress lines to
/// `progress`.
pub fn reclaim_cycle<H: Delete>(
    host: &H,
    prepared: &Prepared,
    state: &mut TallyState,
    progress: &mut dyn FnMut(&str),
) -> Report {
    let budget = state.index.budget_bytes();
    let taken = std::mem::replace(state, TallyState::new(crate::index::Index::new(budget)));
    let mut index = RunIndex::live(taken);
    let report = prepared.execute(host, false, &mut index, progress);
    *state = index.state;
    report
}

/// The watcher's loop (docs/reference.md, The reclaim cycle): samples,
/// cycles and snapshot ticks until a signal; then the stop's snapshot.
///
/// - `cycle` runs one reclaim on the live index ([`reclaim_cycle`]);
/// - `record` takes each cycle's JSON line (`None`: no `--json`); its
///   failure ends the run at once ([`Ended::OutputFailed`]).
#[allow(clippy::too_many_arguments)]
pub fn run<H: Host, L: LiveIndex>(
    host: &H,
    watched: &Watched<H::Fd>,
    goal: Option<Goal>,
    live: &mut L,
    writer: &mut Writer<H::Fd>,
    cycle: Cycle<'_>,
    mut record: Option<Record<'_>>,
    log: &mut dyn FnMut(&str),
) -> Ended {
    let started = host.now();
    let at = |d: Duration| started.checked_add(d).expect("monotonic time overflow");
    let mut next_snapshot = at(SNAPSHOT_PERIOD);
    let mut next_sample = started; // the first sample at once
    let mut backoff: Option<Backoff> = None;
    let mut cycles: u64 = 0;
    let signal = loop {
        if let Some(signal) = host.interrupted() {
            break signal;
        }
        let until = match goal {
            Some(_) => next_snapshot.min(next_sample),
            None => next_snapshot,
        };
        live.wait_until(until);
        if let Some(signal) = host.interrupted() {
            break signal;
        }
        let now = host.now();
        if now >= next_snapshot {
            live.with_state(|state| writer.write(host, state, log));
            next_snapshot = after(next_snapshot, SNAPSHOT_PERIOD, host.now());
        }
        let Some(goal) = goal else { continue };
        if now < next_sample {
            continue;
        }
        let sample = host.statvfs(&watched.fd).map(|s| s.avail_bytes);
        match sample {
            Ok(avail) if avail >= goal.min_free => {
                if backoff.take().is_some() {
                    log(&format!(
                        "space recovered: {} available, at or above --min-free {}; back-off \
                         ended",
                        format_bytes(avail),
                        format_bytes(goal.min_free)
                    ));
                }
            }
            sample => {
                let avail = sample.as_ref().ok().copied();
                if backoff.as_ref().is_none_or(|b| b.allows(host.now(), avail)) {
                    cycles += 1;
                    let report = match sample {
                        Ok(avail) => {
                            log(&format!(
                                "cycle {cycles}: {} available, below --min-free {}; goal \
                                 --target-free {}",
                                format_bytes(avail),
                                format_bytes(goal.min_free),
                                format_bytes(goal.target_free)
                            ));
                            match same_filesystem(host, watched) {
                                Ok(()) => live.with_state(|state| cycle(state, log)),
                                Err(why) => failed_cycle(host, watched, goal, why, log),
                            }
                        }
                        Err(e) => {
                            log(&format!("cycle {cycles}: sampling the filesystem failed"));
                            let why = format!("PATH {}: {e}", watched.path.display());
                            failed_cycle(host, watched, goal, why, log)
                        }
                    };
                    log(&end_line(cycles, &report));
                    backoff = if backs_off(report.status) {
                        let (class, causes) = classify(&report);
                        let next = Backoff::after(
                            class,
                            causes,
                            host.now(),
                            report.avail_after_bytes,
                            watched.size_bytes,
                            backoff.as_ref(),
                        );
                        log(&next.log_line());
                        Some(next)
                    } else {
                        None
                    };
                    if let Some(record) = record.as_deref_mut() {
                        let watch = Json::Obj(vec![
                            ("cycle", Json::U64(cycles)),
                            ("backoff", Json::opt(backoff.as_ref(), Backoff::to_json)),
                        ]);
                        if let Err(why) = record(&report.to_watch_json(watch)) {
                            return Ended::OutputFailed(why);
                        }
                    }
                }
            }
        }
        next_sample = after(next_sample, goal.interval, host.now());
    };
    log(&match signal {
        Signal::Hup => "stopping on SIGHUP: diskgc does not reload; the configuration is read \
                        at start, so a restart applies an edited one"
            .to_owned(),
        other => format!("stopping on {}", other.name()),
    });
    live.with_state(|state| writer.write(host, state, log));
    Ended::Stopped(signal)
}

/// Runs a started `watch` with its background tally on a thread of its
/// own (a real host, [`live::Threaded`]); `log` takes both threads'
/// lines. The reclaim runs on the caller's thread at its priority.
pub fn run_threaded<H: Delete + Sync>(
    host: &H,
    started: Started<H::Fd>,
    record: Option<Record<'_>>,
    log: &(dyn Fn(&str) + Sync),
) -> Ended {
    let Started {
        watched,
        goal,
        prepared,
        mut writer,
        state,
        sources,
    } = started;
    let shared = live::Shared::new(state);
    live::with_tally_thread(host, &shared, &sources, log, |ended| {
        let mut live = live::Threaded::new(host, &shared, ended);
        let mut log_mut = |line: &str| log(line);
        let mut cycle = |state: &mut TallyState, progress: &mut dyn FnMut(&str)| {
            reclaim_cycle(host, &prepared, state, progress)
        };
        run(
            host,
            &watched,
            goal,
            &mut live,
            &mut writer,
            &mut cycle,
            record,
            &mut log_mut,
        )
    })
}

/// Runs a started `watch` with its background tally inline, on the
/// caller's thread (the model, [`live::Inline`]). Returns how it ended
/// and the live index.
pub fn run_inline<H: Delete>(
    host: &H,
    started: Started<H::Fd>,
    record: Option<Record<'_>>,
    log: &mut dyn FnMut(&str),
) -> (Ended, TallyState) {
    let Started {
        watched,
        goal,
        prepared,
        mut writer,
        state,
        sources,
    } = started;
    // One log for both, in the order the lines come: never borrowed by
    // both at once (the tally logs only between the loop's own lines).
    let log = std::cell::RefCell::new(log);
    let mut tally_log = |line: &str| (log.borrow_mut())(line);
    let mut loop_log = |line: &str| (log.borrow_mut())(line);
    let mut live = live::Inline::new(host, state, sources, &mut tally_log);
    let mut cycle = |state: &mut TallyState, progress: &mut dyn FnMut(&str)| {
        reclaim_cycle(host, &prepared, state, progress)
    };
    let end = run(
        host,
        &watched,
        goal,
        &mut live,
        &mut writer,
        &mut cycle,
        record,
        &mut loop_log,
    );
    (end, live.into_state())
}

/// The first tick of `period` from `tick` that is after `now`.
fn after(mut tick: MonoTime, period: Duration, now: MonoTime) -> MonoTime {
    while tick <= now {
        tick = tick.checked_add(period).expect("monotonic time overflow");
    }
    tick
}

/// Whether `PATH` still resolves to the filesystem watched since start
/// (docs/reference.md, The reclaim cycle): another device is a cycle
/// error (`host`), and nothing is deleted.
fn same_filesystem<H: Host>(host: &H, watched: &Watched<H::Fd>) -> Result<(), String> {
    let at = resolve_unscreened(host, &watched.path, Want::Directory)
        .map_err(|e| format!("PATH {}: {e}", watched.path.display()))?;
    if at.stat.dev != watched.dev {
        return Err(format!(
            "PATH {} is now on device {}, not on the filesystem watched since start (device \
             {}): nothing is reclaimed there (a mount stacked on PATH?)",
            watched.path.display(),
            at.stat.dev,
            watched.dev
        ));
    }
    Ok(())
}

/// A cycle that ended before the reclaim: status `error`, cause `host`
/// (so back-off `retry`), nothing deleted.
fn failed_cycle<H: Host>(
    host: &H,
    watched: &Watched<H::Fd>,
    goal: Goal,
    why: String,
    log: &mut dyn FnMut(&str),
) -> Report {
    log(&format!("error: {why}"));
    timed_report(host, || {
        let mut report = Report::new(Status::Error, Some(&watched.path), false).with_goal(
            Size::Bytes(goal.min_free),
            Some(Size::Bytes(goal.target_free)),
        );
        report.mount = Some(watched.mount.clone());
        report.errors.push(ErrorEntry::run(ErrorCause::Host, why));
        report
    })
}

/// The one line that ends a cycle: status, space before and after, what
/// was reclaimed and what was kept, counted by reason.
fn end_line(cycle: u64, report: &Report) -> String {
    let space = |v: Option<u64>| v.map_or_else(|| "unknown".to_owned(), format_bytes);
    let reclaimed: u64 = report.actions.iter().map(|a| a.count).sum();
    let mut kept: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    for skip in &report.skipped {
        *kept.entry(skip.reason.as_str()).or_default() += skip.count;
    }
    let kept_total: u64 = kept.values().sum();
    let by_reason = if kept.is_empty() {
        String::new()
    } else {
        format!(
            " ({})",
            kept.iter()
                .map(|(reason, n)| format!("{reason} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "cycle {cycle} ended {}: {} available before, {} after; {reclaimed} reclaimed, \
         {kept_total} kept{by_reason}",
        report.status.as_str(),
        space(report.avail_before_bytes),
        space(report.avail_after_bytes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> MonoTime {
        MonoTime(Duration::from_secs(secs))
    }

    #[test]
    fn the_next_tick_is_the_first_after_now() {
        let ten = Duration::from_secs(10);
        assert_eq!(after(at(0), ten, at(0)), at(10));
        assert_eq!(after(at(0), ten, at(9)), at(10));
        assert_eq!(after(at(0), ten, at(10)), at(20));
        // A cycle that took 95 s: the next sample is the tick after it.
        assert_eq!(after(at(10), ten, at(105)), at(110));
    }
}
