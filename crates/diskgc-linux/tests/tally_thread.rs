//! TASK-236: `watch`'s background tally lowers the priority of its own
//! thread, never the caller's, where the reclaim runs
//! ([`diskgc_core::watch::live::with_tally_thread`], which
//! `diskgc_core::watch::run_threaded` runs the watcher's loop in). On the
//! real kernel, since `setpriority` and `ioprio_set` act per thread there.
//!
//! It touches nothing but this test process's own threads and `/proc`:
//! the tally has no sources, so it lists nothing, and its pause probe
//! opens the host lock read-only for a moment only where
//! `/run/diskgc/lock` exists (a shared, non-blocking `flock`, released at
//! once; docs/architecture.md, The tally: Pausing).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diskgc_core::host::Host;
use diskgc_core::index::{BUDGET_BYTES, Index};
use diskgc_core::tally::TallyState;
use diskgc_core::watch::live::{LiveIndex, Shared, Threaded, with_tally_thread};
use diskgc_linux::LinuxHost;

/// The `nice` field (19th) of a `/proc/.../stat` line, counted after the
/// command's closing parenthesis (the command may hold spaces).
fn nice_of(stat: &str) -> i64 {
    let after = &stat[stat.rfind(')').expect("comm") + 2..];
    after
        .split(' ')
        .nth(16)
        .expect("nice")
        .parse()
        .expect("a number")
}

/// The nice value of every task of this process named `name`.
fn nice_of_named(name: &str) -> Vec<i64> {
    let tasks = std::fs::read_dir("/proc/self/task").expect("tasks");
    tasks
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("stat")).ok())
        .filter(|stat| stat.contains(&format!("({name})")))
        .map(|stat| nice_of(&stat))
        .collect()
}

fn nice_of_task(task: &Path) -> i64 {
    nice_of(
        &std::fs::read_to_string(PathBuf::from("/proc").join(task).join("stat"))
            .expect("a task stat"),
    )
}

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The tests look at this process's `diskgc-tally` threads: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn log(line: &str) {
    LINES.lock().unwrap().push(line.to_owned());
}

#[test]
fn the_background_tally_runs_at_idle_priority_and_the_caller_does_not() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let host: &'static LinuxHost = Box::leak(Box::new(LinuxHost::new().expect("a LinuxHost")));
    let shared = Arc::new(Shared::new(TallyState::new(Index::new(BUDGET_BYTES))));
    let caller = std::fs::read_link("/proc/thread-self").expect("thread-self");
    let before = nice_of_task(&caller);
    let (tally, during) = with_tally_thread(host, shared, Vec::new(), &log, |ended| {
        // The tally lowers its priority as it starts.
        let mut tally = nice_of_named("diskgc-tally");
        for _ in 0..200 {
            if tally == [19] {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            tally = nice_of_named("diskgc-tally");
        }
        assert!(!ended(), "the tally thread runs until the stop");
        // Where the watcher's loop, and so every reclaim, runs.
        (tally, nice_of_task(&caller))
    });
    assert_eq!(tally, [19], "one tally thread, at nice 19");
    assert_eq!(during, before, "the reclaim's thread keeps its priority");
    assert!(nice_of_named("diskgc-tally").is_empty(), "joined on return");
    let lines = LINES.lock().unwrap();
    assert!(
        !lines.iter().any(|l| l.contains("could not lower")),
        "{lines:?}"
    );
}

/// TASK-32 review (mped S1): a panic in the watcher's loop stops the tally
/// thread on the way out (the drop guard), so the process exits 101
/// instead of waiting on a thread that turns forever.
#[test]
fn a_panic_in_the_watchers_loop_stops_the_tally_thread() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let host: &'static LinuxHost = Box::leak(Box::new(LinuxHost::new().expect("a LinuxHost")));
    let shared = Arc::new(Shared::new(TallyState::new(Index::new(BUDGET_BYTES))));
    let theirs = Arc::clone(&shared);
    let started = std::time::Instant::now();
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_tally_thread(host, theirs, Vec::new(), &log, |_| -> () {
            panic!("the watcher's loop fails");
        })
    }));
    assert!(unwound.is_err());
    // Stopped and joined within the bound, not left turning.
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        Arc::strong_count(&shared),
        1,
        "the tally thread let go of the state"
    );
    assert!(
        nice_of_named("diskgc-tally").is_empty(),
        "the tally thread is gone"
    );
}

/// Codex 5 (TASK-32 review): a tally turn that does not end (blocked in the
/// kernel, holding the live index) costs the watcher at most the bound it
/// asked for: it gets no index then, never waits for the turn.
#[test]
fn a_tally_turn_that_does_not_end_never_holds_the_watcher_past_its_bound() {
    let host = LinuxHost::new().expect("a LinuxHost");
    let shared = Arc::new(Shared::new(TallyState::new(Index::new(BUDGET_BYTES))));
    let holder = Arc::clone(&shared);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let stuck = std::thread::spawn(move || {
        let _turn = holder.hold();
        held_tx.send(()).expect("send");
        std::thread::sleep(Duration::from_secs(3));
    });
    held_rx.recv().expect("the turn holds the index");
    let never = || false;
    let mut live = Threaded::new(&host, &shared, &never);
    let started = std::time::Instant::now();
    let until = host.now().checked_add(Duration::from_secs(1)).unwrap();
    let got = live.with_state(until, |state| state.is_some());
    let waited = started.elapsed();
    stuck.join().expect("the holder ends");
    assert!(!got, "no index while the turn holds it");
    assert!(
        waited >= Duration::from_millis(900) && waited < Duration::from_secs(2),
        "waited {waited:?}, not about the 1 s bound"
    );
}
