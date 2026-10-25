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
//!   resumed across batches by the live cursor). A listing is not
//!   resumable: it runs whole, only as a batch's first job, and the duty
//!   cycle charges its time (it can exceed the batch bounds; see
//!   [`Event::Batch`]);
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
use super::recount::Complete;
use super::schedule::{DISCOVERY_PERIOD, Lateness, MostOverdue, RECHECK_PERIOD, RETALLY_PERIOD};
use super::size::reach;
use super::{Sizing, TallyState};
use crate::host::{Host, HostError, MonoTime, Pressure};
use crate::hostlock::{self, LockError};
use crate::index::{Identity, MountKey, UnitRef};

/// How often a paused tally tests the host lock again.
pub const LOCK_POLL: Duration = Duration::from_secs(1);
/// The longest sleep when nothing is due (so a new source or a changed
/// clock is noticed within it).
pub const IDLE_SLEEP_MAX: Duration = Duration::from_secs(60);

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
    /// A failure of the host stopped a batch (its jobs stay due).
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
    pub listed: usize,
    pub rechecked: usize,
    pub evicted: usize,
    pub sized: usize,
    /// Sizing walks that paused at the batch's end.
    pub paused: usize,
    /// The sleep after it.
    pub sleep: Duration,
}

impl BatchWork {
    fn jobs(&self) -> usize {
        self.listed + self.rechecked + self.evicted + self.sized + self.paused
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
        let run = batch(host, state, sources, self.started, &mut budget, &mut work);
        work.took = host.now().saturating_since(start);
        work.ops = budget.ops();
        let mut sleep = self.pace.after_batch(work.took);
        match run {
            Ok(next_due) if work.jobs() == 0 => {
                let until_due = next_due.map_or(IDLE_SLEEP_MAX, |due| {
                    due.saturating_since(host.now()).min(IDLE_SLEEP_MAX)
                });
                sleep = sleep.max(until_due);
            }
            Ok(_) => {}
            Err(e) => log(Event::Failed(e)),
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

/// One batch: the most overdue jobs within `budget`. Returns when the next
/// job not yet due falls due (`None`: none known).
fn batch<H: Host>(
    host: &H,
    state: &mut TallyState,
    sources: &[TallySource],
    started: MonoTime,
    budget: &mut Budget,
    work: &mut BatchWork,
) -> Result<Option<MonoTime>, HostError> {
    let now = host.now();
    let (jobs, next_due) = due_jobs(state, sources, started, now);
    if jobs.is_empty() {
        return Ok(next_due);
    }
    budget.take(1);
    let mounts = host.mountinfo()?;
    for (n, job) in jobs.into_iter().enumerate() {
        if budget.spent(host) {
            break;
        }
        match job {
            Job::List(i) => {
                // Not resumable: only as the batch's first job.
                if n > 0 {
                    break;
                }
                let source = &sources[i];
                let listed = state.list_source(host, source, None)?;
                budget.take(listed.listing.listed.len() + listed.listing.unindexed.len() + 2);
                if listed.listing.complete {
                    let recount = state.recount(&[Complete {
                        finder: &source.finder,
                        listed: &listed.listing.listed,
                        unindexed: &listed.listing.unindexed,
                    }]);
                    work.evicted += recount.singles_evicted + recount.blocks_evicted;
                }
                work.listed += 1;
            }
            Job::Recheck(mount, path, identity) => {
                if state.index.single(&mount, &path).map(|s| s.identity) != Some(identity) {
                    continue; // evicted or replaced by an earlier job
                }
                match reach(host, &mounts, &path, identity, budget)? {
                    Some(_) => {
                        let at = host.now();
                        state
                            .index
                            .single_mut(&mount, &path)
                            .expect("read above")
                            .checked_at = at;
                        work.rechecked += 1;
                    }
                    None => {
                        state.index.evict_single(&mount, &path);
                        work.evicted += 1;
                    }
                }
            }
            Job::Size(mount, path) => {
                // One live cursor: finish it before starting another walk.
                let (mount, path) = match state.cursor_unit() {
                    Some((m, p)) => (m.clone(), p.to_owned()),
                    None => (mount, path),
                };
                let now = host.now();
                let still_due = state.index.single(&mount, &path).is_some_and(|s| {
                    s.tally
                        .is_none_or(|t| now.saturating_since(t.tallied_at) >= RETALLY_PERIOD)
                });
                if !still_due {
                    continue; // sized by an earlier job (the live cursor's)
                }
                match state.size_unit(host, &mounts, &mount, &path, budget)? {
                    Sizing::Paused => work.paused += 1,
                    Sizing::Gone => work.evicted += 1,
                    Sizing::Done | Sizing::Refused(_) => work.sized += 1,
                    Sizing::Missing | Sizing::NoRoom => {}
                }
            }
        }
    }
    Ok(next_due)
}

/// The jobs due at `now`, most overdue first (at most one batch's worth:
/// [`BATCH_OPS_MAX`]), and when the next job not yet due falls due. A
/// source never listed is due from `started`; a single never sized from
/// when it was last found (its `checked_at`).
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
        match Lateness::of(Some(single.checked_at), started, RECHECK_PERIOD, now) {
            Some(late) => jobs.offer(
                late,
                Job::Recheck(mount.clone(), path.to_owned(), single.identity),
            ),
            None => not_due(single.checked_at, RECHECK_PERIOD),
        }
        // Never sized: due from when it was last found. A refusal hint
        // waits a period like a tally.
        let last = single
            .tally
            .map(|t| t.tallied_at)
            .into_iter()
            .chain(single.refusal.map(|r| r.at))
            .max();
        match Lateness::of(last, single.checked_at, RETALLY_PERIOD, now) {
            Some(late) => jobs.offer(late, Job::Size(mount.clone(), path.to_owned())),
            None => match last {
                Some(last) => not_due(last, RETALLY_PERIOD),
                None => not_due(single.checked_at, Duration::ZERO),
            },
        }
    });
    (jobs.into_jobs(), next_due)
}
