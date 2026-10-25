//! The background tally (in `watch`; docs/architecture.md, The tally:
//! Background). It must not load the system:
//!
//! - at start it lowers its own thread's priority (`nice 19`, idle I/O
//!   class: [`Host::lower_priority`]) and probes the pressure stall
//!   information; without it, the duty cycle alone applies (one
//!   [`Event::NoPsi`]);
//! - before each batch it tests, without waiting, whether anyone holds the
//!   host lock ([`hostlock::held_by_another`]); if so a reclaim is running
//!   and it touches nothing, polling every [`LOCK_POLL`];
//! - then it reads the pressure; under pressure the batch is skipped and
//!   the sleep doubles ([`Pace`]);
//! - a batch ([`Budget::batch`]: at most 50 ms or 2,000 operations) does the
//!   most overdue jobs first ([`super::schedule`]): listing a source (every 5
//!   minutes), re-checking a single (every 10), sizing a single (every 60,
//!   resumed across batches by the live cursor). A listing is resumable
//!   too (Mark, 2026-10-25): a round of one source runs across batches
//!   within their bounds ([`super::listing::ListingCursor`]); one round is
//!   live at a time, finished before another starts;
//! - after a batch that took `t` it sleeps `19 × t`, or, when nothing was
//!   due, until the next job is due (at most [`IDLE_SLEEP_MAX`]).
//!
//! It never speeds up: no sleep is ever shortened because the system is
//! idle. Blocks are not re-checked or sized here (they have no check time
//! and no member names); collapse re-validates them and complete listings
//! recount them.

use std::time::Duration;

use super::budget::Budget;
use super::listing::TallySource;
use super::pace::{BATCH_OPS_MAX, Gate, Pace};
use super::schedule::{
    DISCOVERY_PERIOD, Lateness, MostOverdue, RECHECK_PERIOD, RETALLY_PERIOD, last_sized,
    needs_sizing,
};
use super::size::reach;
use super::{Rounded, Sizing, TallyState};
use crate::host::{Host, HostError, MonoTime, Pressure};
use crate::hostlock::{self, LockError};
use crate::index::{Identity, MountKey, UnitRef};

/// How often a paused tally tests the host lock again.
pub const LOCK_POLL: Duration = Duration::from_secs(1);
/// The longest sleep when nothing is due (so a new source or a changed
/// clock is noticed within it).
pub const IDLE_SLEEP_MAX: Duration = Duration::from_secs(60);
/// After a batch whose every job failed (a failure of the host), the sleep
/// is at least this, doubling with each such batch in a row, up to
/// [`super::pace::BRAKE_SLEEP_MAX`].
pub const FAILURE_SLEEP_MIN: Duration = Duration::from_secs(1);
/// Distinct failure messages logged per run; later ones are counted only.
pub const FAILURE_MESSAGES_MAX: usize = 64;

/// What the background tally reports, for the log (TASK-32 words it).
#[derive(Debug)]
pub enum Event {
    /// Started: whether the pressure brake can work.
    Started { psi: bool },
    /// The priority could not be lowered (it runs on all the same).
    PriorityNotLowered(HostError),
    /// No pressure stall information: the duty cycle alone applies. Once.
    NoPsi(HostError),
    /// A pressure read failed after the start: that batch ran unbraked.
    /// Once per run.
    PsiUnreadable(HostError),
    /// Another process holds the host lock: nothing done, polling.
    Paused,
    /// The host lock cannot be tested (missing: no reclaim can run either;
    /// otherwise the tally pauses). Once per kind of error.
    HostLockUnusable(LockError),
    /// Under pressure: the batch skipped.
    Braked { reading: Pressure, sleep: Duration },
    /// A batch ran.
    Batch(BatchWork),
    /// A job failed with a failure of the host (`EIO`, `EMFILE`): that job
    /// waits its period (a unit's: [`RECHECK_PERIOD`]), the batch goes on.
    /// Logged once per distinct message.
    Failed(HostError),
}

/// What one batch did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchWork {
    /// How long it took, by the clock.
    pub took: Duration,
    /// Filesystem operations, as counted (a listing counts one per
    /// candidate it found and two more, an estimate).
    pub ops: usize,
    /// Listing rounds ended.
    pub listed: usize,
    /// Listing rounds paused at the batch's end.
    pub listing: usize,
    pub rechecked: usize,
    pub evicted: usize,
    pub sized: usize,
    /// Sizing walks that paused at the batch's end.
    pub paused: usize,
    /// Jobs that failed (a failure of the host).
    pub failed: usize,
    /// The sleep after it.
    pub sleep: Duration,
}

impl BatchWork {
    fn jobs(&self) -> usize {
        self.listed
            + self.listing
            + self.rechecked
            + self.evicted
            + self.sized
            + self.paused
            + self.failed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Psi {
    Present,
    Absent,
}

/// The background tally's own state between batches.
#[derive(Debug)]
pub struct Background {
    pace: Pace,
    psi: Psi,
    /// When it started: sources never listed are due from then.
    started: MonoTime,
    psi_error_logged: bool,
    lock_error_logged: Option<std::mem::Discriminant<LockError>>,
    /// Batches in a row whose every job failed.
    failing: u32,
    /// Failure messages logged so far (at most [`FAILURE_MESSAGES_MAX`]).
    failures_logged: std::collections::BTreeSet<String>,
}

/// One job of a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// List the source with this index.
    List(usize),
    /// Re-check the single at this path (recorded with this identity).
    Recheck(MountKey, std::path::PathBuf, Identity),
    /// Size the single at this path.
    Size(MountKey, std::path::PathBuf),
}

impl Background {
    /// Starts: lowers the priority and probes the pressure.
    pub fn start<H: Host>(host: &H, log: &mut dyn FnMut(Event)) -> Self {
        if let Err(e) = host.lower_priority() {
            log(Event::PriorityNotLowered(e));
        }
        let psi = match host.pressure() {
            Ok(_) => Psi::Present,
            Err(e) => {
                log(Event::NoPsi(e));
                Psi::Absent
            }
        };
        log(Event::Started {
            psi: psi == Psi::Present,
        });
        Self {
            pace: Pace::default(),
            psi,
            started: host.now(),
            psi_error_logged: false,
            lock_error_logged: None,
            failing: 0,
            failures_logged: std::collections::BTreeSet::new(),
        }
    }

    /// One turn: pause, brake or batch. Returns how long to sleep before
    /// the next.
    pub fn turn<H: Host>(
        &mut self,
        host: &H,
        state: &mut TallyState,
        sources: &[TallySource],
        log: &mut dyn FnMut(Event),
    ) -> Duration {
        // The whole turn is the worker's active time: the lock test and the
        // pressure read count towards the duty cycle like the batch.
        let start = host.now();
        match hostlock::held_by_another(host) {
            Ok(false) => {}
            Ok(true) => {
                log(Event::Paused);
                return LOCK_POLL;
            }
            Err(e) => {
                let missing = matches!(e, LockError::Missing { .. });
                let kind = std::mem::discriminant(&e);
                if self.lock_error_logged != Some(kind) {
                    self.lock_error_logged = Some(kind);
                    log(Event::HostLockUnusable(e));
                }
                // Missing: not provisioned, so no reclaim can hold it.
                // Anything else: it cannot be told, so do nothing.
                if !missing {
                    return LOCK_POLL;
                }
            }
        }
        let reading = match self.psi {
            Psi::Absent => None,
            Psi::Present => match host.pressure() {
                Ok(reading) => Some(reading),
                Err(e) => {
                    if !self.psi_error_logged {
                        self.psi_error_logged = true;
                        log(Event::PsiUnreadable(e));
                    }
                    None
                }
            },
        };
        if let Gate::Skip(sleep) = self.pace.before_batch(reading.as_ref()) {
            log(Event::Braked {
                reading: reading.expect("only a reading brakes"),
                sleep,
            });
            return sleep;
        }
        let mut budget = Budget::batch(start);
        let mut work = BatchWork::default();
        let mut failures = Vec::new();
        let next_due = batch(
            host,
            state,
            sources,
            self.started,
            &mut budget,
            &mut work,
            &mut failures,
        );
        work.took = host.now().saturating_since(start);
        work.ops = budget.ops();
        let mut sleep = self.pace.after_batch(work.took);
        for failure in failures {
            let message = failure.to_string();
            if self.failures_logged.len() < FAILURE_MESSAGES_MAX
                && self.failures_logged.insert(message)
            {
                log(Event::Failed(failure));
            }
        }
        if work.failed > 0 && work.failed == work.jobs() {
            self.failing = self.failing.saturating_add(1);
            let backoff = FAILURE_SLEEP_MIN
                .saturating_mul(1 << (self.failing - 1).min(16))
                .min(super::pace::BRAKE_SLEEP_MAX);
            sleep = sleep.max(backoff);
        } else {
            self.failing = 0;
        }
        if work.jobs() == 0 {
            let until_due = next_due.map_or(IDLE_SLEEP_MAX, |due| {
                due.saturating_since(host.now()).min(IDLE_SLEEP_MAX)
            });
            sleep = sleep.max(until_due);
        }
        work.sleep = sleep;
        log(Event::Batch(work));
        sleep
    }
}

/// Runs the background tally until `stop` says so or a signal arrives
/// (checked between turns).
pub fn run<H: Host>(
    host: &H,
    state: &mut TallyState,
    sources: &[TallySource],
    log: &mut dyn FnMut(Event),
    stop: &mut dyn FnMut(&TallyState) -> bool,
) {
    let mut background = Background::start(host, log);
    while !stop(state) && host.interrupted().is_none() {
        let sleep = background.turn(host, state, sources, log);
        host.sleep(sleep);
    }
}

/// One batch: the most overdue jobs within `budget`. A job that fails
/// with a failure of the host is recorded in `failures` and waits its
/// period; the batch goes on. Returns when the next job not yet due falls
/// due (`None`: none known).
fn batch<H: Host>(
    host: &H,
    state: &mut TallyState,
    sources: &[TallySource],
    started: MonoTime,
    budget: &mut Budget,
    work: &mut BatchWork,
    failures: &mut Vec<HostError>,
) -> Option<MonoTime> {
    let now = host.now();
    if let Some(older_than) = now.0.checked_sub(RECHECK_PERIOD) {
        state.prune_failed(MonoTime(older_than));
    }
    let (jobs, next_due) = due_jobs(state, sources, started, now);
    if jobs.is_empty() {
        return next_due;
    }
    budget.take(1);
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(e) => {
            work.failed += 1;
            failures.push(e);
            return next_due;
        }
    };
    for job in jobs {
        if budget.spent(host) {
            break;
        }
        let (unit, done) = match job {
            Job::List(i) => {
                // One live round: finish it before another starts.
                let i = match state.round_finder() {
                    None => i,
                    Some(live) => match sources.iter().position(|s| s.finder == *live) {
                        Some(live) => live,
                        None => {
                            state.drop_round(); // its source left the run
                            i
                        }
                    },
                };
                // A new round only while its source is still due (a round
                // just ended by a redirected job must not start again).
                let source = &sources[i];
                let live = state.round_finder() == Some(&source.finder);
                let due = state
                    .listed_at(&source.finder)
                    .is_none_or(|at| host.now().saturating_since(at) >= DISCOVERY_PERIOD);
                if !live && !due {
                    continue;
                }
                (None, list(host, state, source, budget, work))
            }
            Job::Recheck(mount, path, identity) => {
                let done = recheck(host, state, &mounts, &mount, &path, identity, budget, work);
                (Some((mount, path)), done)
            }
            Job::Size(mount, path) => {
                // One live cursor: finish it first while it is still due.
                let (mount, path) = match state.cursor_unit() {
                    Some((m, p)) if due_to_size(state, m, p, host.now()) => {
                        (m.clone(), p.to_owned())
                    }
                    Some(_) => {
                        state.drop_cursor();
                        (mount, path)
                    }
                    None => (mount, path),
                };
                if !due_to_size(state, &mount, &path, host.now()) {
                    continue; // sized or evicted by an earlier job
                }
                let done = size(host, state, &mounts, &mount, &path, budget, work);
                (Some((mount, path)), done)
            }
        };
        if let Err(e) = done {
            work.failed += 1;
            if let Some((mount, path)) = unit {
                state.mark_failed(&mount, &path, host.now());
            }
            failures.push(e);
        }
    }
    next_due
}

/// Whether the single at `path` still needs sizing and has not just failed.
fn due_to_size(
    state: &TallyState,
    mount: &MountKey,
    path: &std::path::Path,
    now: MonoTime,
) -> bool {
    state
        .index
        .single(mount, path)
        .is_some_and(|s| needs_sizing(s, now))
        && state.failed_at(mount, path).is_none()
}

fn list<H: Host>(
    host: &H,
    state: &mut TallyState,
    source: &TallySource,
    budget: &mut Budget,
    work: &mut BatchWork,
) -> Result<(), HostError> {
    match state.list_round(host, source, budget)? {
        Rounded::Paused => work.listing += 1,
        Rounded::Done { recount, .. } => {
            work.evicted += recount.singles_evicted + recount.blocks_evicted;
            work.listed += 1;
        }
        Rounded::NoRoom => work.listing += 1,
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn recheck<H: Host>(
    host: &H,
    state: &mut TallyState,
    mounts: &[crate::host::MountEntry],
    mount: &MountKey,
    path: &std::path::Path,
    identity: Identity,
    budget: &mut Budget,
    work: &mut BatchWork,
) -> Result<(), HostError> {
    if state.index.single(mount, path).map(|s| s.identity) != Some(identity) {
        return Ok(()); // evicted or replaced by an earlier job
    }
    match reach(host, mounts, path, identity, budget)? {
        Some(_) => {
            let at = host.now();
            state
                .index
                .single_mut(mount, path)
                .expect("read above")
                .checked_at = at;
            work.rechecked += 1;
        }
        None => {
            state.evict_single(mount, path);
            work.evicted += 1;
        }
    }
    Ok(())
}

fn size<H: Host>(
    host: &H,
    state: &mut TallyState,
    mounts: &[crate::host::MountEntry],
    mount: &MountKey,
    path: &std::path::Path,
    budget: &mut Budget,
    work: &mut BatchWork,
) -> Result<(), HostError> {
    match state.size_unit(host, mounts, mount, path, budget)? {
        Sizing::Paused => work.paused += 1,
        Sizing::Gone => work.evicted += 1,
        Sizing::Done | Sizing::Refused(_) => work.sized += 1,
        // Its cursor does not fit the budget: wait, as after a failure.
        Sizing::NoRoom => {
            state.mark_failed(mount, path, host.now());
            work.paused += 1;
        }
        Sizing::Missing => {}
    }
    Ok(())
}

/// The jobs due at `now`, most overdue first (at most one batch's worth:
/// [`BATCH_OPS_MAX`]), and when the next job not yet due falls due. A
/// source never listed and a single never sized are due from `started`; a
/// single whose last job failed waits [`RECHECK_PERIOD`] from the failure.
pub fn due_jobs(
    state: &TallyState,
    sources: &[TallySource],
    started: MonoTime,
    now: MonoTime,
) -> (Vec<Job>, Option<MonoTime>) {
    let mut jobs = MostOverdue::new(BATCH_OPS_MAX);
    let mut next_due: Option<MonoTime> = None;
    let mut not_due = |last: MonoTime, period: Duration| {
        let due = last.checked_add(period).expect("monotonic time overflow");
        next_due = Some(next_due.map_or(due, |n: MonoTime| n.min(due)));
    };
    for (i, source) in sources.iter().enumerate() {
        // A live round stays due until it ends: `listed_at` is stamped
        // only when a round ends (or fails).
        let last = state.listed_at(&source.finder);
        match Lateness::of(last, started, DISCOVERY_PERIOD, now) {
            Some(late) => jobs.offer(late, Job::List(i)),
            None => not_due(
                last.expect("due at once when never listed"),
                DISCOVERY_PERIOD,
            ),
        }
    }
    state.index.for_each_unit(|mount, path, unit| {
        let UnitRef::Single(single) = unit else {
            return;
        };
        if let Some(failed) = state.failed_at(mount, path) {
            not_due(failed, RECHECK_PERIOD);
            return;
        }
        match Lateness::of(Some(single.checked_at), started, RECHECK_PERIOD, now) {
            Some(late) => jobs.offer(
                late,
                Job::Recheck(mount.clone(), path.to_owned(), single.identity),
            ),
            None => not_due(single.checked_at, RECHECK_PERIOD),
        }
        // Never sized: due from the tally's start (a unit without any
        // estimate must not starve behind overdue re-tallies, which its
        // own listings' refreshes of `checked_at` would make it do). A
        // refusal hint waits a period like a tally (`needs_sizing`).
        let last = last_sized(single);
        match Lateness::of(last, started, RETALLY_PERIOD, now) {
            Some(late) => jobs.offer(late, Job::Size(mount.clone(), path.to_owned())),
            None => not_due(last.unwrap_or(started), RETALLY_PERIOD),
        }
    });
    (jobs.into_jobs(), next_due)
}
