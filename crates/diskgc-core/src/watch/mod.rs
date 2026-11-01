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
//! - [`crate::snapshot::writer`]: the single-writer lock, held for the
//!   process's life, and the snapshot writes every [`SNAPSHOT_PERIOD`] and
//!   at the stop;
//! - [`start`]: the start sequence (docs/reference.md, Watch start), whose
//!   first failure exits with its code and deletes nothing;
//! - [`cycle_once`]: one cycle from its triggering sample, and [`run`]: the
//!   loop of samples, cycles and snapshot ticks until a signal.
//!
//! A cycle is `ensure`'s reclaim, not a copy of it: [`reclaim_cycle`] runs
//! [`Prepared::execute`] on [`RunIndex::live`] with a request that carries
//! the cycle ([`WatchCycle`]: the device `PATH` must still be on, and the
//! triggering sample the deadline counts from). Only [`run_threaded`] and
//! [`run_inline`] build it, and only for a reclaiming `watch`; the loop
//! itself holds no [`Delete`] and cannot delete.
//!
//! Known gaps against the contract: a second signal during the stop does
//! not skip the snapshot write (TASK-257); the report's `index` block stays
//! `null` (TASK-244 defines it for `ensure` and `watch` alike).

pub mod backoff;
pub mod live;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::executor::{
    self, MeasureFailure, WatchCycle, goal_bytes, reach_path, timed_report_from,
};
use crate::host::{Delete, Dev, Host, MonoTime, Timestamp};
use crate::hostlock::{self, LockError, Readiness, ReclaimLock};
use crate::invocation::{Loaded, Platform, Prepared, Selection, prepare};
use crate::report::json::Json;
use crate::report::{ErrorCause, ErrorEntry, Report, Signal, Status};
use crate::snapshot::writer::{StartLock, Writer};
use crate::snapshot::{self, Guard, Reclaimed, RunIndex};
use crate::tally::TallyState;
use crate::tally::listing::TallySource;
use crate::units::{Size, format_bytes};

use backoff::{Backoff, backs_off, classify};
use live::LiveIndex;

/// Where each cycle's JSON line goes (`--json`): a failure ends `watch`
/// at once ([`Ended::OutputFailed`]).
pub type Record<'a> = &'a mut dyn FnMut(&str) -> Result<(), String>;

/// One reclaim cycle ([`reclaim_cycle`]).
pub type Cycle<'a> = &'a mut dyn FnMut(CycleRun<'_>) -> Report;

/// How often the snapshot is written (and, without the single-writer
/// lock, how often it is tried again).
pub const SNAPSHOT_PERIOD: Duration = Duration::from_secs(5 * 60);

/// What a `watch` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Reclaim below `--min-free` up to `--target-free`, sampling every
    /// `interval`; each cycle's deadline is `timeout` from its triggering
    /// sample.
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

/// The goal of a reclaiming `watch`. A percentage is resolved against the
/// filesystem's size at the first sample that succeeds (at start, or
/// later when that one failed), and that size is rule 1's 1 % of the
/// back-off ([`backoff::step`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Goal {
    pub min_free: Size,
    pub target_free: Size,
    pub interval: Duration,
    pub timeout: Duration,
    resolved: Option<GoalBytes>,
}

/// [`Goal`] in bytes, and the filesystem's size it was resolved with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalBytes {
    pub min_free: u64,
    pub target_free: u64,
    pub size_bytes: u64,
}

impl Goal {
    /// Resolves the goal against a filesystem of `size_bytes`, once; a
    /// target below the minimum is a usage error.
    fn resolve(&mut self, size_bytes: u64) -> Result<GoalBytes, String> {
        if let Some(bytes) = self.resolved {
            return Ok(bytes);
        }
        let (min_free, target_free) = goal_bytes(self.min_free, Some(self.target_free), size_bytes)
            .map_err(|e| match e {
                MeasureFailure::Usage(why) | MeasureFailure::Host(why) => why,
            })?;
        let bytes = GoalBytes {
            min_free,
            target_free,
            size_bytes,
        };
        self.resolved = Some(bytes);
        Ok(bytes)
    }

    /// The goal as the report shows it before anything is measured.
    fn report(&self, path: &Path) -> Report {
        Report::new(Status::Error, Some(path), false)
            .with_goal(self.min_free, Some(self.target_free))
    }
}

/// The filesystem watched: `PATH` as reached at start, whose descriptor is
/// what every sample reads (a mount later stacked on `PATH` does not
/// change it), its device (every cycle's resolution of `PATH` must find
/// it), and the device mountinfo names for its mount (the snapshot's space
/// guard).
#[derive(Debug)]
pub struct Watched<F> {
    pub fd: F,
    pub path: PathBuf,
    pub mount: PathBuf,
    pub dev: Dev,
    pub fs: Dev,
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

    fn measure(path: &Path, failure: MeasureFailure) -> Self {
        match failure {
            MeasureFailure::Usage(why) => Self::usage(ErrorCause::Usage, path, why),
            MeasureFailure::Host(why) => Self::with(Status::Error, ErrorCause::Host, path, why),
        }
    }

    fn stopped() -> Self {
        Self {
            exit_code: 0,
            report: None,
        }
    }
}

/// What a reclaiming `watch` reclaims with: its goal and `watch`'s
/// decisions (made once at start; each cycle refreshes what must be
/// checked again, [`Prepared::for_cycle`]).
#[derive(Debug)]
pub struct Reclaim {
    pub goal: Goal,
    pub prepared: Prepared,
}

/// A started `watch`: everything [`run`] needs.
#[derive(Debug)]
pub struct Started<F> {
    pub watched: Watched<F>,
    /// `None` under `--tally-only`: nothing in it can delete.
    pub reclaim: Option<Reclaim>,
    pub writer: Writer<F>,
    /// The live index, seeded from a valid snapshot.
    pub state: TallyState,
    /// The background tally's sources.
    pub sources: Vec<TallySource>,
}

/// The start sequence (docs/reference.md, Watch start), in its order, from
/// step 1's [`Loaded`] ([`crate::invocation::load`], which the binary runs
/// before its host startup); the first failure is the refusal. `state_dir`
/// names the state directory for the log.
pub fn start<H: Delete>(
    host: &H,
    mode: &Mode,
    selection: &Selection,
    loaded: &Loaded,
    platform: &Platform<'_>,
    state_dir: Option<&Path>,
    log: &mut dyn FnMut(&str),
) -> Result<Started<H::Fd>, Refused> {
    let path = selection.path.as_path();
    let refuse = |(cause, why): (ErrorCause, String)| {
        Refused::with(crate::invocation::refusal_status(cause), cause, path, why)
    };
    // 1 (the configuration and --select) is `load`'s and 2 (kernel,
    // /proc) the host's startup, both before this. 3: privileges, and what
    // depends on the user.
    let prepared = prepare(host, selection, loaded, platform).map_err(refuse)?;
    let mut goal = match mode {
        Mode::Reclaim {
            min_free,
            target_free,
            interval,
            timeout,
        } => Some(Goal {
            min_free: *min_free,
            target_free: *target_free,
            interval: *interval,
            timeout: *timeout,
            resolved: None,
        }),
        Mode::TallyOnly { .. } => None,
    };
    // 3. The pre-release sandbox gate, unless nothing can be deleted.
    if goal.is_some()
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
    // 4. PATH, reached safely and held. The first sample is not a start
    //    check: a goal whose percentage needs it waits for one that works.
    let at = reach_path(host, path).map_err(|e| Refused::measure(path, e))?;
    let first = host.statvfs(&at.fd);
    if let Some(goal) = &mut goal {
        match &first {
            Ok(space) => {
                goal.resolve(space.size_bytes)
                    .map_err(|why| Refused::usage(ErrorCause::Usage, path, why))?;
            }
            Err(e) => {
                if let (Size::Bytes(min), Size::Bytes(target)) = (goal.min_free, goal.target_free)
                    && target < min
                {
                    return Err(Refused::usage(
                        ErrorCause::Usage,
                        path,
                        format!("--target-free ({target} bytes) is below --min-free ({min} bytes)"),
                    ));
                }
                log(&format!(
                    "warning: the first sample of PATH {} failed ({e}); it is the first cycle, \
                     and the goal is resolved at the first sample that works",
                    path.display()
                ));
            }
        }
    }
    let guard_min = match (mode, &goal) {
        (_, Some(goal)) => goal.resolved.map(|g| g.min_free),
        (Mode::TallyOnly { min_free: None }, _) => None,
        (Mode::TallyOnly { min_free: Some(m) }, _) => match (&first, m) {
            (_, Size::Bytes(b)) => Some(*b),
            (Ok(space), m) => Some(m.bytes(space.size_bytes).map_err(|e| {
                Refused::usage(ErrorCause::Usage, path, format!("--min-free {m}: {e}"))
            })?),
            (Err(e), m) => {
                log(&format!(
                    "warning: --min-free {m} cannot be resolved (sampling PATH failed: {e}); \
                     snapshot writes skip only on a failed write"
                ));
                None
            }
        },
        (Mode::Reclaim { .. }, None) => unreachable!("a reclaiming watch has a goal"),
    };
    // 5. The lock reclaims take (docs/safety.md, Host lock): the host lock,
    // or for a non-root watch without it the per-user lock, made now in
    // reclaim mode; --tally-only makes nothing, it only names what it polls.
    let host_lock = hostlock::host_lock_path();
    if goal.is_some() {
        match hostlock::prepare(host) {
            Ok(ReclaimLock::Host) => {}
            Ok(lock @ ReclaimLock::User(_)) => log(&format!(
                "the host lock {} is not provisioned: reclaims take the {lock} instead, so \
                 reclaims by other users are not serialised with this watch's",
                host_lock.display()
            )),
            Ok(ReclaimLock::Unlocked(hostlock::Unlocked { path, why })) => log(&format!(
                "warning: the host lock {} is not provisioned and the per-user lock{} cannot be \
                 made ({why}): each cycle tries again, and runs without a lock while it cannot",
                host_lock.display(),
                path.as_ref()
                    .map_or_else(String::new, |p| format!(" {}", p.display()))
            )),
            Err(
                e @ (LockError::Missing { .. }
                | LockError::Unusable { .. }
                | LockError::UserLockRefused { .. }),
            ) => {
                return Err(Refused::usage(
                    ErrorCause::Provisioning,
                    path,
                    format!("a watch that can never reclaim does not start: {e}"),
                ));
            }
            Err(e) => log(&format!(
                "warning: {e}; each cycle tries it again (a cycle that cannot take it is an \
                 error and backs off)"
            )),
        }
    } else {
        match hostlock::readiness(host) {
            Ok(Readiness::Host) => {}
            Ok(Readiness::FallsBack(Some(lock))) => log(&format!(
                "the host lock {} is not provisioned: the tally pauses only while the \
                 per-user lock {} is held (this user's reclaims), not for other users' reclaims",
                host_lock.display(),
                lock.display()
            )),
            Ok(Readiness::FallsBack(None)) => log(&format!(
                "the host lock {} is not provisioned and there is no place for a per-user \
                 lock: the tally pauses for no reclaim",
                host_lock.display()
            )),
            Err(e) => log(&format!(
                "warning: {e} (--tally-only only polls it: the tally pauses while it is held)"
            )),
        }
    }
    // 6. Supplied roots.
    prepared
        .refuse_missing_roots(host, selection)
        .map_err(refuse)?;
    let (tiers, timeouts) = prepared.tiers();
    if let Some(goal) = &goal {
        for (tier, own) in tiers.iter().zip(&timeouts) {
            if *own > goal.timeout {
                log(&format!(
                    "warning: tier `{}`: its timeout {} s is longer than --timeout {} s; the \
                     cycle's deadline cuts it",
                    tier.name,
                    own.as_secs(),
                    goal.timeout.as_secs()
                ));
            }
        }
    }
    let sources = executor::tally_sources(tiers.iter().copied());
    // 7. The single-writer lock.
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
    let writer =
        Writer::start(host, key, guard, state_dir, SNAPSHOT_PERIOD, log).map_err(|e| match e {
            StartLock::Held(why) => Refused::usage(ErrorCause::Usage, path, why),
            StartLock::Interrupted => Refused::stopped(),
        })?;
    // 8. The seed.
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
    let stats = state.index.stats();
    let config = prepared
        .config_path()
        .map_or_else(|| "none (--select)".to_owned(), |p| p.display().to_string());
    let state_dir = state_dir.map_or_else(|| "none".to_owned(), |d| d.display().to_string());
    let units = stats.singles + stats.blocks;
    let mount = at.mount.mount_point.display();
    log(&match &goal {
        Some(g) => format!(
            "watch {mount}: --min-free {}, --target-free {}, every {} s; config {config}; state \
             directory {state_dir}; index starts with {units} unit(s)",
            shown(g.min_free, g.resolved.map(|r| r.min_free)),
            shown(g.target_free, g.resolved.map(|r| r.target_free)),
            g.interval.as_secs(),
        ),
        None => format!(
            "watch --tally-only {mount}: {} source(s); config {config}; state directory \
             {state_dir}; index starts with {units} unit(s)",
            sources.len(),
        ),
    });
    Ok(Started {
        watched: Watched {
            path: path.to_path_buf(),
            mount: at.mount.mount_point.clone(),
            dev: at.stat.dev,
            fs: at.mount.dev,
            fd: at.fd,
        },
        reclaim: goal.map(|goal| Reclaim { goal, prepared }),
        writer,
        state,
        sources,
    })
}

/// A size for the log: in bytes when known.
fn shown(size: Size, bytes: Option<u64>) -> String {
    bytes.map_or_else(|| size.to_string(), format_bytes)
}

/// How [`run`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// A signal stopped it (exit 0).
    Stopped(Signal),
    /// Writing a record to stdout failed (exit 3, at once).
    OutputFailed(String),
    /// A percentage goal, resolved at the first sample that worked (the
    /// start's failed), has its target below its minimum (exit 2, as at
    /// start).
    Refused(String),
}

/// What one cycle gets ([`cycle_once`] makes it, [`reclaim_cycle`] runs
/// it).
pub struct CycleRun<'a> {
    /// The live index; `None` when the background tally did not let go of
    /// it in time ([`live::LOCK_WAIT`]): the cycle runs on discovery alone.
    pub index: Option<&'a mut TallyState>,
    pub goal: GoalBytes,
    pub timeout: Duration,
    pub watch: WatchCycle,
    pub log: &'a mut dyn FnMut(&str),
}

/// Runs a cycle: `watch`'s decisions refreshed for it ([`Prepared::for_cycle`]:
/// owners resolved again, random groups shuffled again; a refusal is the
/// cycle's error, nothing deleted), then `ensure`'s reclaim
/// ([`Prepared::execute`]) on the live index ([`RunIndex::live`]; without
/// one, an empty index: discovery alone), with the cycle's request.
pub fn reclaim_cycle<H: Delete>(
    host: &H,
    prepared: &Prepared,
    draw_seed: &dyn Fn() -> Result<u64, String>,
    run: CycleRun<'_>,
) -> Report {
    let CycleRun {
        index,
        goal,
        timeout,
        watch,
        log,
    } = run;
    let prepared = match prepared.for_cycle(host, draw_seed, log) {
        Ok(prepared) => prepared,
        Err((cause, why)) => {
            log(&format!("error: {why}"));
            return timed_report_from(host, Some((watch.origin, watch.origin_wall)), || {
                let mut report = Report::new(Status::Error, None, false).with_goal(
                    Size::Bytes(goal.min_free),
                    Some(Size::Bytes(goal.target_free)),
                );
                report.min_free_bytes = Some(goal.min_free);
                report.target_free_bytes = Some(goal.target_free);
                report.errors.push(ErrorEntry::run(cause, why));
                report
            });
        }
    };
    let request = prepared.request(
        Size::Bytes(goal.min_free),
        Some(Size::Bytes(goal.target_free)),
        Some(timeout),
        None,
        Some(watch),
    );
    let fresh = || TallyState::new(crate::index::Index::new(crate::index::BUDGET_BYTES));
    match index {
        Some(state) => {
            let mut live = RunIndex::live(std::mem::replace(state, fresh()));
            let report = prepared.execute(host, &request, false, &mut live, log);
            *state = live.state;
            report
        }
        None => prepared.execute(host, &request, false, &mut RunIndex::live(fresh()), log),
    }
}

/// One cycle from its triggering sample at `watch.origin` (the deadline
/// and the record's times count from it): the live index taken within
/// [`live::LOCK_WAIT`] (and never past the deadline), else none, said in
/// one line; then `cycle`.
pub fn cycle_once<H: Host, L: LiveIndex>(
    host: &H,
    watch: WatchCycle,
    goal: GoalBytes,
    timeout: Duration,
    live: &mut L,
    cycle: Cycle<'_>,
    log: &mut dyn FnMut(&str),
) -> Report {
    let deadline = watch
        .origin
        .checked_add(timeout)
        .unwrap_or(MonoTime(Duration::MAX));
    let until = host
        .now()
        .checked_add(live::LOCK_WAIT)
        .map_or(deadline, |t| t.min(deadline));
    live.with_state(until, |index| {
        if index.is_none() {
            log(
                "index: the background tally did not let go of the live index within 1 s; \
                 this cycle runs without it (each tier's discovery alone)",
            );
        }
        cycle(CycleRun {
            index,
            goal,
            timeout,
            watch,
            log,
        })
    })
}

/// One snapshot write: encoded with the live index (taken within
/// [`live::LOCK_WAIT`]), written without it.
fn write_snapshot<H: Host, L: LiveIndex>(
    host: &H,
    live: &mut L,
    writer: &mut Writer<H::Fd>,
    log: &mut dyn FnMut(&str),
) {
    let until = host
        .now()
        .checked_add(live::LOCK_WAIT)
        .expect("monotonic time overflow");
    let encoded = live.with_state(until, |state| match state {
        Some(state) => writer.encode(host, state, log),
        None => {
            log(&format!(
                "index: snapshot {} not written: the background tally did not let go of the \
                 live index within 1 s",
                writer.key().snapshot_name()
            ));
            None
        }
    });
    if let Some(encoded) = encoded {
        writer.write(host, &encoded, log);
    }
}

/// The watcher's loop (docs/reference.md, The reclaim cycle): samples,
/// cycles and snapshot ticks until a signal; then the stop's snapshot.
///
/// - `reclaim`: the goal and the cycle ([`reclaim_cycle`]); `None` under
///   `--tally-only`, which only writes snapshots;
/// - `record` takes each cycle's JSON line (`None`: no `--json`); its
///   failure ends the run at once ([`Ended::OutputFailed`]).
pub fn run<H: Host, L: LiveIndex>(
    host: &H,
    watched: &Watched<H::Fd>,
    mut reclaim: Option<(&mut Goal, Cycle<'_>)>,
    live: &mut L,
    writer: &mut Writer<H::Fd>,
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
        let until = match reclaim {
            Some(_) => next_snapshot.min(next_sample),
            None => next_snapshot,
        };
        live.wait_until(until);
        if let Some(signal) = host.interrupted() {
            break signal;
        }
        let now = host.now();
        if now >= next_snapshot {
            write_snapshot(host, live, writer, log);
            next_snapshot = after(next_snapshot, SNAPSHOT_PERIOD, host.now());
        }
        let Some((goal, cycle)) = &mut reclaim else {
            continue;
        };
        if now < next_sample {
            continue;
        }
        let origin = (host.now(), host.wall_clock());
        let sample = host.statvfs(&watched.fd);
        let resolved = match &sample {
            Ok(space) => match goal.resolve(space.size_bytes) {
                Ok(bytes) => Some(bytes),
                Err(why) => return Ended::Refused(why),
            },
            Err(_) => goal.resolved,
        };
        let avail = sample.as_ref().ok().map(|s| s.avail_bytes);
        if let (Some(avail), Some(bytes)) = (avail, resolved)
            && avail >= bytes.min_free
        {
            if backoff.take().is_some() {
                log(&format!(
                    "space recovered: {} available, at or above --min-free {}; back-off ended",
                    format_bytes(avail),
                    format_bytes(bytes.min_free)
                ));
            }
        } else if backoff.as_ref().is_none_or(|b| b.allows(host.now(), avail)) {
            // Below --min-free, or a sample that failed (it counts as below).
            cycles += 1;
            let report = match (&sample, resolved) {
                (Ok(space), Some(bytes)) => {
                    log(&format!(
                        "cycle {cycles}: {} available, below --min-free {}; goal --target-free {}",
                        format_bytes(space.avail_bytes),
                        format_bytes(bytes.min_free),
                        format_bytes(bytes.target_free)
                    ));
                    let watch = WatchCycle {
                        expect_dev: watched.dev,
                        origin: origin.0,
                        origin_wall: origin.1,
                    };
                    cycle_once(host, watch, bytes, goal.timeout, live, &mut **cycle, log)
                }
                (sample, _) => {
                    let why = match sample {
                        Err(e) => format!("PATH {}: {e}", watched.path.display()),
                        Ok(_) => unreachable!("a sample that works resolves the goal"),
                    };
                    log(&format!("cycle {cycles}: sampling the filesystem failed"));
                    failed_cycle(host, watched, goal, origin, why, log)
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
                    goal.resolved.map_or(0, |g| g.size_bytes),
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
        next_sample = after(next_sample, goal.interval, host.now());
    };
    log(&match signal {
        Signal::Hup => "stopping on SIGHUP: diskgc does not reload; the configuration is read \
                        at start, so a restart applies an edited one"
            .to_owned(),
        other => format!("stopping on {}", other.name()),
    });
    write_snapshot(host, live, writer, log);
    Ended::Stopped(signal)
}

/// Runs a started `watch` with its background tally on a thread of its
/// own (a real host, [`live::Threaded`]); `log` takes both threads' lines.
/// The reclaim runs on the caller's thread at its priority. `'static`
/// because the tally thread is not joined if it is blocked in the kernel
/// at the end ([`live::TallyThread`]).
pub fn run_threaded<H: Delete + Sync + 'static>(
    host: &'static H,
    started: Started<H::Fd>,
    draw_seed: &dyn Fn() -> Result<u64, String>,
    record: Option<Record<'_>>,
    log: &'static (dyn Fn(&str) + Sync),
) -> Ended {
    let Started {
        watched,
        reclaim,
        mut writer,
        state,
        sources,
    } = started;
    let shared = Arc::new(live::Shared::new(state));
    live::with_tally_thread(host, Arc::clone(&shared), sources, log, |ended| {
        let mut live = live::Threaded::new(host, &shared, ended);
        let mut log_mut = |line: &str| log(line);
        match reclaim {
            Some(Reclaim { mut goal, prepared }) => {
                let mut cycle = |run: CycleRun<'_>| reclaim_cycle(host, &prepared, draw_seed, run);
                run(
                    host,
                    &watched,
                    Some((&mut goal, &mut cycle)),
                    &mut live,
                    &mut writer,
                    record,
                    &mut log_mut,
                )
            }
            None => run(
                host,
                &watched,
                None,
                &mut live,
                &mut writer,
                record,
                &mut log_mut,
            ),
        }
    })
}

/// Runs a started `watch` with its background tally inline, on the
/// caller's thread and at its priority (the model, [`live::Inline`]).
/// Returns how it ended and the live index.
pub fn run_inline<H: Delete>(
    host: &H,
    started: Started<H::Fd>,
    draw_seed: &dyn Fn() -> Result<u64, String>,
    record: Option<Record<'_>>,
    log: &mut dyn FnMut(&str),
) -> (Ended, TallyState) {
    let Started {
        watched,
        reclaim,
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
    let end = match reclaim {
        Some(Reclaim { mut goal, prepared }) => {
            let mut cycle = |run: CycleRun<'_>| reclaim_cycle(host, &prepared, draw_seed, run);
            run(
                host,
                &watched,
                Some((&mut goal, &mut cycle)),
                &mut live,
                &mut writer,
                record,
                &mut loop_log,
            )
        }
        None => run(
            host,
            &watched,
            None,
            &mut live,
            &mut writer,
            record,
            &mut loop_log,
        ),
    };
    (end, live.into_state())
}

/// The first tick of `period` from `tick` that is after `now`.
fn after(mut tick: MonoTime, period: Duration, now: MonoTime) -> MonoTime {
    while tick <= now {
        tick = tick.checked_add(period).expect("monotonic time overflow");
    }
    tick
}

/// A cycle that ended at its triggering sample, which failed: status
/// `error`, cause `host` (so back-off `retry`), nothing deleted. Its times
/// count from the sample (`origin`).
fn failed_cycle<H: Host>(
    host: &H,
    watched: &Watched<H::Fd>,
    goal: &Goal,
    origin: (MonoTime, Timestamp),
    why: String,
    log: &mut dyn FnMut(&str),
) -> Report {
    log(&format!("error: {why}"));
    timed_report_from(host, Some(origin), || {
        let mut report = goal.report(&watched.path);
        if let Some(bytes) = goal.resolved {
            report.min_free_bytes = Some(bytes.min_free);
            report.target_free_bytes = Some(bytes.target_free);
        }
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

    #[test]
    fn a_goal_resolves_once_and_refuses_a_target_below_its_minimum() {
        let mut goal = Goal {
            min_free: Size::Percent(10),
            target_free: Size::Percent(20),
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(60),
            resolved: None,
        };
        let first = goal.resolve(1000).unwrap();
        assert_eq!((first.min_free, first.target_free), (100, 200));
        // Resolved at the first sample that worked, never again.
        assert_eq!(goal.resolve(5000).unwrap(), first);
        let mut bad = Goal {
            min_free: Size::Bytes(500),
            target_free: Size::Percent(10),
            resolved: None,
            ..goal.clone()
        };
        assert!(bad.resolve(1000).unwrap_err().contains("below --min-free"));
    }
}
