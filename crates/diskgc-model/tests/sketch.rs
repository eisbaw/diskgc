//! The host-model sketch of docs/testing.md (Model tests) as a model test
//! (task-14 AC #1).
//!
//! The executor does not exist yet (task-10, task-9, task-53), so the
//! scenario is a stand-in for the Cargo lock check, written against the
//! port with the core's real path resolution: reach the target, open the
//! profile's `.cargo-lock` below it and try an exclusive lock without
//! waiting. When the executor lands, this fixture runs through it instead.

mod common;

use std::path::Path;

use diskgc_core::host::{Access, Host, LockMode, TryLock};
use diskgc_core::resolve::{Want, open_beneath, resolve};
use diskgc_model::harness::{Outcome, fixture_path, load_fixture, run};
use diskgc_model::spec::{Op, Skipped};
use diskgc_model::{Holder, ModelHost};

/// Stand-in: a target is `locked` (exit 1) if any profile's `.cargo-lock`
/// is held by someone else.
fn cargo_lock_probe(host: &ModelHost, target: &Path, profiles: &[&str]) -> Outcome {
    let root = resolve(host, target, Want::Directory).expect("target resolves");
    for profile in profiles {
        let (dir, _) = open_beneath(
            host,
            &root.fd,
            root.mount.id,
            Path::new(profile),
            Access::Path,
            Want::Directory,
        )
        .expect("profile dir opens");
        let (lock, _) = open_beneath(
            host,
            &dir,
            root.mount.id,
            Path::new(".cargo-lock"),
            Access::Read,
            Want::Any,
        )
        .expect("lock file opens");
        match host.flock_try(lock, LockMode::Exclusive).expect("flock") {
            TryLock::Busy(_) => {
                return Outcome {
                    exit: 1,
                    skipped: vec![Skipped {
                        candidate: target.to_owned(),
                        reason: "locked".to_owned(),
                    }],
                };
            }
            TryLock::Acquired(locked) => drop(locked),
        }
    }
    Outcome {
        exit: 0,
        skipped: Vec::new(),
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
    let target = Path::new("/home/mark/src/app/target");
    // `run` checks expect (exit 1, skipped locked) and that the event fired.
    let host = run(&model, |host| cargo_lock_probe(host, target, &["debug"]));

    // The lock is cargo's (pid 4242) and still held.
    let lock = Path::new("/home/mark/src/app/target/debug/.cargo-lock");
    assert_eq!(
        host.locks_on(lock),
        vec![(Holder::Process(4242), LockMode::Exclusive)]
    );
    // The event addressed to the first flock on that path created the new
    // profile's lock file right after the call.
    let trace = host.trace();
    let flock = trace
        .iter()
        .find(|c| c.op == Op::Flock)
        .expect("a flock call");
    assert_eq!(flock.path.as_deref(), Some(lock));
    assert_eq!(flock.result, "busy");
    assert_eq!(flock.index as usize, trace.len(), "flock was the last call");
    assert!(
        host.stat_path(Path::new("/home/mark/src/app/target/release/.cargo-lock"))
            .is_some()
    );
    // The resolution protocol on the trace: mountinfo, the mount point from
    // the root, then one step at a time below it, each confirmed by statx.
    let ops: Vec<Op> = trace.iter().map(|c| c.op).collect();
    assert_eq!(
        ops,
        [
            Op::Mountinfo,
            Op::Openat2,
            Op::Statx,
            Op::Openat2,
            Op::Statx,
            Op::Openat2,
            Op::Statx,
            Op::Openat2,
            Op::Statx,
            Op::Flock,
        ]
    );
    // Available space is derived: 900G - 880G - the 8G file and the
    // directories (one 4 KiB block each; /home is the mount's root).
    let space = host.space_at(Path::new("/home"));
    let dirs = ["/home", "mark", "src", "app", "target", "debug", "release"].len() as u64;
    let files = 8 << 30;
    assert_eq!(space.avail_bytes, (20u64 << 30) - files - dirs * 4096);
    // Cargo reads like a live process on LinuxHost: nothing empty (these
    // reads come after the trace assertions, as they are calls too).
    let cargo = diskgc_core::host::Pid(4242);
    assert_eq!(host.proc_fds(cargo).expect("fds").len(), 1, "its lock fd");
    assert!(host.proc_maps(cargo).is_ok() && host.proc_environ(cargo).is_ok());
}
