//! TASK23: named safety scenarios that combine a changing model filesystem
//! with the production walk, delete, and idle predicate.
//!
//! These deletions affect only ModelHost's in-memory inode graph.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::engine::{
    Candidate, DeleteOptions, End, Reason, Rules, WalkStop, delete_unchecked, walk,
};
use diskgc_core::host::Host;
use diskgc_core::profile::{IdleFor, Idleness};
use diskgc_model::spec::Op;

const OLD: &str = "2026-10-01T00:00:00Z";

fn fixture(extra: &str) -> diskgc_model::ModelHost {
    common::host(&format!(
        "host_model: 1\nclock: 2026-10-15T03:00:00Z\nmounts:\n  - \
         {{id: 1, at: /, fs: ext4, dev: '254:1', size: 100G}}\n  - \
         {{id: 30, at: /tmp, fs: tmpfs, dev: '0:42', size: 1G, parent: 1}}\n\
         tree:\n  /tmp/c: {{.: {{mtime: {OLD}}}, a: {{mtime: {OLD}}}, b: {{mtime: 1970-01-01T00:00:01Z}}}}\n\
         {extra}"
    ))
}

fn unlinked(host: &diskgc_model::ModelHost) -> Vec<PathBuf> {
    host.trace()
        .into_iter()
        .filter(|call| call.op == Op::Unlinkat && call.result == "ok")
        .filter_map(|call| call.path)
        .collect()
}

#[test]
fn replacing_an_entry_after_walk_keeps_the_replacement() {
    let host = fixture(
        "events:\n  - {at: {op: unlinkat, path: /tmp/c/a, when: after}, replace: /tmp/c/b}\n",
    );
    let parent = common::dir(&host, "/tmp");
    let candidate = Candidate::new(&parent, Path::new("/tmp/c")).unwrap();
    let record = walk(&host, &candidate, &Rules::new(&[1000]), None).unwrap();
    let original = host.stat_path(Path::new("/tmp/c/b")).unwrap().dev_ino();

    let deletion = delete_unchecked(&host, &candidate, &record, DeleteOptions::default());
    assert!(
        matches!(deletion.end, End::Changed { ref path, .. } if path == Path::new("/tmp/c/b")),
        "{deletion:?}"
    );
    let replacement = host.stat_path(Path::new("/tmp/c/b")).unwrap();
    assert_ne!(replacement.dev_ino(), original);
    assert_eq!(unlinked(&host), [PathBuf::from("/tmp/c/a")]);
    assert!(host.unfired_events().is_empty());
}

#[test]
fn a_file_with_old_mtime_and_fresh_ctime_is_not_idle() {
    // The file's mtime is Unix second 1, as with a file copied from a Nix
    // store. `chown` changes ctime alone. Reading mtime alone would falsely
    // say the file is old enough for idle-3d.
    let host = fixture(
        "events:\n  - {at: {op: statx, path: /tmp/c/b, when: before}, chown: {path: /tmp/c/b, uid: 1000}}\n",
    );
    let parent = common::dir(&host, "/tmp");
    let candidate = Candidate::new(&parent, Path::new("/tmp/c")).unwrap();
    let record = walk(&host, &candidate, &Rules::new(&[1000]), None).unwrap();
    let file = host.stat_path(Path::new("/tmp/c/b")).unwrap();
    let idle = IdleFor(Duration::from_secs(3 * 24 * 3600));
    assert_eq!(file.mtime.unwrap().secs, 1);

    assert_eq!(
        idle.verdict(file.mtime, host.wall_clock()),
        Idleness::Idle,
        "the mtime-only counterfactual must falsely accept this file"
    );
    assert_eq!(
        idle.verdict(file.ctime, host.wall_clock()),
        Idleness::NotIdle
    );
    assert_eq!(
        idle.verdict(record.newest_change(), host.wall_clock()),
        Idleness::NotIdle,
        "the production walk must see the fresh ctime"
    );
    assert!(host.unfired_events().is_empty());
}

#[test]
fn interrupted_delete_makes_the_remaining_directory_not_idle() {
    let host =
        fixture("events:\n  - {at: {op: unlinkat, path: /tmp/c/a, when: after}, signal: term}\n");
    let parent = common::dir(&host, "/tmp");
    let candidate = Candidate::new(&parent, Path::new("/tmp/c")).unwrap();
    let record = walk(&host, &candidate, &Rules::new(&[1000]), None).unwrap();
    let idle = IdleFor(Duration::from_secs(3 * 24 * 3600));
    assert_eq!(
        idle.verdict(record.newest_change(), host.wall_clock()),
        Idleness::Idle,
        "the untouched candidate initially passes idle-3d"
    );
    let deletion = delete_unchecked(&host, &candidate, &record, DeleteOptions::default());

    assert_eq!(unlinked(&host), [PathBuf::from("/tmp/c/a")]);
    assert!(host.stat_path(Path::new("/tmp/c/b")).is_some());
    assert!(
        matches!(deletion.end, End::Failed { ref error, .. } if error.detail.contains("interrupted")),
        "{deletion:?}"
    );
    let remaining = host.stat_path(Path::new("/tmp/c")).unwrap();
    assert_eq!(
        idle.verdict(remaining.mtime, host.wall_clock()),
        Idleness::NotIdle,
        "an idle filter on a later run must see the unlink's fresh directory mtime"
    );
    assert!(host.unfired_events().is_empty());
}

#[test]
fn mixed_owner_directory_is_refused_before_any_delete() {
    let host = fixture("  /tmp/c/other: {uid: 1001}\n");
    let parent = common::dir(&host, "/tmp");
    let candidate = Candidate::new(&parent, Path::new("/tmp/c")).unwrap();
    let result = walk(&host, &candidate, &Rules::new(&[1000]), None);
    assert!(
        matches!(result, Err(WalkStop::Skipped(ref skip)) if skip.reason == Reason::NotOwner && skip.path == Path::new("/tmp/c/other")),
        "{result:?}"
    );
    assert!(unlinked(&host).is_empty());
    // This counterfactual proves the refusal depends on the owner allowlist.
    let accepted = walk(&host, &candidate, &Rules::new(&[1000, 1001]), None);
    assert!(accepted.is_ok(), "{accepted:?}");
}
