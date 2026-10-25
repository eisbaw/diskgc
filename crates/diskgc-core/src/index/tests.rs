//! The index without the Host port: insertion short of a collapse,
//! coverage, eviction, dissolution, the counted size and queue derivation.
//! Collapse (which re-validates through the port) is tested on ModelHost in
//! diskgc-model (`tests/index.rs`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use proptest::prelude::*;

use super::queue::{AgeFilter, Queued, Source, TierQuery, derive};
use super::trie::Delta;
use super::*;
use crate::config::Order;
use crate::engine::Reason;
use crate::executor::CandidateQueue;

const DEV: Dev = Dev {
    major: 0,
    minor: 42,
};

fn ts(secs: i64) -> Timestamp {
    Timestamp { secs, nanos: 0 }
}

fn at(secs: u64) -> MonoTime {
    MonoTime(Duration::from_secs(secs))
}

fn tmp() -> MountKey {
    MountKey {
        dev: DEV,
        mount_point: PathBuf::from("/tmp"),
    }
}

fn identity(ino: u64) -> Identity {
    Identity {
        dev: DEV,
        ino,
        birth: Birth::Time(ts(i64::try_from(ino).unwrap())),
    }
}

fn found(ino: u64, change: i64) -> Discovered {
    Discovered {
        identity: identity(ino),
        newest_change: ts(change),
        checked_at: at(1),
    }
}

fn finder(index: &mut Index, profile: &str, root: &str) -> Finder {
    Finder {
        profile: profile.into(),
        profile_hash: 7,
        root: index.root(Path::new(root)).unwrap(),
    }
}

/// Inserts without a collapse (`None`: no room).
fn put(index: &mut Index, path: &str, finder: &Finder, ino: u64, change: i64) -> Option<Inserted> {
    let done = index.try_insert(&tmp(), Path::new(path), finder, found(ino, change));
    index.assert_consistent();
    done
}

fn tally(index: &mut Index, path: &str, est_bytes: u64) {
    index.single_mut(&tmp(), Path::new(path)).unwrap().tally = Some(Tally {
        est_bytes,
        files: 1,
        tallied_at: at(2),
    });
}

/// Puts `block` at `site` directly (blocks otherwise only come from a
/// collapse, which needs the Host port).
fn put_block(index: &mut Index, site: &str, block: Block) {
    let i = index.mount_index(&tmp());
    let mut delta = Delta::default();
    let mut node = &mut index.mounts[i].root;
    for name in rel_names(Path::new("/tmp"), Path::new(site)) {
        node = node.child_entry(name, &mut delta);
    }
    node.change_blocks(&mut delta, |blocks| blocks.push(block));
    index.apply(delta);
    index.assert_consistent();
}

fn block(finders: FinderSetId, family: Family, members: u64, est: u64, oldest: i64) -> Block {
    Block {
        site: identity(900),
        finders,
        family,
        members,
        est_bytes: est,
        files: members,
        oldest_change: ts(oldest),
        newest_change: ts(oldest + 100),
        tallied_at: Some(at(3)),
    }
}

const BUDGET: usize = 1 << 20;

#[test]
fn a_path_found_again_is_refreshed_replaced_or_gains_a_finder() {
    let mut index = Index::new(BUDGET);
    let rc = finder(&mut index, "nix-shell-rc", "/tmp");
    let entry = finder(&mut index, "tmp-entry", "/tmp");
    assert_eq!(put(&mut index, "/tmp/a", &rc, 10, 5), Some(Inserted::New));
    tally(&mut index, "/tmp/a", 4096);
    assert_eq!(
        put(&mut index, "/tmp/a", &entry, 10, 6),
        Some(Inserted::Refreshed)
    );
    let single = index.single(&tmp(), Path::new("/tmp/a")).unwrap().clone();
    assert_eq!(
        index.finder_set(single.finders()).len(),
        2,
        "one unit, two finders"
    );
    assert_eq!(
        single.newest_change,
        ts(5),
        "a tallied unit keeps its tallied age"
    );
    assert!(single.tally.is_some());
    assert_eq!(
        put(&mut index, "/tmp/a", &rc, 11, 7),
        Some(Inserted::Replaced)
    );
    let single = index.single(&tmp(), Path::new("/tmp/a")).unwrap();
    assert_eq!((single.identity, single.tally), (identity(11), None));
    assert_eq!(index.finder_set(single.finders()), [rc]);
    assert_eq!(index.units(), 1);
}

#[test]
fn a_candidate_a_block_stands_for_is_covered_and_nothing_else_is() {
    let mut index = Index::new(BUDGET);
    let rc = finder(&mut index, "nix-shell-rc", "/tmp");
    let other = finder(&mut index, "tmp-entry", "/tmp");
    let rc_set = index.intern_set(vec![rc.clone()]);
    let family = Family::Name(name_family(b"nix-shell.aaaaaa"));
    put_block(&mut index, "/tmp", block(rc_set, family, 3, 30, 1));
    assert_eq!(
        put(&mut index, "/tmp/nix-shell.bbbbbb", &rc, 20, 1),
        Some(Inserted::Covered)
    );
    // Another family, another finder, or deeper than a family block reaches:
    assert_eq!(
        put(&mut index, "/tmp/pytest-of-u", &rc, 21, 1),
        Some(Inserted::New)
    );
    assert_eq!(
        put(&mut index, "/tmp/nix-shell.cccccc", &other, 22, 1),
        Some(Inserted::New)
    );
    assert_eq!(
        put(&mut index, "/tmp/d/nix-shell.dddddd", &rc, 23, 1),
        Some(Inserted::New)
    );
    // An `any` block covers every depth below its site, not the site.
    put_block(&mut index, "/tmp/d", block(rc_set, Family::Any, 2, 30, 1));
    assert_eq!(
        put(&mut index, "/tmp/d/e/f", &rc, 24, 1),
        Some(Inserted::Covered)
    );
    assert_eq!(
        index.blocks_at(&tmp(), Path::new("/tmp"))[0].members,
        3,
        "covered: not counted"
    );
}

#[test]
fn an_insertion_that_would_cross_the_ceiling_needs_a_collapse() {
    let mut index = Index::new(64 * 1024);
    let rc = finder(&mut index, "nix-shell-rc", "/tmp");
    let mut inserted = 0;
    for ino in 0..10_000 {
        match put(&mut index, &format!("/tmp/nix-shell.{ino:06}"), &rc, ino, 1) {
            Some(Inserted::New) => inserted += 1,
            None => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(inserted > 10 && inserted < 10_000, "{inserted}");
    assert!(index.accounted_bytes() <= index.budget_bytes());
}

#[test]
fn eviction_prunes_and_block_members_count_down() {
    let mut index = Index::new(BUDGET);
    let rc = finder(&mut index, "nix-shell-rc", "/tmp");
    put(&mut index, "/tmp/a/b/c", &rc, 1, 1);
    assert!(index.evict_single(&tmp(), Path::new("/tmp/a/b/c")));
    assert!(!index.evict_single(&tmp(), Path::new("/tmp/a/b/c")));
    index.assert_consistent();
    assert_eq!(
        (index.units(), index.trie_bytes),
        (0, 0),
        "empty nodes pruned"
    );

    let set = index.intern_set(vec![rc.clone()]);
    put_block(&mut index, "/tmp/s", block(set, Family::Any, 3, 300, 1));
    let removed = |index: &mut Index| {
        let outcome =
            index.block_member_removed(&tmp(), Path::new("/tmp/s"), set, &Family::Any, 100, 1);
        index.assert_consistent();
        outcome
    };
    assert_eq!(removed(&mut index), MemberRemoved::Remaining(2));
    assert_eq!(
        index.blocks_at(&tmp(), Path::new("/tmp/s"))[0].est_bytes,
        200
    );
    assert_eq!(removed(&mut index), MemberRemoved::Evicted);
    assert_eq!(removed(&mut index), MemberRemoved::Missing);
    assert_eq!(index.units(), 0);
}

#[test]
fn a_block_dissolves_only_when_its_members_fit_under_the_low_water_mark() {
    let mut index = Index::new(64 * 1024);
    let rc = finder(&mut index, "nix-shell-rc", "/tmp");
    let set = index.intern_set(vec![rc.clone()]);
    let family = Family::Name(name_family(b"nix-shell.aaaaaa"));
    put_block(&mut index, "/tmp", block(set, family.clone(), 400, 4000, 1));
    let members = |n: u64| -> Vec<(PathBuf, Discovered)> {
        (0..n)
            .map(|i| {
                (
                    PathBuf::from(format!("/tmp/nix-shell.abcd{i:04}")),
                    found(100 + i, 1),
                )
            })
            .collect()
    };
    let site = Path::new("/tmp");
    assert_eq!(
        index.dissolve(&tmp(), site, set, &family, &members(400)),
        Dissolved::Kept
    );
    index.assert_consistent();
    assert_eq!(
        index.dissolve(&tmp(), site, set, &family, &members(20)),
        Dissolved::Done
    );
    index.assert_consistent();
    assert!(index.blocks_at(&tmp(), site).is_empty());
    assert_eq!(index.stats().singles, 20);
    assert_eq!(
        index.dissolve(&tmp(), site, set, &family, &members(2)),
        Dissolved::Missing
    );
}

// ---- Queue derivation ----

fn sources<'a>(root: &'a Path, hash: u64) -> [Source<'a>; 1] {
    [Source {
        profile: "nix-shell-rc",
        profile_hash: hash,
        root,
    }]
}

fn tier<'a>(sources: &'a [Source<'a>], age: AgeFilter) -> TierQuery<'a> {
    TierQuery {
        dev: DEV,
        sources,
        age,
    }
}

fn paths(entries: &[queue::QueueEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| e.path.display().to_string())
        .collect()
}

#[test]
fn oldest_order_with_blocks_by_their_oldest_member_and_hints_last() {
    let mut index = Index::new(BUDGET);
    let rc = finder(&mut index, "nix-shell-rc", "/tmp");
    let other = finder(&mut index, "tmp-entry", "/tmp");
    put(&mut index, "/tmp/c", &rc, 1, 30);
    put(&mut index, "/tmp/a", &rc, 2, 10);
    put(&mut index, "/tmp/b", &rc, 3, 10);
    put(&mut index, "/tmp/x", &other, 4, 1);
    put(&mut index, "/tmp/hinted", &rc, 5, 0);
    index
        .single_mut(&tmp(), Path::new("/tmp/hinted"))
        .unwrap()
        .refusal = Some(Refusal {
        reason: Reason::TooManyEntries,
        at: at(1),
    });
    let set = index.intern_set(vec![rc.clone()]);
    put_block(&mut index, "/tmp/d", block(set, Family::Any, 5, 50, 20));
    let root = Path::new("/tmp");
    let src = sources(root, 7);
    let queue = derive(&index, &tier(&src, AgeFilter::None));
    assert_eq!(
        paths(&queue.ready),
        ["/tmp/a", "/tmp/b", "/tmp/d", "/tmp/c"]
    );
    assert_eq!(paths(&queue.hinted), ["/tmp/hinted"]);
    assert_eq!(
        derive(&index, &tier(&src, AgeFilter::None)),
        queue,
        "deterministic"
    );

    // A positive idle filter keeps the block by its oldest member; a
    // negated one by its newest.
    let idle = derive(&index, &tier(&src, AgeFilter::IdleSince(ts(20))));
    assert_eq!(paths(&idle.ready), ["/tmp/a", "/tmp/b", "/tmp/d"]);
    let changed = derive(&index, &tier(&src, AgeFilter::ChangedSince(ts(100))));
    assert_eq!(paths(&changed.ready), ["/tmp/d"]);

    // Another profile hash or bound root, or another filesystem: nothing.
    assert!(
        derive(&index, &tier(&sources(root, 8), AgeFilter::None))
            .ready
            .is_empty()
    );
    assert!(
        derive(
            &index,
            &tier(&sources(Path::new("/tmp/d"), 7), AgeFilter::None)
        )
        .ready
        .is_empty()
    );
    let elsewhere = TierQuery {
        dev: Dev { major: 8, minor: 1 },
        ..tier(&src, AgeFilter::None)
    };
    assert!(derive(&index, &elsewhere).ready.is_empty());
}

#[test]
fn best_fit_takes_the_smallest_covering_unit_else_the_largest_and_blocks_after_singles() {
    let mut index = Index::new(BUDGET);
    let rc = finder(&mut index, "nix-shell-rc", "/tmp");
    for (ino, path, est) in [
        (1, "/tmp/s100", 100),
        (2, "/tmp/s400", 400),
        (3, "/tmp/s900", 900),
    ] {
        put(&mut index, path, &rc, ino, i64::try_from(ino).unwrap());
        tally(&mut index, path, est);
    }
    put(&mut index, "/tmp/unknown", &rc, 4, 0);
    let set = index.intern_set(vec![rc.clone()]);
    // Older than every single: first in the input order, so only its
    // deletion count ranks it after the single of the same size.
    put_block(&mut index, "/tmp/blk", block(set, Family::Any, 4, 400, -1));
    let root = Path::new("/tmp");
    let src = sources(root, 7);
    let ready = derive(&index, &tier(&src, AgeFilter::None)).ready;
    let mut queue = CandidateQueue::new(Order::BestFit, ready, queue::QueueEntry::estimate);
    let mut picks = Vec::new();
    for need in [300, 300, 2000, 1, 1] {
        picks.push(queue.pop(need).unwrap().path.display().to_string());
    }
    assert_eq!(
        picks,
        [
            "/tmp/s400",
            "/tmp/blk",
            "/tmp/s900",
            "/tmp/s100",
            "/tmp/unknown"
        ]
    );
    assert!(queue.is_empty());
}

// ---- Properties ----

/// An operation on a small tree: names from a tiny alphabet so paths
/// collide, nest and share parents.
#[derive(Debug, Clone)]
enum Op {
    Insert {
        path: Vec<u8>,
        profile: bool,
        ino: u64,
        change: i64,
    },
    Evict {
        path: Vec<u8>,
    },
    Tally {
        path: Vec<u8>,
        est: u64,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let path = prop::collection::vec(0u8..4, 1..4);
    prop_oneof![
        4 => (path.clone(), any::<bool>(), 1u64..40, 0i64..50)
            .prop_map(|(path, profile, ino, change)| Op::Insert { path, profile, ino, change }),
        1 => path.clone().prop_map(|path| Op::Evict { path }),
        1 => (path, 0u64..1000).prop_map(|(path, est)| Op::Tally { path, est }),
    ]
}

fn path_of(names: &[u8]) -> String {
    let mut path = String::from("/tmp");
    for n in names {
        path.push_str(["/a", "/bb", "/c.12345", "/c.67890"][usize::from(*n)]);
    }
    path
}

fn run(ops: &[Op], budget: usize) -> Index {
    let mut index = Index::new(budget);
    let rc = finder(&mut index, "nix-shell-rc", "/tmp");
    let entry = finder(&mut index, "tmp-entry", "/tmp");
    for op in ops {
        match op {
            Op::Insert {
                path,
                profile,
                ino,
                change,
            } => {
                let f = if *profile { &rc } else { &entry };
                let path = path_of(path);
                let _ = index.try_insert(&tmp(), Path::new(&path), f, found(*ino, *change));
            }
            Op::Evict { path } => {
                index.evict_single(&tmp(), Path::new(&path_of(path)));
            }
            Op::Tally { path, est } => {
                if let Some(s) = index.single_mut(&tmp(), Path::new(&path_of(path))) {
                    s.tally = Some(Tally {
                        est_bytes: *est,
                        files: 1,
                        tallied_at: at(2),
                    });
                }
            }
        }
        index.assert_consistent();
        assert!(index.accounted_bytes() <= index.budget_bytes());
    }
    index
}

proptest! {
    /// The incremental count equals a count from scratch after every
    /// change, and the ceiling holds without a collapse (an insertion that
    /// does not fit is refused).
    #[test]
    fn the_count_stays_exact_and_under_the_budget(
        ops in prop::collection::vec(op(), 0..60),
        budget in 16usize * 1024..64 * 1024,
    ) {
        run(&ops, budget);
    }

    /// The queue is a function of the index's content: deterministic, in
    /// the documented order, whatever order the units were inserted in.
    #[test]
    fn the_queue_is_deterministic_and_in_documented_order(
        ops in prop::collection::vec(op(), 0..60),
    ) {
        let index = run(&ops, BUDGET);
        let root = Path::new("/tmp");
        let src = sources(root, 7);
        let queue = derive(&index, &tier(&src, AgeFilter::None));
        prop_assert_eq!(&derive(&index, &tier(&src, AgeFilter::None)), &queue);
        for list in [&queue.ready, &queue.hinted] {
            for pair in list.windows(2) {
                prop_assert!(
                    (pair[0].order_key(), &pair[0].path) <= (pair[1].order_key(), &pair[1].path),
                    "out of order: {:?}", pair
                );
            }
        }
        let mut every = 0;
        index.for_each_unit(|_, _, unit| {
            if let UnitRef::Single(s) = unit
                && index.finder_set(s.finders()).iter().any(|f| &*f.profile == "nix-shell-rc")
            {
                every += 1;
            }
        });
        prop_assert_eq!(queue.ready.len() + queue.hinted.len(), every);

        // The same units inserted in reverse order give the same queue.
        let mut reversed = Index::new(BUDGET);
        let rc = finder(&mut reversed, "nix-shell-rc", "/tmp");
        for entry in queue.ready.iter().rev() {
            let Queued::Single(s) = &entry.unit else { unreachable!("no blocks here") };
            let found = Discovered { identity: s.identity, newest_change: s.newest_change, checked_at: s.checked_at };
            reversed.try_insert(&tmp(), &entry.path, &rc, found).expect("room");
            *reversed.single_mut(&tmp(), &entry.path).unwrap() = Single { finders: reversed.single(&tmp(), &entry.path).unwrap().finders, ..s.clone() };
        }
        let again = derive(&reversed, &tier(&src, AgeFilter::None));
        let strip = |q: &[queue::QueueEntry]| -> Vec<(PathBuf, Timestamp)> {
            q.iter().map(|e| (e.path.clone(), e.order_key())).collect()
        };
        prop_assert_eq!(strip(&again.ready), strip(&queue.ready));
    }
}
