//! The index (docs/architecture.md, The index): what the source profiles
//! found, per filesystem, with what the tally learned about it. A cache that
//! orders work, never an authority: nothing here decides that anything may
//! be deleted, and every unit is reached and judged again at reclaim
//! (docs/safety.md, Discovery).
//!
//! Composed bottom-up:
//!
//! - [`unit`]: the data, [`Single`] and [`Block`], their [`Identity`] and
//!   [`Finder`]s, and the block arithmetic;
//! - [`family`]: name families, which a collapse groups by first;
//! - [`memory`]: how the index counts its memory against its budget;
//! - `trie`: the per-mount path trie and its counted changes;
//! - this module: [`Index`], the tries plus the root and finder-set tables,
//!   insertion, eviction, dissolution, updates and reading;
//! - [`collapse`]: making room at the ceiling, re-validating through the
//!   [`Host`] port before anything is absorbed;
//! - [`queue`]: a tier's queue, a pure function of the index.
//!
//! **Budget.** The counted size ([`memory`]) of the tries and of the root,
//! finder-set and mount tables never exceeds the budget. An insertion that
//! would cross it first collapses ([`collapse`]) until the index is at or
//! below the low-water mark, then inserts; if collapsing cannot make room
//! (everything is already one block per finder set and root) the insertion
//! is refused with [`Inserted::NoRoom`] and the candidate is left to the
//! reclaim walk's per-tier discovery. Registering a root ([`Index::root`])
//! takes no Host port and so cannot collapse: at the ceiling it is refused
//! ([`TablesFull`]) until a collapse made room.
//!
//! **Coverage.** A candidate found beneath a block that stands for it (one
//! of the block's finders found it; for a family block, it lies directly
//! beneath the site and its name is in the family) is not inserted: it is
//! [`Inserted::Covered`], and the block is left as it is (its member count
//! is not raised: a block stores no names, so a candidate it already
//! counts cannot be told from a new one). A path that already holds a
//! single gains the finder instead, whatever blocks lie above it.
//!
//! Not here yet: the tally that fills the index (TASK-29, which also adds
//! its live cursors to the count), the snapshot and dropping finders whose
//! profile hash changed (TASK-30), the reclaim walk's use of it (TASK-31).

pub mod collapse;
pub mod family;
pub mod memory;
pub mod queue;
mod trie;
pub mod unit;

use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

pub use collapse::CollapseReport;
pub use family::name_family;
pub use memory::BUDGET_BYTES;
pub use unit::{
    Birth, Block, Family, Finder, FinderSetId, Identity, Refusal, RootId, Single, Tally,
};

use crate::host::{Dev, Host, HostError, MonoTime, MountEntry, Timestamp};
use memory::{heap, low_water, vec_bytes};
use trie::{Delta, Node, children_map_bytes, name_bytes, new_child_bytes, single_bytes};

/// A mount the index has a trie for: the filesystem's device and where it
/// is mounted. Not the mount id, which is per boot.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MountKey {
    pub dev: Dev,
    pub mount_point: PathBuf,
}

impl MountKey {
    pub fn of(mount: &MountEntry) -> Self {
        Self {
            dev: mount.dev,
            mount_point: mount.mount_point.clone(),
        }
    }
}

/// A root a finder names, and when it was last listed completely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub path: PathBuf,
    pub discovered_at: Option<MonoTime>,
}

/// A candidate as discovery reports it to the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Discovered {
    pub identity: Identity,
    /// `max(mtime, ctime)` of its root: the provisional `newest_change`.
    pub newest_change: Timestamp,
    pub checked_at: MonoTime,
}

/// What [`Index::insert`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// A new single.
    New,
    /// The path held a single of the same identity: it gained the finder
    /// and the check time.
    Refreshed,
    /// The path held a single of another identity, which was evicted.
    Replaced,
    /// A block stands for it (see the module doc, Coverage).
    Covered,
    /// No room even after collapsing, or the tables are full.
    NoRoom,
}

/// What [`Index::insert`] did, and the collapse it needed first, if any
/// (in `watch` its `statx` calls are tally work inside the duty cycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insertion {
    pub outcome: Inserted,
    pub collapse: Option<CollapseReport>,
}

/// What [`Index::dissolve`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dissolved {
    /// The block was replaced by its listed members.
    Done,
    /// The members do not fit under the low-water mark; the block stays.
    Kept,
    /// No such block.
    Missing,
}

/// What [`Index::block_member_removed`] left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRemoved {
    Remaining(u64),
    /// That was the last member: the block was evicted.
    Evicted,
    Missing,
}

/// A unit as [`Index::for_each_unit`] shows it.
#[derive(Debug, Clone, Copy)]
pub enum UnitRef<'a> {
    Single(&'a Single),
    Block(&'a Block),
}

/// Counts for reports and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub singles: usize,
    pub blocks: usize,
    /// Members of all blocks.
    pub members: u64,
    pub accounted_bytes: usize,
    pub budget_bytes: usize,
}

/// A new root does not fit: collapse ([`Index::collapse_to_low_water`])
/// and register it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TablesFull;

#[derive(Debug)]
struct MountTrie {
    key: MountKey,
    root: Node,
}

/// The index: per-mount tries of units, the root and finder-set tables, and
/// the counted size of all of it.
#[derive(Debug)]
pub struct Index {
    budget: usize,
    trie_bytes: usize,
    tables_bytes: usize,
    units: usize,
    mounts: Vec<MountTrie>,
    roots: Vec<Root>,
    finder_sets: Vec<Box<[Finder]>>,
}

fn signed(bytes: usize) -> isize {
    isize::try_from(bytes).expect("index bytes fit isize")
}

fn apply(total: &mut usize, change: isize) {
    *total = total
        .checked_add_signed(change)
        .expect("index count went negative: a delta was lost");
}

/// The names of `path` below `mount_point`. Programmer errors (a path
/// outside the mount, not absolute, with `.` or `..`) panic.
fn rel_names<'p>(mount_point: &Path, path: &'p Path) -> Vec<&'p [u8]> {
    assert!(
        path.is_absolute(),
        "index path {} is not absolute",
        path.display()
    );
    let rel = path.strip_prefix(mount_point).unwrap_or_else(|_| {
        panic!(
            "index path {} is not below its mount {}",
            path.display(),
            mount_point.display()
        )
    });
    rel.components()
        .map(|c| match c {
            Component::Normal(name) => name.as_bytes(),
            other => panic!("index path {} holds {other:?}", path.display()),
        })
        .collect()
}

/// Bytes of a finder set's slice and profile names.
fn finder_set_bytes(set: &[Finder]) -> usize {
    vec_bytes(set.len(), size_of::<Finder>())
        + set.iter().map(|f| heap(f.profile.len())).sum::<usize>()
}

/// Whether `block` (of the finder set `set`) stands for a candidate found by
/// `finder` that lies `depth_below` levels beneath the block's site and is
/// named `last`.
fn block_covers(
    block: &Block,
    set: &[Finder],
    finder: &Finder,
    depth_below: usize,
    last: &[u8],
) -> bool {
    set.contains(finder)
        && match &block.family {
            Family::Any => depth_below >= 1,
            Family::Name(family) => depth_below == 1 && *family == name_family(last),
        }
}

impl Index {
    /// An empty index with a budget of `budget` bytes ([`BUDGET_BYTES`]
    /// outside tests).
    pub fn new(budget: usize) -> Self {
        assert!(
            budget >= 1024,
            "index budget {budget} too small to hold its tables"
        );
        Self {
            budget,
            trie_bytes: 0,
            tables_bytes: 0,
            units: 0,
            mounts: Vec::new(),
            roots: Vec::new(),
            finder_sets: Vec::new(),
        }
    }

    pub fn budget_bytes(&self) -> usize {
        self.budget
    }

    /// The counted size (see [`memory`]).
    pub fn accounted_bytes(&self) -> usize {
        self.trie_bytes + self.tables_bytes
    }

    /// Singles plus blocks.
    pub fn units(&self) -> usize {
        self.units
    }

    /// Whether `cost` more bytes keep the index within its budget.
    fn has_room(&self, cost: usize) -> bool {
        self.accounted_bytes() + cost <= self.budget
    }

    /// Whether the index is at or below its low-water mark.
    pub fn at_low_water(&self) -> bool {
        self.accounted_bytes() <= low_water(self.budget)
    }

    fn apply(&mut self, delta: Delta) {
        apply(&mut self.trie_bytes, delta.bytes);
        apply(&mut self.units, delta.units);
        assert!(
            self.accounted_bytes() <= self.budget,
            "index over its budget: {} > {}",
            self.accounted_bytes(),
            self.budget
        );
    }

    // ---- Tables ----

    fn tables_cost(&self) -> usize {
        vec_bytes(self.mounts.capacity(), size_of::<MountTrie>())
            + self
                .mounts
                .iter()
                .map(|m| heap(m.key.mount_point.capacity()))
                .sum::<usize>()
            + vec_bytes(self.roots.capacity(), size_of::<Root>())
            + self
                .roots
                .iter()
                .map(|r| heap(r.path.capacity()))
                .sum::<usize>()
            + vec_bytes(self.finder_sets.capacity(), size_of::<Box<[Finder]>>())
            + self
                .finder_sets
                .iter()
                .map(|s| finder_set_bytes(s))
                .sum::<usize>()
    }

    /// What pushing an entry owning `extra` heap bytes onto `table` costs
    /// (the table grows by exactly one: [`push_exact`]).
    fn push_cost<T>(table: &Vec<T>, extra: usize) -> usize {
        let grow = if table.len() == table.capacity() {
            vec_bytes(table.len() + 1, size_of::<T>()) - vec_bytes(table.capacity(), size_of::<T>())
        } else {
            0
        };
        grow + extra
    }

    /// The id of the root at `path` (absolute), registered if new.
    pub fn root(&mut self, path: &Path) -> Result<RootId, TablesFull> {
        assert!(
            path.is_absolute(),
            "root {} is not absolute",
            path.display()
        );
        let i = match self.roots.iter().position(|r| r.path == path) {
            Some(i) => i,
            None => {
                let path = path.to_path_buf();
                let cost = Self::push_cost(&self.roots, heap(path.capacity()));
                if !self.has_room(cost) {
                    return Err(TablesFull);
                }
                self.tables_bytes += cost;
                push_exact(
                    &mut self.roots,
                    Root {
                        path,
                        discovered_at: None,
                    },
                )
            }
        };
        Ok(RootId(u32::try_from(i).expect("roots fit u32")))
    }

    pub fn root_info(&self, id: RootId) -> &Root {
        &self.roots[id.0 as usize]
    }

    /// Records the time of a complete discovery listing of a root.
    pub fn set_discovered(&mut self, id: RootId, at: MonoTime) {
        self.roots[id.0 as usize].discovered_at = Some(at);
    }

    pub fn finder_set(&self, id: FinderSetId) -> &[Finder] {
        &self.finder_sets[id.0 as usize]
    }

    /// The set `base` plus `finder` (sorted, no duplicates).
    fn set_with(&self, base: Option<FinderSetId>, finder: &Finder) -> Vec<Finder> {
        let mut set: Vec<Finder> = base.map_or_else(Vec::new, |id| self.finder_set(id).to_vec());
        if let Err(at) = set.binary_search(finder) {
            set.insert(at, finder.clone());
        }
        set
    }

    fn find_set(&self, set: &[Finder]) -> Option<FinderSetId> {
        let i = self.finder_sets.iter().position(|s| **s == *set)?;
        Some(FinderSetId(u32::try_from(i).expect("sets fit u32")))
    }

    /// What interning `set` costs (nothing if it is known).
    fn set_cost(&self, set: &[Finder]) -> usize {
        match self.find_set(set) {
            Some(_) => 0,
            None => Self::push_cost(&self.finder_sets, finder_set_bytes(set)),
        }
    }

    /// The id of `set`, registered if new; the caller checked the room
    /// ([`Self::set_cost`]).
    fn intern_set(&mut self, set: Vec<Finder>) -> FinderSetId {
        assert!(
            set.windows(2).all(|w| w[0] < w[1]),
            "finder set sorted, unique"
        );
        assert!(!set.is_empty(), "a unit has at least one finder");
        for finder in &set {
            assert!(
                (finder.root.0 as usize) < self.roots.len(),
                "finder names an unknown root"
            );
        }
        if let Some(id) = self.find_set(&set) {
            return id;
        }
        let cost = self.set_cost(&set);
        assert!(self.has_room(cost), "finder set interned without room");
        self.tables_bytes += cost;
        let i = push_exact(&mut self.finder_sets, set.into_boxed_slice());
        FinderSetId(u32::try_from(i).expect("sets fit u32"))
    }

    /// What registering `key` costs (nothing if it is known).
    fn mount_cost(&self, key: &MountKey) -> usize {
        match self.find_mount(key) {
            Some(_) => 0,
            None => Self::push_cost(&self.mounts, heap(key.mount_point.capacity())),
        }
    }

    /// The trie of `key`, registered if new; the caller checked the room
    /// ([`Self::mount_cost`]).
    fn mount_index(&mut self, key: &MountKey) -> usize {
        if let Some(i) = self.find_mount(key) {
            return i;
        }
        let key = key.clone();
        let cost = Self::push_cost(&self.mounts, heap(key.mount_point.capacity()));
        assert!(self.has_room(cost), "mount registered without room");
        self.tables_bytes += cost;
        push_exact(
            &mut self.mounts,
            MountTrie {
                key,
                root: Node::default(),
            },
        )
    }

    fn find_mount(&self, key: &MountKey) -> Option<usize> {
        self.mounts.iter().position(|m| m.key == *key)
    }

    // ---- Reading ----

    fn node(&self, mount: &MountKey, path: &Path) -> Option<&Node> {
        let trie = &self.mounts[self.find_mount(mount)?];
        let mut node = &trie.root;
        for name in rel_names(&mount.mount_point, path) {
            node = node.children.get(name)?;
        }
        Some(node)
    }

    fn node_mut(&mut self, mount: &MountKey, path: &Path) -> Option<&mut Node> {
        let i = self.find_mount(mount)?;
        let mut node = &mut self.mounts[i].root;
        for name in rel_names(&mount.mount_point, path) {
            node = node.children.get_mut(name)?;
        }
        Some(node)
    }

    pub fn single(&self, mount: &MountKey, path: &Path) -> Option<&Single> {
        self.node(mount, path)?.single.as_deref()
    }

    /// The single at `path`, to record a tally, check or refusal. Its size
    /// in memory does not change with its fields.
    pub fn single_mut(&mut self, mount: &MountKey, path: &Path) -> Option<&mut Single> {
        self.node_mut(mount, path)?.single.as_deref_mut()
    }

    /// The blocks sited at the directory `path`.
    pub fn blocks_at(&self, mount: &MountKey, path: &Path) -> &[Block] {
        self.node(mount, path).map_or(&[], |n| &n.blocks)
    }

    /// Calls `f` for every unit, mounts in their order of registration,
    /// paths in trie order (a node's single and blocks before its children).
    pub fn for_each_unit(&self, mut f: impl FnMut(&MountKey, &Path, UnitRef<'_>)) {
        fn walk(
            key: &MountKey,
            path: &mut PathBuf,
            node: &Node,
            f: &mut impl FnMut(&MountKey, &Path, UnitRef<'_>),
        ) {
            if let Some(single) = &node.single {
                f(key, path, UnitRef::Single(single));
            }
            for block in &node.blocks {
                f(key, path, UnitRef::Block(block));
            }
            for (name, child) in &node.children {
                path.push(std::ffi::OsStr::from_bytes(name));
                walk(key, path, child, f);
                path.pop();
            }
        }
        for trie in &self.mounts {
            let mut path = trie.key.mount_point.clone();
            walk(&trie.key, &mut path, &trie.root, &mut f);
        }
    }

    pub fn stats(&self) -> Stats {
        let mut stats = Stats {
            accounted_bytes: self.accounted_bytes(),
            budget_bytes: self.budget,
            ..Stats::default()
        };
        self.for_each_unit(|_, _, unit| match unit {
            UnitRef::Single(_) => stats.singles += 1,
            UnitRef::Block(b) => {
                stats.blocks += 1;
                stats.members += b.members;
            }
        });
        stats
    }

    /// Checks the counted size and unit count against a count from scratch,
    /// and the trie's shape (no empty node, exact block vectors, sites
    /// within their finders' roots). For tests: linear in the index.
    pub fn assert_consistent(&self) {
        let mut bytes = 0;
        let mut units = 0;
        for trie in &self.mounts {
            let (b, u) = trie.root.subtree_cost();
            bytes += b;
            units += u;
            assert_no_empty_nodes(&trie.root);
        }
        assert_eq!(bytes, self.trie_bytes, "incremental trie bytes drifted");
        assert_eq!(units, self.units, "incremental unit count drifted");
        assert_eq!(self.tables_cost(), self.tables_bytes, "table bytes drifted");
        assert!(
            self.accounted_bytes() <= self.budget,
            "index over its budget"
        );
        self.for_each_unit(|_, path, unit| {
            if let UnitRef::Block(block) = unit {
                assert!(
                    block.members >= 2,
                    "a block of fewer than two members at {}",
                    path.display()
                );
                for finder in self.finder_set(block.finders) {
                    assert!(
                        path.starts_with(&self.root_info(finder.root).path),
                        "block site {} above its root",
                        path.display()
                    );
                }
            }
        });
    }

    // ---- Insertion ----

    /// Records the candidate at `path` (absolute, on `mount`) found by
    /// `finder`, collapsing first if it would not fit (see the module doc).
    /// An error is a failed `mountinfo` read during a collapse.
    pub fn insert<H: Host>(
        &mut self,
        host: &H,
        mount: &MountKey,
        path: &Path,
        finder: &Finder,
        found: Discovered,
    ) -> Result<Insertion, HostError> {
        assert_eq!(
            found.identity.dev, mount.dev,
            "a unit on another device than its mount"
        );
        let root = &self.root_info(finder.root).path;
        assert!(
            path.starts_with(root),
            "{} is not below its finder's root {}",
            path.display(),
            root.display()
        );
        if let Some(outcome) = self.try_insert(mount, path, finder, found) {
            return Ok(Insertion {
                outcome,
                collapse: None,
            });
        }
        let collapse = collapse::to_low_water(self, host)?;
        let outcome = self
            .try_insert(mount, path, finder, found)
            .unwrap_or(Inserted::NoRoom);
        Ok(Insertion {
            outcome,
            collapse: Some(collapse),
        })
    }

    /// One attempt: `None` if there is no room for it.
    fn try_insert(
        &mut self,
        mount: &MountKey,
        path: &Path,
        finder: &Finder,
        found: Discovered,
    ) -> Option<Inserted> {
        let names = rel_names(&mount.mount_point, path);
        if let Some(existing) = self.single(mount, path).cloned() {
            let refreshed = existing.identity == found.identity;
            let set = self.set_with(refreshed.then_some(existing.finders), finder);
            if !self.has_room(self.set_cost(&set)) {
                return None;
            }
            let set = self.intern_set(set);
            let single = self.single_mut(mount, path).expect("just read");
            if refreshed {
                single.finders = set;
                single.checked_at = found.checked_at;
                if single.tally.is_none() {
                    single.newest_change = found.newest_change;
                }
                return Some(Inserted::Refreshed);
            }
            *single = Single {
                identity: found.identity,
                finders: set,
                newest_change: found.newest_change,
                tally: None,
                checked_at: found.checked_at,
                refusal: None,
            };
            return Some(Inserted::Replaced);
        }
        let Some(trie_cost) = self.new_single_cost(mount, &names, finder) else {
            return Some(Inserted::Covered);
        };
        let set = self.set_with(None, finder);
        if !self.has_room(trie_cost + self.set_cost(&set) + self.mount_cost(mount)) {
            return None;
        }
        let set = self.intern_set(set);
        let i = self.mount_index(mount);
        let mut delta = Delta::default();
        let mut node = &mut self.mounts[i].root;
        for name in &names {
            node = node.child_entry(name, &mut delta);
        }
        node.set_single(
            Single {
                identity: found.identity,
                finders: set,
                newest_change: found.newest_change,
                tally: None,
                checked_at: found.checked_at,
                refusal: None,
            },
            &mut delta,
        );
        assert_eq!(delta.bytes, signed(trie_cost), "insert cost mispredicted");
        self.apply(delta);
        Some(Inserted::New)
    }

    /// The trie bytes a new single at `names` costs, or `None` if a block
    /// above it covers it.
    fn new_single_cost(&self, mount: &MountKey, names: &[&[u8]], finder: &Finder) -> Option<usize> {
        let mut cost = single_bytes();
        let Some(i) = self.find_mount(mount) else {
            return Some(cost + names.iter().map(|n| new_child_bytes(0, n)).sum::<usize>());
        };
        let last = names.last().copied().unwrap_or(b"");
        let mut node = Some(&self.mounts[i].root);
        for (depth, name) in names.iter().enumerate() {
            match node {
                Some(n) => {
                    let below = names.len() - depth;
                    let covered = n
                        .blocks
                        .iter()
                        .any(|b| block_covers(b, self.finder_set(b.finders), finder, below, last));
                    if covered {
                        return None;
                    }
                    if !n.children.contains_key(*name) {
                        cost += new_child_bytes(n.children.len(), name);
                    }
                    node = n.children.get(*name);
                }
                None => cost += new_child_bytes(0, name),
            }
        }
        Some(cost)
    }

    // ---- Eviction and updates ----

    /// Applies `edit` to the node at `path` (if there is one), then prunes
    /// the nodes left empty on the way up. Returns whether it was there.
    fn edit_at(
        &mut self,
        mount: &MountKey,
        path: &Path,
        edit: impl FnOnce(&mut Node, &mut Delta),
    ) -> bool {
        let Some(i) = self.find_mount(mount) else {
            return false;
        };
        let names = rel_names(&mount.mount_point, path);
        let mut delta = Delta::default();
        let found = edit_path(&mut self.mounts[i].root, &names, &mut delta, edit);
        self.apply(delta);
        found
    }

    /// Evicts the single at `path`. Returns whether there was one.
    pub fn evict_single(&mut self, mount: &MountKey, path: &Path) -> bool {
        let mut evicted = false;
        self.edit_at(mount, path, |node, delta| {
            evicted = node.take_single(delta).is_some();
        });
        evicted
    }

    /// Evicts the block of `finders` and `family` sited at `site`.
    pub fn evict_block(
        &mut self,
        mount: &MountKey,
        site: &Path,
        finders: FinderSetId,
        family: &Family,
    ) -> bool {
        let mut evicted = false;
        self.edit_at(mount, site, |node, delta| {
            node.change_blocks(delta, |blocks| {
                let before = blocks.len();
                blocks.retain(|b| !(b.finders == finders && b.family == *family));
                evicted = blocks.len() < before;
            });
        });
        evicted
    }

    /// A member of an expanded block was deleted (docs/architecture.md,
    /// Index update after an action): subtracts its measured size and one
    /// member; the block goes with its last member.
    pub fn block_member_removed(
        &mut self,
        mount: &MountKey,
        site: &Path,
        finders: FinderSetId,
        family: &Family,
        measured_bytes: u64,
        files: u64,
    ) -> MemberRemoved {
        let mut outcome = MemberRemoved::Missing;
        self.edit_at(mount, site, |node, delta| {
            node.change_blocks(delta, |blocks| {
                let Some(at) = blocks
                    .iter()
                    .position(|b| b.finders == finders && b.family == *family)
                else {
                    return;
                };
                let block = &mut blocks[at];
                block.members -= 1;
                block.est_bytes = block.est_bytes.saturating_sub(measured_bytes);
                block.files = block.files.saturating_sub(files);
                if block.members < 2 {
                    // A block of one member cannot be told from its site:
                    // the member is left to discovery.
                    blocks.remove(at);
                    outcome = MemberRemoved::Evicted;
                } else {
                    outcome = MemberRemoved::Remaining(blocks[at].members);
                }
            });
        });
        outcome
    }

    /// Replaces a block by the members a complete discovery listing of its
    /// root found beneath its site, if they fit under the low-water mark
    /// (docs/architecture.md, No re-splitting in place). `members` must be
    /// what the block stands for: beneath the site, and for a family block
    /// directly beneath it with a name in the family.
    pub fn dissolve(
        &mut self,
        mount: &MountKey,
        site: &Path,
        finders: FinderSetId,
        family: &Family,
        members: &[(PathBuf, Discovered)],
    ) -> Dissolved {
        let Some(node) = self.node(mount, site) else {
            return Dissolved::Missing;
        };
        let Some(at) = node
            .blocks
            .iter()
            .position(|b| b.finders == finders && b.family == *family)
        else {
            return Dissolved::Missing;
        };
        let site_names = rel_names(&mount.mount_point, site);
        for (path, _) in members {
            let names = rel_names(&mount.mount_point, path);
            assert!(
                names.len() > site_names.len() && names.starts_with(&site_names),
                "member {} not beneath site {}",
                path.display(),
                site.display()
            );
            if let Family::Name(f) = family {
                assert!(
                    names.len() == site_names.len() + 1
                        && **f == *name_family(names[names.len() - 1]),
                    "member {} not in the block's family",
                    path.display()
                );
            }
        }
        let freed = node.remove_block_bytes(at);
        let cost = self.batch_cost(mount, members);
        let after = (self.accounted_bytes() + cost).saturating_sub(freed);
        if !self.at_low_water() || after > low_water(self.budget) {
            return Dissolved::Kept;
        }
        // The members join with the block's own finders; a path already
        // holding a single keeps it (and its finders).
        let Some(i) = self.find_mount(mount) else {
            return Dissolved::Missing;
        };
        let mut delta = Delta::default();
        for (path, found) in members {
            let names = rel_names(&mount.mount_point, path);
            let mut node = &mut self.mounts[i].root;
            for name in &names {
                node = node.child_entry(name, &mut delta);
            }
            if node.single.is_none() {
                node.set_single(
                    Single {
                        identity: found.identity,
                        finders,
                        newest_change: found.newest_change,
                        tally: None,
                        checked_at: found.checked_at,
                        refusal: None,
                    },
                    &mut delta,
                );
            }
        }
        self.apply(delta);
        assert!(
            self.evict_block(mount, site, finders, family),
            "block vanished while dissolving"
        );
        Dissolved::Done
    }

    /// An upper bound of the trie bytes `members` add as new singles.
    fn batch_cost(&self, mount: &MountKey, members: &[(PathBuf, Discovered)]) -> usize {
        // Per parent directory: entries it has now and the new names.
        let mut grow: NewNames<'_> = BTreeMap::new();
        let mut singles = 0;
        let i = self.find_mount(mount);
        for (path, _) in members {
            let names = rel_names(&mount.mount_point, path);
            let mut node = i.map(|i| &self.mounts[i].root);
            for (depth, name) in names.iter().enumerate() {
                let next = node.and_then(|n| n.children.get(*name));
                if next.is_none() {
                    let parent = names[..depth].to_vec();
                    let len = node.map_or(0, |n| n.children.len());
                    grow.entry(parent)
                        .or_insert((len, BTreeSet::new()))
                        .1
                        .insert(name);
                }
                node = next;
            }
            if node.is_none_or(|n| n.single.is_none()) {
                singles += 1;
            }
        }
        let maps: usize = grow
            .values()
            .map(|(len, new)| {
                children_map_bytes(len + new.len()) - children_map_bytes(*len)
                    + new.iter().map(|n| name_bytes(n)).sum::<usize>()
            })
            .sum();
        maps + singles * single_bytes()
    }
}

/// Per parent directory (its names below the mount): the entries it has
/// now and the new names a batch adds.
type NewNames<'a> = BTreeMap<Vec<&'a [u8]>, (usize, BTreeSet<&'a [u8]>)>;

/// Pushes `entry`, growing `table` by exactly one slot (so that its counted
/// capacity is what [`Index::push_cost`] predicted). Returns its index.
fn push_exact<T>(table: &mut Vec<T>, entry: T) -> usize {
    table.reserve_exact(1);
    table.push(entry);
    table.len() - 1
}

/// Walks to `names` below `node`, applies `edit` there, and prunes empty
/// nodes on the way back. Returns whether the path existed.
fn edit_path(
    node: &mut Node,
    names: &[&[u8]],
    delta: &mut Delta,
    edit: impl FnOnce(&mut Node, &mut Delta),
) -> bool {
    let Some((first, rest)) = names.split_first() else {
        edit(node, delta);
        return true;
    };
    let Some(child) = node.children.get_mut(*first) else {
        return false;
    };
    let found = edit_path(child, rest, delta, edit);
    node.prune_child(first, delta);
    found
}

fn assert_no_empty_nodes(node: &Node) {
    for (name, child) in &node.children {
        assert!(
            !child.is_empty(),
            "empty trie node {:?}",
            String::from_utf8_lossy(name)
        );
        assert_eq!(
            child.blocks.capacity(),
            child.blocks.len(),
            "blocks not exact"
        );
        assert_no_empty_nodes(child);
    }
}

#[cfg(test)]
mod tests;
