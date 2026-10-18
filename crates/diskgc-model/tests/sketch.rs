//! The host-model sketch of docs/testing.md (Model tests) as a model test
//! (task-14 AC #1), run through the executor (task-10): `ensure --min-free
//! 15G --select profile:cargo-target --roots /home/mark/src
//! /home/mark/src`. It takes the host lock, discovers the target, and the
//! reclaim protocol's first lock (`.cargo-lock`, held by Cargo) is busy:
//! skipped `locked`, exit 1 (everything processed, still below
//! `--min-free`).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::host::{Host, LockMode};
use diskgc_core::profile::Library;
use diskgc_core::units::parse_size;
use diskgc_model::harness::{Outcome, fixture_path, load_fixture, run};
use diskgc_model::spec::{Op, Skipped};
use diskgc_model::{Holder, ModelHost};

/// `ensure` with `--select profile:cargo-target`, `root` as `--roots` and
/// `PATH`: the report's exit code and skipped candidates.
fn ensure_below(host: &ModelHost, root: &Path) -> Outcome {
    let library = Library::builtin().unwrap_or_else(|e| panic!("{e}"));
    let roots = [root.display().to_string()];
    let request = Request {
        path: root.to_owned(),
        min_free: parse_size("15G").unwrap_or_else(|e| panic!("{e}")),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select("profile:cargo-target", &library, Some(&roots), 1000)
            .unwrap_or_else(|e| panic!("{e}")),
    };
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let report = executor::ensure(host, &request, uid, &mut |l| eprintln!("{l}"));
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    Outcome {
        exit: i32::from(report.exit_code()),
        skipped: report
            .skipped
            .iter()
            .map(|s| Skipped {
                candidate: s.candidate.clone(),
                reason: s.reason.to_owned(),
            })
            .collect(),
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
        ensure_below(host, &PathBuf::from("/home/mark/src"))
    });

    // The lock is cargo's (pid 4242) and still held; diskgc holds none.
    let lock = Path::new("/home/mark/src/app/target/debug/.cargo-lock");
    assert_eq!(
        host.locks_on(lock),
        vec![(Holder::Process(4242), LockMode::Exclusive)]
    );
    // Two lock attempts: the host lock (taken), then the protocol's only
    // one on the target (the profile has no other lock file), which the
    // event addressed to it followed by creating the new profile's lock
    // file; nothing was unlinked.
    let trace = host.trace();
    let flocks: Vec<_> = trace.iter().filter(|c| c.op == Op::Flock).collect();
    assert_eq!(flocks.len(), 2, "{flocks:?}");
    assert_eq!(
        flocks[0].path.as_deref(),
        Some(Path::new("/run/diskgc/lock"))
    );
    assert_eq!(flocks[0].result, "acquired");
    assert_eq!(flocks[1].path.as_deref(), Some(lock));
    assert_eq!(flocks[1].result, "busy");
    assert!(
        host.locks_on(Path::new("/run/diskgc/lock")).is_empty(),
        "released"
    );
    assert!(trace.iter().all(|c| c.op != Op::Unlinkat));
    assert!(
        host.stat_path(Path::new("/home/mark/src/app/target/release/.cargo-lock"))
            .is_some()
    );
    // The lock file was opened O_PATH below the profile directory and
    // reopened through that fd for the flock, never by name.
    let at = trace
        .iter()
        .position(|c| c.op == Op::Flock && c.path.as_deref() == Some(lock))
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
