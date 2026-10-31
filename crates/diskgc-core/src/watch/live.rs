//! `watch`'s live index and its background tally (TASK-236;
//! docs/architecture.md, The tally: Background).
//!
//! **Ownership.** One [`TallyState`] serves two users: the background
//! tally, which changes it in mini-batches (at most 50 ms each), and the
//! reclaim cycle and the snapshot encoding, which need all of it at once.
//! They never work on it at once, and the contract wants exactly that: the
//! tally pauses while a reclaim runs. So the state sits behind one
//! [`Mutex`] with two kinds of critical section:
//!
//! - the tally holds it for one turn (a batch, or the pause and pressure
//!   tests) and sleeps without it;
//! - a cycle holds it for its run, the snapshot tick only to encode it
//!   (the file is written outside).
//!
//! A lock with that shape is simpler than message passing (lending the
//! state to the tally and taking it back is the same exclusion with a
//! protocol of its own to get wrong), and the cycle's long hold is the
//! pause the contract asks for, not contention. The tally additionally
//! pauses while *another* process holds the reclaim lock (its own probe,
//! [`crate::hostlock::held_by_another`]).
//!
//! **Never waiting on the tally.** The watcher takes the lock by
//! `try_lock` in [`POLL`] steps that check for a signal and a bound
//! ([`LiveIndex::with_state`]): a tally turn blocked in the kernel (it
//! holds the lock) costs a cycle its index, never the cycle (it runs on an
//! empty index: discovery alone, which decides anyway), never a stop.
//! At the end the tally thread is stopped and joined for at most
//! [`JOIN_WAIT`]; a thread still blocked then is left to the process's
//! exit ([`TallyThread`]).
//!
//! **Threads.** [`Host::lower_priority`] applies to the calling thread
//! only, so the tally runs on a thread of its own ([`TallyThread`]) that
//! lowers its own priority and starts no reclaim work; the reclaim runs on
//! the caller's thread at its priority ([`Threaded`]). The model has no
//! threads: [`Inline`] runs the same turns on the one thread, between the
//! watcher's waits, on the virtual clock, and does not lower the
//! priority (so it is never wrong on a real host either, only not
//! idle).
//!
//! **Stopping.** The watcher's waits and the tally's sleeps are cut into
//! [`POLL`] steps that check for a signal, so both stop within one step
//! (plus a running batch) on a real host. On the model's virtual clock a
//! sleep runs whole (one traced call), so a signal is seen at the next
//! wake.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Duration;

use crate::host::{Host, MonoTime};
use crate::hostlock::ReclaimLock;
use crate::tally::TallyState;
use crate::tally::background::{Background, Event};
use crate::tally::listing::TallySource;

/// The longest a real-host wait or tally sleep goes without checking for
/// a signal: the contract's "the background tally stops at its batch
/// boundary (at most 50 ms)".
pub const POLL: Duration = Duration::from_millis(50);

/// How long the watcher waits for the tally to let go of the live index
/// before a cycle runs without it (a batch takes at most 50 ms; more is a
/// turn blocked in the kernel).
pub const LOCK_WAIT: Duration = Duration::from_secs(1);

/// How long the end of `watch` waits for the stopped tally thread.
pub const JOIN_WAIT: Duration = Duration::from_secs(2);

/// The live index as the watcher sees it: use of it for a cycle or a
/// snapshot encoding, and waits during which the background tally runs.
pub trait LiveIndex {
    /// Runs `f` with exclusive use of the live index (the background tally
    /// runs no batch meanwhile), or with `None` when the tally did not let
    /// go of it by `until`.
    fn with_state<R>(&mut self, until: MonoTime, f: impl FnOnce(Option<&mut TallyState>) -> R)
    -> R;

    /// Waits until `until`, or until a signal arrives; the background tally
    /// runs meanwhile.
    fn wait_until(&mut self, until: MonoTime);
}

/// The background tally's events as `watch`'s log lines
/// (docs/reference.md, Watch log): the pressure brake's engaging and
/// releasing, each with the batches since the previous such line; a pause
/// for the host lock when it starts and when it ends; what cannot work
/// (priority, PSI, the host lock) and failures, each once. A batch is no
/// line.
#[derive(Debug, Default)]
pub struct TallyLog {
    braked: bool,
    /// The lock a pause is waiting for, while paused.
    paused: Option<ReclaimLock>,
    /// Batches run (or skipped, while braked) since the last brake line.
    batches: u64,
}

impl TallyLog {
    /// The line `event` makes, if any.
    pub fn line(&mut self, event: &Event) -> Option<String> {
        let text = match event {
            Event::Started { .. } => return None,
            Event::PriorityNotLowered(e) => {
                format!("background tally: could not lower its priority ({e}); it runs as it is")
            }
            Event::NoPsi(e) => format!(
                "background tally: no pressure stall information ({e}): the pressure brake is \
                 off, the duty cycle alone applies"
            ),
            Event::PsiUnreadable(e) => format!(
                "background tally: a pressure reading failed ({e}); that batch ran unbraked \
                 (said once)"
            ),
            Event::Paused(lock) => {
                if self.paused.replace(lock.clone()).is_some() {
                    return None;
                }
                format!(
                    "background tally paused: {} is held (a reclaim runs)",
                    Named(lock)
                )
            }
            Event::HostLockUnusable(e) => format!("background tally: {e}"),
            Event::Braked { reading, sleep } => {
                self.paused = None;
                if std::mem::replace(&mut self.braked, true) {
                    self.batches += 1;
                    return None;
                }
                let n = std::mem::take(&mut self.batches);
                format!(
                    "background tally: pressure brake engaged after {n} batch(es) (io {}, \
                     memory {}, cpu {}); sleeping {} s",
                    percent(reading.io),
                    percent(reading.memory),
                    percent(reading.cpu),
                    sleep.as_secs()
                )
            }
            Event::Batch(_) => {
                let resumed = self.paused.take();
                if std::mem::replace(&mut self.braked, false) {
                    let n = std::mem::take(&mut self.batches);
                    self.batches = 1;
                    format!("background tally: pressure brake released after {n} skipped batch(es)")
                } else {
                    self.batches += 1;
                    let lock = resumed?;
                    format!("background tally resumed: {} is free", Named(&lock))
                }
            }
            Event::Failed(e) => format!("background tally: {e}"),
        };
        Some(text)
    }
}

/// A lock as the pause lines name it: "the host lock" as before TASK-277,
/// a per-user lock with its path.
struct Named<'a>(&'a ReclaimLock);

impl std::fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            ReclaimLock::Host => f.write_str("the host lock"),
            lock @ ReclaimLock::User(_) => write!(f, "the {lock}"),
        }
    }
}

/// Hundredths of a percent, as PSI's `avg10` reads.
fn percent(hundredths: u32) -> String {
    format!("{}.{:02} %", hundredths / 100, hundredths % 100)
}

/// Sleeps `duration` in [`POLL`] steps until `stop` says so.
fn nap<H: Host>(host: &H, duration: Duration, stop: &dyn Fn() -> bool) {
    let end = host.now().checked_add(duration);
    while !stop() {
        let left = end.map_or(POLL, |end| end.saturating_since(host.now()));
        if left.is_zero() {
            return;
        }
        host.sleep(left.min(POLL));
    }
}

/// The live index between the threads of a real host: the state behind
/// one lock, and the flag that stops the tally thread.
#[derive(Debug)]
pub struct Shared {
    state: Mutex<TallyState>,
    stop: AtomicBool,
}

impl Shared {
    pub fn new(state: TallyState) -> Self {
        Self {
            state: Mutex::new(state),
            stop: AtomicBool::new(false),
        }
    }

    /// Tells the tally thread to end at its next check.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Holds the live index as one tally turn does, for as long as the guard
    /// lives: for tests of a turn that does not end (a tally blocked in the
    /// kernel). The watcher never waits on it ([`LiveIndex::with_state`]).
    #[doc(hidden)]
    pub fn hold(&self) -> std::sync::MutexGuard<'_, TallyState> {
        self.state
            .lock()
            .expect("the live index's other user panicked")
    }

    /// The live index, waiting for it in [`POLL`] steps until `until`
    /// (`None` then).
    fn try_lock_until<H: Host>(
        &self,
        host: &H,
        until: MonoTime,
    ) -> Option<std::sync::MutexGuard<'_, TallyState>> {
        loop {
            match self.state.try_lock() {
                Ok(guard) => return Some(guard),
                Err(TryLockError::Poisoned(_)) => {
                    panic!("the background tally panicked holding the live index")
                }
                Err(TryLockError::WouldBlock) => {}
            }
            // Not cut short by a signal: the stop's snapshot needs the index
            // too, and the wait is bounded (at most `LOCK_WAIT`).
            let now = host.now();
            if now >= until {
                return None;
            }
            host.sleep(until.saturating_since(now).min(POLL));
        }
    }
}

/// The background tally's thread (see the module doc): lowers its own
/// priority, then turns until [`Shared::stop`] or a signal, holding the
/// live index for one turn at a time. `log` takes its lines
/// ([`TallyLog`]).
pub fn tally_thread<H: Host + Sync>(
    host: &H,
    shared: &Shared,
    sources: &[TallySource],
    log: &(dyn Fn(&str) + Sync),
) {
    let mut lines = TallyLog::default();
    let mut emit = |event: Event| {
        if let Some(line) = lines.line(&event) {
            log(&line);
        }
    };
    let mut background = Background::start(host, &mut emit);
    let stop = || shared.stopped() || host.interrupted().is_some();
    while !stop() {
        let sleep = {
            let mut state = shared
                .state
                .lock()
                .expect("the watcher panicked holding the live index");
            background.turn(host, &mut state, sources, &mut emit)
        };
        nap(host, sleep, &stop);
    }
}

/// The background tally's thread, running ([`TallyThread::spawn`]).
/// Dropping it, also while a panic unwinds the watcher's loop, stops the
/// tally and waits for its thread at most [`JOIN_WAIT`]; one still blocked
/// in the kernel then is detached and ends with the process (it only
/// reads).
pub struct TallyThread {
    shared: Arc<Shared>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TallyThread {
    /// Starts [`tally_thread`] on a thread of its own, `diskgc-tally`.
    pub fn spawn<H: Host + Sync + 'static>(
        host: &'static H,
        shared: Arc<Shared>,
        sources: Vec<TallySource>,
        log: &'static (dyn Fn(&str) + Sync),
    ) -> Self {
        let theirs = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("diskgc-tally".to_owned())
            .spawn(move || tally_thread(host, &theirs, &sources, log))
            .expect("spawn the background tally's thread");
        Self {
            shared,
            handle: Some(handle),
        }
    }

    /// Whether the thread has ended: before a stop, only by a panic.
    pub fn ended(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }
}

impl Drop for TallyThread {
    fn drop(&mut self) {
        self.shared.stop();
        let Some(handle) = self.handle.take() else {
            return;
        };
        let deadline = std::time::Instant::now() + JOIN_WAIT;
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            // Its panic, if any, was the watcher's to see through `ended`.
            let _ = handle.join();
        }
        // Otherwise detached: it ends with the process.
    }
}

/// Runs `main` on the caller's thread while the background tally runs on
/// a thread of its own ([`TallyThread`]); stops it on return or unwind.
/// `main` gets whether the tally thread has ended (before the stop, only
/// by a panic). The reclaim runs in `main`, so it keeps the caller's
/// priority.
pub fn with_tally_thread<H: Host + Sync + 'static, R>(
    host: &'static H,
    shared: Arc<Shared>,
    sources: Vec<TallySource>,
    log: &'static (dyn Fn(&str) + Sync),
    main: impl FnOnce(&dyn Fn() -> bool) -> R,
) -> R {
    let tally = TallyThread::spawn(host, shared, sources, log);
    main(&|| tally.ended())
}

/// The watcher's side on a real host (see the module doc): the tally runs
/// on its own thread ([`TallyThread`]); this waits in [`POLL`] steps and
/// tries the lock for a cycle or a snapshot encoding.
pub struct Threaded<'a, H> {
    host: &'a H,
    shared: &'a Shared,
    /// Whether the tally thread has ended: before a stop, only by a panic.
    tally_ended: &'a dyn Fn() -> bool,
}

impl<'a, H: Host> Threaded<'a, H> {
    pub fn new(host: &'a H, shared: &'a Shared, tally_ended: &'a dyn Fn() -> bool) -> Self {
        Self {
            host,
            shared,
            tally_ended,
        }
    }
}

impl<H: Host> LiveIndex for Threaded<'_, H> {
    fn with_state<R>(
        &mut self,
        until: MonoTime,
        f: impl FnOnce(Option<&mut TallyState>) -> R,
    ) -> R {
        match self.shared.try_lock_until(self.host, until) {
            Some(mut guard) => f(Some(&mut guard)),
            None => f(None),
        }
    }

    fn wait_until(&mut self, until: MonoTime) {
        let host = self.host;
        let ended = self.tally_ended;
        nap(host, until.saturating_since(host.now()), &|| {
            // A tally that ended by itself panicked: so does the watcher
            // (exit 101), rather than run on with an index nobody keeps.
            assert!(
                self.shared.stopped() || host.interrupted().is_some() || !ended(),
                "the background tally thread ended unexpectedly"
            );
            host.interrupted().is_some()
        });
    }
}

/// The live index and its background tally on one thread, for the model
/// (see the module doc): the tally's turns run during the watcher's
/// waits, each when its sleep is over, at the caller's priority.
pub struct Inline<'a, H> {
    host: &'a H,
    state: TallyState,
    background: Background,
    sources: Vec<TallySource>,
    next_turn: MonoTime,
    lines: TallyLog,
    log: &'a mut dyn FnMut(&str),
}

impl<'a, H: Host> Inline<'a, H> {
    /// Starts the background tally (its PSI probe) on the caller's thread,
    /// without lowering its priority.
    pub fn new(
        host: &'a H,
        state: TallyState,
        sources: Vec<TallySource>,
        log: &'a mut dyn FnMut(&str),
    ) -> Self {
        let mut lines = TallyLog::default();
        let background = Background::start_at_caller_priority(host, &mut |event| {
            if let Some(line) = lines.line(&event) {
                log(&line);
            }
        });
        Self {
            host,
            state,
            background,
            sources,
            next_turn: host.now(),
            lines,
            log,
        }
    }

    /// The live index, after the run.
    pub fn into_state(self) -> TallyState {
        self.state
    }
}

impl<H: Host> LiveIndex for Inline<'_, H> {
    fn with_state<R>(
        &mut self,
        _until: MonoTime,
        f: impl FnOnce(Option<&mut TallyState>) -> R,
    ) -> R {
        f(Some(&mut self.state))
    }

    fn wait_until(&mut self, until: MonoTime) {
        let host = self.host;
        while host.interrupted().is_none() {
            let now = host.now();
            if now >= until {
                return;
            }
            if now >= self.next_turn {
                let (lines, log) = (&mut self.lines, &mut self.log);
                let sleep =
                    self.background
                        .turn(host, &mut self.state, &self.sources, &mut |event| {
                            if let Some(line) = lines.line(&event) {
                                log(&line);
                            }
                        });
                self.next_turn = host
                    .now()
                    .checked_add(sleep)
                    .expect("monotonic time overflow");
                continue;
            }
            host.sleep(self.next_turn.min(until).saturating_since(now));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{HostError, Pressure};
    use crate::tally::background::BatchWork;

    #[test]
    fn the_brake_is_two_lines_with_batch_counts_and_batches_are_silent() {
        let mut log = TallyLog::default();
        let batch = Event::Batch(BatchWork::default());
        let braked = Event::Braked {
            reading: Pressure {
                io: 30_00,
                memory: 0,
                cpu: 0,
            },
            sleep: Duration::from_secs(2),
        };
        assert_eq!(log.line(&batch), None);
        assert_eq!(log.line(&batch), None);
        let on = log.line(&braked).expect("engaged");
        assert!(on.contains("engaged after 2 batch(es)"), "{on}");
        assert!(on.contains("io 30.00 %"), "{on}");
        assert_eq!(log.line(&braked), None);
        assert_eq!(log.line(&braked), None);
        let off = log.line(&batch).expect("released");
        assert!(off.contains("released after 2 skipped"), "{off}");
        assert_eq!(log.line(&batch), None);
    }

    #[test]
    fn a_pause_is_one_line_and_the_resume_one_more() {
        let mut log = TallyLog::default();
        assert!(log.line(&Event::Paused(ReclaimLock::Host)).is_some());
        assert_eq!(log.line(&Event::Paused(ReclaimLock::Host)), None);
        let resumed = log.line(&Event::Batch(BatchWork::default()));
        assert!(resumed.is_some_and(|l| l.contains("resumed")));
        assert_eq!(log.line(&Event::Batch(BatchWork::default())), None);
        let no_psi = log.line(&Event::NoPsi(HostError::invalid("read", "x", "absent")));
        assert!(no_psi.is_some_and(|l| l.contains("pressure brake is off")));
    }
}
