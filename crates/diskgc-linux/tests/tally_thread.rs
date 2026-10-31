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
use std::sync::Mutex;
use std::time::Duration;

use diskgc_core::index::{BUDGET_BYTES, Index};
use diskgc_core::tally::TallyState;
use diskgc_core::watch::live::{Shared, with_tally_thread};
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

#[test]
fn the_background_tally_runs_at_idle_priority_and_the_caller_does_not() {
    let host = LinuxHost::new().expect("a LinuxHost");
    let shared = Shared::new(TallyState::new(Index::new(BUDGET_BYTES)));
    let caller = std::fs::read_link("/proc/thread-self").expect("thread-self");
    let before = nice_of_task(&caller);
    let lines = Mutex::new(Vec::new());
    let log = |line: &str| lines.lock().unwrap().push(line.to_owned());
    let (tally, during) = with_tally_thread(&host, &shared, &[], &log, |ended| {
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
    let lines = lines.into_inner().unwrap();
    assert!(
        !lines.iter().any(|l| l.contains("could not lower")),
        "{lines:?}"
    );
}
