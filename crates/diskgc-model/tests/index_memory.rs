//! The index's counted size against the allocator (diskgc-core
//! `index::memory`): the heap an index owns, plus the counted overhead per
//! allocation, never exceeds what it counts. Measured by a counting global
//! allocator as what dropping the index frees, on this thread only (the
//! counters are thread-local, so other tests and the model do not mix in).
//!
//! One test per binary would also do; the thread-local counters make the
//! tests here independent of each other.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use diskgc_core::host::{Dev, MonoTime, Timestamp};
use diskgc_core::index::memory::ALLOC_OVERHEAD_BYTES;
use diskgc_core::index::{Birth, Discovered, Finder, Identity, Index, Inserted, MountKey, Tally};
use diskgc_model::ModelHost;

struct Counting;

thread_local! {
    static LIVE_BYTES: Cell<isize> = const { Cell::new(0) };
    static LIVE_ALLOCS: Cell<isize> = const { Cell::new(0) };
}

fn count(bytes: isize, allocs: isize) {
    // `try_with`: the allocator also runs while thread-locals are torn down.
    let _ = LIVE_BYTES.try_with(|c| c.set(c.get() + bytes));
    let _ = LIVE_ALLOCS.try_with(|c| c.set(c.get() + allocs));
}

fn size(layout: Layout) -> isize {
    isize::try_from(layout.size()).expect("fits")
}

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged and only adds bookkeeping in thread-local cells.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded as is.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            count(size(layout), 1);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded as is.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            count(size(layout), 1);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded as is.
        unsafe { System.dealloc(ptr, layout) };
        count(-size(layout), -1);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded as is.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            count(isize::try_from(new_size).expect("fits") - size(layout), 0);
        }
        p
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn live() -> (isize, isize) {
    (LIVE_BYTES.with(Cell::get), LIVE_ALLOCS.with(Cell::get))
}

/// Drops `index` and returns what it owned: heap bytes and allocations.
fn owned(index: Index) -> (usize, usize) {
    let before = live();
    drop(index);
    let after = live();
    let bytes = usize::try_from(before.0 - after.0).expect("dropping frees");
    let allocs = usize::try_from(before.1 - after.1).expect("dropping frees");
    (bytes, allocs)
}

/// Asserts the bound and returns a line for the report.
fn check(name: &str, index: Index) -> String {
    let counted = index.accounted_bytes();
    let stats = index.stats();
    let (bytes, allocs) = owned(index);
    let with_overhead = bytes + allocs * ALLOC_OVERHEAD_BYTES;
    assert!(
        with_overhead <= counted,
        "{name}: owns {bytes} B in {allocs} allocations ({with_overhead} B with overhead), \
         counts only {counted} B"
    );
    let units = (stats.singles + stats.blocks).max(1);
    format!(
        "{name}: {} singles, {} blocks; counted {counted} B ({} B/unit); allocated {bytes} B in \
         {allocs} allocations, {with_overhead} B with overhead ({} B/unit, {:.0}% of counted)",
        stats.singles,
        stats.blocks,
        counted / units,
        with_overhead / units,
        100.0 * with_overhead as f64 / counted as f64
    )
}

const DEV: Dev = Dev {
    major: 0,
    minor: 42,
};

fn tmp() -> MountKey {
    MountKey {
        dev: DEV,
        mount_point: PathBuf::from("/tmp"),
    }
}

fn found(ino: u64) -> Discovered {
    Discovered {
        identity: Identity {
            dev: DEV,
            ino,
            birth: Birth::Time(Timestamp { secs: 1, nanos: 0 }),
        },
        newest_change: Timestamp { secs: 1, nanos: 0 },
        checked_at: MonoTime(std::time::Duration::ZERO),
    }
}

fn empty_host() -> ModelHost {
    ModelHost::from_yaml(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}
  - {id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 100G, parent: 1}
"#,
    )
    .unwrap()
}

/// A small deterministic generator (no randomness crate in this binary).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.0 >> 33) % below
    }
}

fn index_with(host: &ModelHost, paths: impl IntoIterator<Item = PathBuf>) -> Index {
    let mut index = Index::new(1 << 30);
    let rc = Finder {
        profile: "nix-shell-rc".into(),
        profile_hash: 1,
        root: index.root(Path::new("/tmp")).unwrap(),
    };
    let other = Finder {
        profile: "tmp-entry".into(),
        ..rc.clone()
    };
    for (ino, path) in paths.into_iter().enumerate() {
        let ino = u64::try_from(ino).unwrap() + 100;
        let finder = if ino % 3 == 0 { &other } else { &rc };
        let done = index
            .insert(host, &tmp(), &path, finder, found(ino))
            .unwrap();
        assert_ne!(done.outcome, Inserted::NoRoom);
    }
    index
}

#[test]
fn the_count_bounds_the_heap_of_wide_deep_and_churned_indexes() {
    let host = empty_host();
    let mut report = String::new();

    let wide = (0..20_000).map(|i| PathBuf::from(format!("/tmp/nix-shell.{i:06}ab")));
    writeln!(report, "{}", check("wide", index_with(&host, wide))).unwrap();

    let deep =
        (0..10_000).map(|i| PathBuf::from(format!("/tmp/u{}/build-{}/target-{i}", i % 50, i % 7)));
    writeln!(report, "{}", check("deep", index_with(&host, deep))).unwrap();

    // Churn: random inserts and evictions over a small namespace, so maps
    // shrink and regrow (the B-tree's minimum-fill rule on removal).
    let mut rng = Lcg(42);
    let mut index = index_with(&host, std::iter::empty());
    let rc = Finder {
        profile: "nix-shell-rc".into(),
        profile_hash: 1,
        root: index.root(Path::new("/tmp")).unwrap(),
    };
    for step in 0..60_000u64 {
        let path = PathBuf::from(format!("/tmp/d{}/e{}", rng.next(40), rng.next(400)));
        if rng.next(3) == 0 {
            index.evict_single(&tmp(), &path);
        } else {
            index
                .insert(&host, &tmp(), &path, &rc, found(step))
                .unwrap();
            if let Some(s) = index.single_mut(&tmp(), &path) {
                s.tally = Some(Tally {
                    est_bytes: step,
                    files: 1,
                    tallied_at: MonoTime(std::time::Duration::ZERO),
                });
            }
        }
    }
    index.assert_consistent();
    writeln!(report, "{}", check("churned", index)).unwrap();
    eprint!("{report}");
}

/// After a collapse: blocks, families and pruned nodes.
#[test]
fn the_count_bounds_the_heap_after_collapses() {
    let mut yaml = String::from(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}
  - {id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 100G, parent: 1}
tree:
"#,
    );
    let mut paths = Vec::new();
    for i in 0..3_000 {
        let path = format!("/tmp/s{}/nix-shell.{:05}ab", i % 30, i);
        writeln!(yaml, "  {path}: {{rc: {{size: 1K}}}}").unwrap();
        paths.push(PathBuf::from(path));
    }
    let host = ModelHost::from_yaml(&yaml).unwrap();
    let mut index = Index::new(200_000);
    let rc = Finder {
        profile: "nix-shell-rc".into(),
        profile_hash: 1,
        root: index.root(Path::new("/tmp")).unwrap(),
    };
    let mut collapses = 0;
    for path in &paths {
        let stat = host.stat_path(path).unwrap();
        let parent = host.stat_path(path.parent().unwrap()).unwrap();
        let found = Discovered {
            identity: Identity::of(&stat, parent.ino.unwrap()).unwrap(),
            newest_change: stat.mtime.unwrap(),
            checked_at: MonoTime(std::time::Duration::ZERO),
        };
        let done = index.insert(&host, &tmp(), path, &rc, found).unwrap();
        collapses += usize::from(done.collapse.is_some());
    }
    index.assert_consistent();
    assert!(
        collapses > 0 && index.stats().blocks > 0,
        "the fixture must collapse"
    );
    eprintln!("{} ({collapses} collapse runs)", check("collapsed", index));
}
