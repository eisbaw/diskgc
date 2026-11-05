//! Forwarding a command action's output to diskgc's stderr: bounded, lossy,
//! and never in the child's way (docs/safety.md, Command actions; TASK-342).
//!
//! The action's thread reads the child's pipes and hands each prefixed
//! segment (a line, or 4096 bytes of a longer one) to [`Forwarding::forward`],
//! which only queues it: it never waits for stderr. One writer thread for the
//! whole process writes the queue to stderr, one segment at a time, and may
//! block in that write for as long as stderr's consumer stalls (a busy log
//! file, a slow terminal, an unread pipe); nothing else waits for it.
//!
//! The queue is bounded ([`QUEUE_BYTES`]). A segment that does not fit
//! pushes out the oldest queued ones, so the newest output (the end of a
//! child's run, where a summary usually is) is what waits. When the action
//! ends, [`Forwarding::finish`] gives the writer a bounded time to catch up,
//! then withdraws whatever of the action is still queued. Every segment is
//! either written or counted as not forwarded: pushed out, withdrawn, failed
//! to write, or still being written when the action ended (that one may
//! still arrive later, so the count can exceed the true loss by one, never
//! fall short of it). Nothing of an action is written after its count was
//! taken, except that one segment.
//!
//! The writer writes to fd 2 itself, whatever it is at the time: no
//! duplicate is held while a segment waits, no stdio lock is taken, and fd
//! 2's file status flags are never changed (they are shared with every
//! process holding the same open file).

use std::collections::{BTreeMap, VecDeque};
use std::os::fd::AsFd;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// The most queued output, counting [`SEGMENT_OVERHEAD`] per segment: some
/// 10,000 short lines, about 240 segments of the longest kind.
pub(crate) const QUEUE_BYTES: usize = 1 << 20;

/// What a queued segment costs beside its bytes (its vector and the
/// allocator's slack), so a flood of tiny lines is bounded too.
const SEGMENT_OVERHEAD: usize = 64;

/// How often a waiting [`Forwarding::finish`] asks whether to stop early.
const STOP_POLL: Duration = Duration::from_millis(20);

/// Writes one segment, all of it, or fails. The writer thread's only
/// effect on the world.
pub(crate) type Sink = Box<dyn FnMut(&[u8]) -> std::io::Result<()> + Send>;

struct Segment {
    action: u64,
    bytes: Vec<u8>,
}

impl Segment {
    fn cost(&self) -> usize {
        self.bytes.len() + SEGMENT_OVERHEAD
    }
}

/// One action's share of the queue, between [`Forwarder::begin`] and the
/// end of its [`Forwarding`].
#[derive(Default)]
struct Counts {
    /// Its segments in the queue.
    queued: usize,
    /// Its segments not forwarded so far.
    lost: u64,
}

/// The queue and its bookkeeping. Only its methods change `segments`, so
/// `cost` and each action's `queued` always match them.
#[derive(Default)]
struct Queue {
    segments: VecDeque<Segment>,
    /// The summed [`Segment::cost`] of `segments`: at most [`QUEUE_BYTES`]
    /// between calls.
    cost: usize,
    /// The action whose segment the writer is writing, if any.
    writing: Option<u64>,
    actions: BTreeMap<u64, Counts>,
    next_action: u64,
}

impl Queue {
    /// Appends `segment`, then drops (and counts) the oldest segments
    /// until the queue is within its bound again.
    fn push(&mut self, segment: Segment) {
        debug_assert!(
            segment.cost() <= QUEUE_BYTES,
            "a segment larger than the queue"
        );
        self.cost += segment.cost();
        self.counts(segment.action, |counts| counts.queued += 1);
        self.segments.push_back(segment);
        while self.cost > QUEUE_BYTES {
            let Some(oldest) = self.pop() else {
                break;
            };
            self.count_lost(oldest.action);
        }
    }

    /// Takes the oldest segment off the queue.
    fn pop(&mut self) -> Option<Segment> {
        let segment = self.segments.pop_front()?;
        self.cost -= segment.cost();
        self.counts(segment.action, |counts| counts.queued -= 1);
        Some(segment)
    }

    /// Takes `action`'s segments off the queue; how many there were.
    fn withdraw(&mut self, action: u64) -> usize {
        let before = self.segments.len();
        let mut cost = 0;
        self.segments.retain(|segment| {
            let keep = segment.action != action;
            if !keep {
                cost += segment.cost();
            }
            keep
        });
        self.cost -= cost;
        self.counts(action, |counts| counts.queued = 0);
        before - self.segments.len()
    }

    fn count_lost(&mut self, action: u64) {
        self.counts(action, |counts| counts.lost += 1);
    }

    /// Applies `change` to `action`'s counts, if the action has not ended
    /// (a segment being written may outlive its action).
    fn counts(&mut self, action: u64, change: impl FnOnce(&mut Counts)) {
        if let Some(counts) = self.actions.get_mut(&action) {
            change(counts);
        }
    }

    /// Whether anything of `action` is queued or being written.
    fn pending(&self, action: u64) -> bool {
        self.writing == Some(action)
            || self
                .actions
                .get(&action)
                .is_some_and(|counts| counts.queued > 0)
    }
}

struct Shared {
    queue: Mutex<Queue>,
    /// Signalled when a segment is queued and when a write ends.
    changed: Condvar,
}

impl Shared {
    /// The queue, also after a panic elsewhere left the lock poisoned: its
    /// state is consistent after every statement that holds it.
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The queue and its one writer thread.
pub(crate) struct Forwarder {
    shared: Arc<Shared>,
}

impl Forwarder {
    /// Starts the writer thread, named `name`, writing to `sink`. It runs
    /// for the life of the process.
    pub(crate) fn start(name: &str, mut sink: Sink) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            changed: Condvar::new(),
        });
        let writer = Arc::clone(&shared);
        thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                loop {
                    let segment = {
                        let mut queue = writer.lock();
                        let segment = loop {
                            if let Some(segment) = queue.pop() {
                                break segment;
                            }
                            queue = writer
                                .changed
                                .wait(queue)
                                .unwrap_or_else(PoisonError::into_inner);
                        };
                        queue.writing = Some(segment.action);
                        segment
                    };
                    let written = sink(&segment.bytes);
                    let mut queue = writer.lock();
                    queue.writing = None;
                    if written.is_err() {
                        queue.count_lost(segment.action);
                    }
                    drop(queue);
                    writer.changed.notify_all();
                }
            })
            .map_err(|error| format!("start the command output writer: {error}"))?;
        Ok(Self { shared })
    }

    /// One action's forwarding, counted apart from any other's.
    pub(crate) fn begin(&self) -> Forwarding<'_> {
        let mut queue = self.shared.lock();
        let action = queue.next_action;
        queue.next_action += 1;
        queue.actions.insert(action, Counts::default());
        Forwarding {
            shared: &self.shared,
            action,
            ended: false,
        }
    }
}

/// The process's forwarder to stderr, started on first use.
pub(crate) fn to_stderr() -> Result<&'static Forwarder, String> {
    static STDERR: OnceLock<Result<Forwarder, String>> = OnceLock::new();
    STDERR
        .get_or_init(|| Forwarder::start("diskgc-command-output", Box::new(write_stderr)))
        .as_ref()
        .map_err(Clone::clone)
}

/// All of `bytes` to fd 2. A nonblocking fd 2 (its flags belong to whoever
/// shares the open file) is waited for with `poll`, as a blocking one would
/// wait in `write`.
fn write_stderr(mut bytes: &[u8]) -> std::io::Result<()> {
    // `AsFd` of std's handle names fd 2 without taking its lock.
    let handle = std::io::stderr();
    let stderr = handle.as_fd();
    while !bytes.is_empty() {
        match rustix::io::write(stderr, bytes) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::AGAIN) => {
                let mut fds = [rustix::event::PollFd::new(
                    &stderr,
                    rustix::event::PollFlags::OUT,
                )];
                match rustix::event::poll(&mut fds, None) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// One action's share of a [`Forwarder`].
pub(crate) struct Forwarding<'f> {
    shared: &'f Shared,
    action: u64,
    ended: bool,
}

impl Forwarding<'_> {
    /// Queues `segment` for stderr without waiting: if the queue is full,
    /// the oldest queued segments are dropped (and counted) to make room.
    pub(crate) fn forward(&self, segment: Vec<u8>) {
        let segment = Segment {
            action: self.action,
            bytes: segment,
        };
        self.shared.lock().push(segment);
        self.shared.changed.notify_all();
    }

    /// Ends the action's forwarding: waits up to `wait` (or until `stop`
    /// says so) for the writer to write what is queued, then withdraws the
    /// rest. Returns how many segments were not forwarded, a segment still
    /// being written included. `stop` is asked with the queue locked: it
    /// must not forward (an atomic load, such as a signal flag, is right).
    pub(crate) fn finish(mut self, wait: Duration, stop: &dyn Fn() -> bool) -> u64 {
        let until = Instant::now().checked_add(wait);
        let mut queue = self.shared.lock();
        while queue.pending(self.action) && !stop() {
            let now = Instant::now();
            let left = until.map_or(STOP_POLL, |until| until.saturating_duration_since(now));
            if left.is_zero() {
                break;
            }
            queue = self
                .shared
                .changed
                .wait_timeout(queue, left.min(STOP_POLL))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        self.end(&mut queue)
    }

    /// Withdraws the action's queued segments and takes its count.
    fn end(&mut self, queue: &mut Queue) -> u64 {
        self.ended = true;
        let withdrawn = queue.withdraw(self.action);
        let in_flight = u64::from(queue.writing == Some(self.action));
        let lost = queue.actions.remove(&self.action).map_or(0, |c| c.lost);
        lost + in_flight + u64::try_from(withdrawn).unwrap_or(u64::MAX)
    }
}

impl Drop for Forwarding<'_> {
    /// A forwarding left without [`Forwarding::finish`] (an early error
    /// return) withdraws its segments too, so none outlives its action.
    fn drop(&mut self) {
        if !self.ended {
            let mut queue = self.shared.lock();
            self.end(&mut queue);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A sink that records what it wrote and blocks each write until the
    /// test lets it go (`gate`), or fails it.
    struct TestSink {
        written: Arc<Mutex<Vec<Vec<u8>>>>,
        gate: mpsc::Sender<bool>,
    }

    fn gated_forwarder() -> (Forwarder, TestSink) {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (gate, opened) = mpsc::channel::<bool>();
        let record = Arc::clone(&written);
        let forwarder = Forwarder::start(
            "test-command-output",
            Box::new(move |bytes| {
                // `true` lets one write through; `false` fails it; a closed
                // gate (the test ended) lets everything through.
                match opened.recv() {
                    Ok(true) | Err(_) => {
                        record.lock().unwrap().push(bytes.to_vec());
                        Ok(())
                    }
                    Ok(false) => Err(std::io::ErrorKind::BrokenPipe.into()),
                }
            }),
        )
        .expect("start test forwarder");
        (forwarder, TestSink { written, gate })
    }

    fn segment(i: usize) -> Vec<u8> {
        format!("diskgc: tier/profile: line {i}\n").into_bytes()
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn a_stalled_sink_never_holds_up_forward_and_every_segment_is_accounted_for() {
        let (forwarder, sink) = gated_forwarder();
        let forwarding = forwarder.begin();
        let lines = 50_000;
        let started = Instant::now();
        for i in 0..lines {
            forwarding.forward(segment(i));
        }
        let forwarded_in = started.elapsed();
        // 50,000 queue operations; a write would block forever here.
        assert!(
            forwarded_in < Duration::from_secs(5),
            "forward waited for the stalled sink: {forwarded_in:?}"
        );
        {
            let queue = forwarding.shared.lock();
            assert!(queue.cost <= QUEUE_BYTES, "queue grew past its bound");
            assert!(queue.segments.len() < lines, "nothing was dropped");
        }
        let started = Instant::now();
        let not_forwarded = forwarding.finish(Duration::from_millis(100), &never);
        assert!(started.elapsed() < Duration::from_secs(2));
        // The writer took one segment and is blocked in it: counted too.
        assert_eq!(not_forwarded, u64::try_from(lines).unwrap());
        assert_eq!(forwarder.shared.lock().cost, 0, "withdrawn segments left");
        sink.gate.send(true).unwrap();
        drop(sink.gate);
    }

    #[test]
    fn the_newest_output_is_what_waits_when_the_queue_overflows() {
        let (forwarder, sink) = gated_forwarder();
        let forwarding = forwarder.begin();
        let lines = 40_000;
        for i in 0..lines {
            forwarding.forward(segment(i));
        }
        let (first_queued, last_queued) = {
            let queue = forwarding.shared.lock();
            (
                queue.segments.front().unwrap().bytes.clone(),
                queue.segments.back().unwrap().bytes.clone(),
            )
        };
        assert_eq!(last_queued, segment(lines - 1));
        assert_ne!(first_queued, segment(1), "the oldest were not dropped");
        drop(sink.gate);
        let not_forwarded = forwarding.finish(Duration::from_secs(10), &never);
        let written = sink.written.lock().unwrap().len();
        // Segment 0 was taken before the queue filled; the rest that fit
        // were written once the gate opened.
        assert_eq!(written as u64 + not_forwarded, lines as u64);
        assert!(written > 1);
        assert_eq!(
            sink.written.lock().unwrap().last().unwrap(),
            &segment(lines - 1)
        );
    }

    #[test]
    fn a_free_sink_forwards_everything_in_order_and_counts_nothing() {
        let (forwarder, sink) = gated_forwarder();
        drop(sink.gate);
        let forwarding = forwarder.begin();
        for i in 0..1000 {
            forwarding.forward(segment(i));
        }
        assert_eq!(forwarding.finish(Duration::from_secs(10), &never), 0);
        let written = sink.written.lock().unwrap().clone();
        assert_eq!(written, (0..1000).map(segment).collect::<Vec<_>>());
    }

    #[test]
    fn a_failed_write_is_counted_for_its_own_action() {
        let (forwarder, sink) = gated_forwarder();
        let first = forwarder.begin();
        first.forward(segment(0));
        first.forward(segment(1));
        sink.gate.send(false).unwrap();
        sink.gate.send(true).unwrap();
        assert_eq!(first.finish(Duration::from_secs(10), &never), 1);
        let second = forwarder.begin();
        second.forward(segment(2));
        sink.gate.send(true).unwrap();
        assert_eq!(second.finish(Duration::from_secs(10), &never), 0);
        assert_eq!(*sink.written.lock().unwrap(), vec![segment(1), segment(2)]);
    }

    #[test]
    fn a_writer_stuck_in_one_action_counts_the_next_action_whole_and_the_first_once() {
        let (forwarder, sink) = gated_forwarder();
        let first = forwarder.begin();
        first.forward(segment(0));
        // The writer takes segment 0 and blocks in it.
        let deadline = Instant::now() + Duration::from_secs(10);
        while forwarder.shared.lock().writing.is_none() {
            assert!(Instant::now() < deadline, "the writer never took segment 0");
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(first.finish(Duration::ZERO, &never), 1);
        let second = forwarder.begin();
        for i in 1..=3 {
            second.forward(segment(i));
        }
        // Still stuck in the first action's segment: the second's three
        // are withdrawn; the first's is not charged to the second.
        assert_eq!(second.finish(Duration::from_millis(50), &never), 3);
        sink.gate.send(false).unwrap();
        let third = forwarder.begin();
        third.forward(segment(4));
        sink.gate.send(true).unwrap();
        // The first action's failed write, after it ended, is nobody's.
        assert_eq!(third.finish(Duration::from_secs(10), &never), 0);
        assert_eq!(*sink.written.lock().unwrap(), vec![segment(4)]);
    }

    #[test]
    fn drop_oldest_charges_each_dropped_segment_to_its_own_action() {
        let (forwarder, sink) = gated_forwarder();
        let first = forwarder.begin();
        let second = forwarder.begin();
        first.forward(segment(0));
        let deadline = Instant::now() + Duration::from_secs(10);
        while forwarder.shared.lock().writing.is_none() {
            assert!(Instant::now() < deadline, "the writer never took segment 0");
            thread::sleep(Duration::from_millis(1));
        }
        // Queue: 100 of the first's, then enough of the second's to push
        // out every one of the first's and some of its own.
        for i in 1..=100 {
            first.forward(segment(i));
        }
        let per = segment(0).len() + SEGMENT_OVERHEAD;
        let seconds = QUEUE_BYTES / per + 50;
        for i in 0..seconds {
            second.forward(segment(i));
        }
        assert_eq!(forwarder.shared.lock().actions[&first.action].queued, 0);
        // The first: 100 pushed out, plus its segment still being written.
        assert_eq!(first.finish(Duration::ZERO, &never), 101);
        // The second: every one, pushed out or withdrawn.
        assert_eq!(
            second.finish(Duration::ZERO, &never),
            u64::try_from(seconds).unwrap()
        );
        assert_eq!(forwarder.shared.lock().cost, 0);
        drop(sink.gate);
    }

    #[test]
    fn finish_stops_waiting_when_asked() {
        let (forwarder, sink) = gated_forwarder();
        let forwarding = forwarder.begin();
        forwarding.forward(segment(0));
        forwarding.forward(segment(1));
        let started = Instant::now();
        let not_forwarded = forwarding.finish(Duration::from_secs(60), &|| true);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(not_forwarded, 2);
        drop(sink.gate);
    }

    #[test]
    fn finish_notices_a_stop_that_comes_while_it_waits() {
        let (forwarder, sink) = gated_forwarder();
        let forwarding = forwarder.begin();
        forwarding.forward(segment(0));
        forwarding.forward(segment(1));
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        let setter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let started = Instant::now();
        let not_forwarded = forwarding.finish(Duration::from_secs(60), &|| {
            stopped.load(std::sync::atomic::Ordering::SeqCst)
        });
        let took = started.elapsed();
        setter.join().unwrap();
        assert!(
            took >= Duration::from_millis(100) && took < Duration::from_secs(5),
            "finish did not wait for, or did not notice, the stop: {took:?}"
        );
        assert_eq!(not_forwarded, 2);
        drop(sink.gate);
    }

    #[test]
    fn a_dropped_forwarding_leaves_nothing_queued() {
        let (forwarder, sink) = gated_forwarder();
        {
            let forwarding = forwarder.begin();
            for i in 0..10 {
                forwarding.forward(segment(i));
            }
        }
        let queue = forwarder.shared.lock();
        assert!(queue.segments.is_empty() && queue.cost == 0 && queue.actions.is_empty());
        drop(queue);
        drop(sink.gate);
    }
}
