//! The host-model sketch of docs/testing.md (Model tests) as a model test
//! (task-14 AC #1), run through the real Cargo reclaim protocol (task-53,
//! diskgc-core `cargo_target::reclaim`): discovery below the root, then
//! the protocol, whose first lock (`.cargo-lock`, held by Cargo) is busy.
//!
//! The exit code is the executor's to decide (task-10): until it exists,
//! this test maps a skipped target to exit 1 (nothing freed) and a
//! reclaimed one to 0, which is all the fixture's `expect` asks.

mod common;

use std::path::{Path, PathBuf};

use diskgc_core::cargo_target::{ReclaimEnd, discover, reclaim};
use diskgc_core::engine::{DEFAULT_MAX_ENTRIES, Rules};
use diskgc_core::host::{Host, LockMode};
use diskgc_core::inuse::{Checker, REUSE_LIMIT};
use diskgc_model::harness::{Outcome, fixture_path, load_fixture, run};
use diskgc_model::spec::{Op, Skipped};
use diskgc_model::{Holder, ModelHost};

/// Discovery below `root` and the reclaim protocol on every target found:
/// a skipped target is listed with its reason (exit 1), and an error of
/// the run fails the test.
fn reclaim_below(host: &ModelHost, root: &Path) -> Outcome {
    let found = discover(host, &[root.to_owned()], None).unwrap_or_else(|e| panic!("{e}"));
    assert!(found.refused.is_empty(), "{:?}", found.refused);
    let owners = [1000];
    let rules = Rules {
        owners: &owners,
        block_ipc: true,
        max_entries: DEFAULT_MAX_ENTRIES,
    };
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let mut skipped = Vec::new();
    for target in &found.targets {
        match reclaim(host, target, None, &rules, None, &mut checker, None).end {
            ReclaimEnd::Complete => {}
            ReclaimEnd::Skipped(skip) => skipped.push(Skipped {
                candidate: target.path.clone(),
                reason: skip.reason.as_str().to_owned(),
            }),
            other => panic!("{}: {other:?}", target.path.display()),
        }
    }
    Outcome {
        exit: i32::from(!skipped.is_empty()),
        skipped,
    }
}

#[test]
fn the_testing_md_sketch_loads_and_runs() {
    let model = load_fixture(&fixture_path(
        env!("CARGO_MANIFEST_DIR"),
        "sketch",
        "locked-cargo-target",
    ))
    .unwrap_or_else(|e| panic!("{e}"));
    // `run` checks expect (exit 1, skipped locked) and that the event fired.
    let host = run(&model, |host| {
        reclaim_below(host, &PathBuf::from("/home/mark/src"))
    });

    // The lock is cargo's (pid 4242) and still held; diskgc holds none.
    let lock = Path::new("/home/mark/src/app/target/debug/.cargo-lock");
    assert_eq!(
        host.locks_on(lock),
        vec![(Holder::Process(4242), LockMode::Exclusive)]
    );
    // The protocol's only lock attempt was that one (the profile has no
    // other lock file), the event addressed to it created the new
    // profile's lock file right after it, and nothing was unlinked.
    let trace = host.trace();
    let flocks: Vec<_> = trace.iter().filter(|c| c.op == Op::Flock).collect();
    assert_eq!(flocks.len(), 1, "{flocks:?}");
    assert_eq!(flocks[0].path.as_deref(), Some(lock));
    assert_eq!(flocks[0].result, "busy");
    assert!(trace.iter().all(|c| c.op != Op::Unlinkat));
    assert!(
        host.stat_path(Path::new("/home/mark/src/app/target/release/.cargo-lock"))
            .is_some()
    );
    // The lock file was opened O_PATH below the profile directory and
    // reopened through that fd for the flock, never by name.
    let at = trace
        .iter()
        .position(|c| c.op == Op::Flock)
        .expect("the flock");
    assert_eq!(trace[at - 1].op, Op::ReopenRegular);
    // Available space is derived: 900G - 880G - the 8G file and the
    // directories (one 4 KiB block each; /home is the mount's root).
    let space = host.space_at(Path::new("/home"));
    let dirs = [
        "/home", "mark", "src", "app", "target", "debug", "deps", "release",
    ]
    .len() as u64;
    let files = 8 << 30;
    assert_eq!(space.avail_bytes, (20u64 << 30) - files - dirs * 4096);
    // Cargo reads like a live process on LinuxHost: nothing empty (these
    // reads come after the trace assertions, as they are calls too).
    let cargo = diskgc_core::host::Pid(4242);
    assert_eq!(host.proc_fds(cargo).expect("fds").len(), 1, "its lock fd");
    assert!(host.proc_maps(cargo).is_ok() && host.proc_environ(cargo).is_ok());
}
