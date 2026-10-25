//! Collapse at the ceiling (docs/architecture.md, Memory budget and
//! collapse): replace units beneath a site directory by one [`Block`],
//! until the index is at or below its low-water mark.
//!
//! Three kinds of step, at a site `D`:
//!
//! - **family** (finder set `F`): the singles of `F` of one name family
//!   directly beneath `D`, with that family's block of `F` at `D`;
//! - **all names** (finder set `F`): every unit of `F` beneath `D`, at any
//!   depth;
//! - **lump** (every finder set): every unit beneath `D` into one block of
//!   family `any` whose finders are the union of theirs. Lumps are planned
//!   only once family and all-names steps make no more progress (everything
//!   is one block per finder set and root). A lump's site may lie above
//!   some of its finders' roots: at reclaim each tier expands it by its own
//!   discovery from the deeper of the site and its bound root. At the
//!   mount's root a lump may take a single unit (to replace a deep path by
//!   one block), so the limit is one lump per mount.
//!
//! One step:
//!
//! 1. **Plan** (pure): the candidate sites are the directories with at
//!    least two units of a step's kind beneath them (family and all-names
//!    steps: of one finder set, the site not above any of the set's roots).
//!    They are taken in the order of the key: sites whose units are all
//!    known to be below [`BIG_UNIT_BYTES`] first, then the deepest, then the
//!    path, then family before all names before lump (ties: finder set,
//!    family). A pass plans at most [`PLAN_LIMIT`] steps (a bounded working
//!    set): the first ones in that order **that would shrink the index**
//!    now (the gain of step 4, counted at planning). A step that would not
//!    shrink it never takes a place in the window, so a crowd of such steps
//!    (long family names over singles whose nodes stay) cannot fill it and
//!    stop the run above the low-water mark while a shrinking step exists
//!    (TASK-230). The next pass plans again from what is left.
//! 2. **Reach `D`** (Host): from its mount's root without following a
//!    symlink or crossing a mount ([`crate::resolve`]), after screening the
//!    mount chain from mountinfo (read again every pass). Gone,
//!    unreachable, on another mount, or of another identity than a single
//!    recorded at `D`: every unit at and beneath `D` is evicted and nothing
//!    is collapsed there. A block sited at `D` with another site identity
//!    (made before `D` was replaced) is evicted, whatever its finders.
//! 3. **Re-validate** (Host): every unit the step would absorb is reached
//!    again beneath `D` (a single by `statx` of its name in its parent, a
//!    finer block's site by opening it) and must have its recorded
//!    identity. A name that is a mount point now is never touched. A unit
//!    that fails (gone, replaced, not permitted) is **evicted** before
//!    anything is absorbed, so it contributes nothing to the block. A
//!    failure of the host itself (EMFILE, ENOMEM, EIO) stops the run with
//!    that error and evicts nothing more.
//! 4. **Absorb** (pure): the step is taken only if it shrinks the index,
//!    counted exactly (what the taken units and their emptied nodes free,
//!    against the new block and, for a lump, its union finder set: charged
//!    unless that set already exists and units outside the step keep it,
//!    and the sets only the step's units use credited); a long family name
//!    over singles whose nodes stay could grow it. The units become one
//!    block at `D` whose totals are the sums of theirs. The same check runs
//!    before any I/O, so a hopeless step costs no `statx`.
//!
//! The finder-set and mount tables are swept after every pass.
//!
//! A collapse never makes a check weaker: a block's members are found and
//! judged afresh when the reclaim walk expands it. Re-validation keeps the
//! block's estimates honest; it grants nothing.
//!
//! Termination: every executed step strictly shrinks the index, and a pass
//! that changes neither the units nor the counted size moves on to lumps
//! or, with lumps, ends the run (asserted).

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::memory::ALLOC_OVERHEAD_BYTES;
use super::trie::{
    Delta, Name, Node, children_map_bytes, exact_blocks_bytes, name_bytes, single_bytes,
};
use super::unit::{Block, Family, FinderSetId, Identity, Single};
use super::{Index, MountKey, finder_set_bytes, name_family};
use crate::host::{Access, Base, Errno, Host, HostError, MountEntry};
use crate::resolve::{ResolveError, Want, mount_for, open_beneath, resolve_in, screen_mount_chain};

/// Units estimated at or above this (64 MiB) keep their exact places
/// longest: they matter most to freeing space and to `best-fit`.
pub const BIG_UNIT_BYTES: u64 = 64 * 1024 * 1024;

/// Steps planned per pass: bounds the planning's working set, while a
/// pass over a 300,000-unit index stays cheap relative to its `statx`es.
pub const PLAN_LIMIT: usize = 1024;

/// What a run of collapses did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CollapseReport {
    pub passes: usize,
    /// Blocks made or grown.
    pub collapses: usize,
    /// Of those, lumps (across finder sets).
    pub lumps: usize,
    /// Units absorbed into blocks.
    pub absorbed: usize,
    /// Units evicted because their re-validation failed or their site was
    /// gone.
    pub evicted: usize,
    /// Whether the index ended at or below its low-water mark (otherwise
    /// nothing was left to collapse: one lump per mount).
    pub reached_low_water: bool,
}

/// Which units a step takes beneath its site.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    /// Singles of the step's set directly beneath, of this name family,
    /// and that family's block of the set at the site.
    Family(Name),
    /// Every unit of the step's set beneath the site.
    Any,
    /// Every unit beneath the site, of any set.
    Lump,
}

/// What a step takes: its scope and, but for a lump, its finder set.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Takes {
    scope: Scope,
    set: Option<FinderSetId>,
}

impl Takes {
    fn of_set(&self, set: FinderSetId) -> bool {
        self.set.is_none_or(|s| s == set)
    }

    fn deep(&self) -> bool {
        !matches!(self.scope, Scope::Family(_))
    }

    /// A single named `name`, `direct`ly beneath the site or deeper.
    fn single(&self, name: &[u8], single: &Single, direct: bool) -> bool {
        self.of_set(single.finders)
            && match &self.scope {
                Scope::Family(family) => direct && name_family(name) == *family,
                Scope::Any | Scope::Lump => true,
            }
    }

    /// A block sited at the step's site.
    fn block_at_site(&self, block: &Block) -> bool {
        self.of_set(block.finders)
            && match &self.scope {
                Scope::Family(family) => block.family == Family::Name(family.clone()),
                Scope::Any | Scope::Lump => true,
            }
    }

    /// A block sited strictly beneath the step's site.
    fn block_below(&self, block: &Block) -> bool {
        self.deep() && self.of_set(block.finders)
    }

    fn family(&self) -> Family {
        match &self.scope {
            Scope::Family(family) => Family::Name(family.clone()),
            Scope::Any | Scope::Lump => Family::Any,
        }
    }
}

/// A planned step.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    /// Whether some unit is not known to be small.
    big: bool,
    depth: usize,
    mount: usize,
    path: PathBuf,
    takes: Takes,
    /// The finder set's place in content order, for a tie-break that
    /// does not depend on the order sets were registered in (0 for lumps).
    set_rank: usize,
}

impl Step {
    fn key(&self) -> (bool, Reverse<usize>, &Path, &Scope, usize) {
        (
            self.big,
            Reverse(self.depth),
            &self.path,
            &self.takes.scope,
            self.set_rank,
        )
    }
}

impl Ord for Step {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key()
            .cmp(&other.key())
            .then(self.mount.cmp(&other.mount))
    }
}

impl PartialOrd for Step {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Units: how many, and whether all are known small.
#[derive(Debug, Clone, Copy)]
struct Agg {
    count: usize,
    small: bool,
}

impl Agg {
    const NONE: Agg = Agg {
        count: 0,
        small: true,
    };

    fn one(small: bool) -> Self {
        Self { count: 1, small }
    }

    fn merge(&mut self, other: Agg) {
        self.count += other.count;
        self.small &= other.small;
    }
}

fn add_to<K: Ord>(map: &mut BTreeMap<K, Agg>, key: K, agg: Agg) {
    map.entry(key).and_modify(|a| a.merge(agg)).or_insert(agg);
}

impl Index {
    /// Collapses until the index is at or below its low-water mark, or
    /// nothing is left to collapse (one lump per mount). In `watch` the
    /// tally calls it as work of its own; [`Index::insert`] calls it at
    /// the ceiling.
    pub fn collapse_to_low_water<H: Host>(
        &mut self,
        host: &H,
    ) -> Result<CollapseReport, HostError> {
        to_target(self, host, super::memory::low_water(self.budget_bytes()))
    }

    /// Collapses until the counted size is at or below `target_bytes`, or
    /// nothing is left to collapse. `0` collapses to the limit: one lump per
    /// mount.
    pub fn collapse_to<H: Host>(
        &mut self,
        host: &H,
        target_bytes: usize,
    ) -> Result<CollapseReport, HostError> {
        to_target(self, host, target_bytes)
    }

    fn site_within_roots(&self, set: FinderSetId, site: &Path) -> bool {
        self.finder_set(set)
            .iter()
            .all(|f| site.starts_with(&f.root))
    }

    fn set_ranks(&self) -> BTreeMap<FinderSetId, usize> {
        let mut live: Vec<(FinderSetId, &[super::Finder])> = self
            .finder_sets
            .iter()
            .map(|slot| (slot.id, &*slot.finders))
            .collect();
        live.sort_by(|a, b| a.1.cmp(b.1));
        live.into_iter()
            .enumerate()
            .map(|(rank, (id, _))| (id, rank))
            .collect()
    }
}

pub(super) fn to_target<H: Host>(
    index: &mut Index,
    host: &H,
    target: usize,
) -> Result<CollapseReport, HostError> {
    let mut report = CollapseReport::default();
    let done = |index: &Index| index.accounted_bytes() <= target;
    if done(index) {
        report.reached_low_water = index.at_low_water();
        return Ok(report);
    }
    // Every pass but the switch to lumps and the last shrinks the index.
    let max_passes = index.accounted_bytes() + 3;
    let mut lumps = false;
    loop {
        report.passes += 1;
        assert!(
            report.passes <= max_passes,
            "collapse did not terminate: {} passes",
            report.passes
        );
        let before = (index.units(), index.accounted_bytes());
        // Read again every pass: a mount made since must not be statted.
        let mounts = host.mountinfo()?;
        // Kept exact through the pass (one walk of the index per pass, not
        // per step: TASK-230).
        let mut users = index.set_users();
        for step in plan(index, PLAN_LIMIT, lumps, &users) {
            if done(index) {
                break;
            }
            execute(index, host, &mounts, &step, &mut users, &mut report)?;
        }
        users.retain(|_, n| *n > 0);
        assert_eq!(users, index.set_users(), "finder-set users drifted");
        index.sweep_tables();
        if done(index) {
            break;
        }
        if (index.units(), index.accounted_bytes()) == before {
            if lumps {
                break;
            }
            lumps = true;
        }
    }
    report.reached_low_water = index.at_low_water();
    Ok(report)
}

// ---- Plan (pure) ----

/// How many units use each finder set.
type Users = BTreeMap<FinderSetId, usize>;

/// The first `limit` steps in key order that would shrink the index now.
fn plan(index: &Index, limit: usize, lumps: bool, users: &Users) -> Vec<Step> {
    assert!(limit >= 1, "a plan window of no step");
    let ranks = index.set_ranks();
    let mut best: BinaryHeap<Step> = BinaryHeap::new();
    for (mount, trie) in index.mounts.iter().enumerate() {
        let mut path = trie.key.mount_point.clone();
        let depth = path.components().count();
        let mut planner = Planner {
            index,
            ranks: &ranks,
            users,
            mount,
            root_depth: depth,
            lumps,
            limit,
            best: &mut best,
        };
        planner.node(&trie.root, &mut path, depth);
    }
    best.into_sorted_vec()
}

struct Planner<'a> {
    index: &'a Index,
    ranks: &'a BTreeMap<FinderSetId, usize>,
    users: &'a Users,
    mount: usize,
    root_depth: usize,
    lumps: bool,
    limit: usize,
    /// The window: a max-heap by key, so its top is the step to drop.
    best: &'a mut BinaryHeap<Step>,
}

impl Planner<'_> {
    /// Plans the steps at `node` and below; returns the units of each finder
    /// set in its subtree (its own single included).
    fn node(
        &mut self,
        node: &Node,
        path: &mut PathBuf,
        depth: usize,
    ) -> BTreeMap<FinderSetId, Agg> {
        let mut beneath: BTreeMap<FinderSetId, Agg> = BTreeMap::new();
        let mut families: BTreeMap<(FinderSetId, Name), Agg> = BTreeMap::new();
        for block in &node.blocks {
            let agg = Agg::one(block.known_below(BIG_UNIT_BYTES));
            add_to(&mut beneath, block.finders, agg);
            if let Family::Name(family) = &block.family {
                add_to(&mut families, (block.finders, family.clone()), agg);
            }
        }
        for (name, child) in &node.children {
            path.push(OsStr::from_bytes(name));
            let sub = self.node(child, path, depth + 1);
            path.pop();
            for (set, agg) in sub {
                add_to(&mut beneath, set, agg);
            }
            if let Some(single) = &child.single {
                let agg = Agg::one(single.known_below(BIG_UNIT_BYTES));
                add_to(&mut families, (single.finders, name_family(name)), agg);
            }
        }
        for ((set, family), agg) in families {
            self.step(node, path, depth, Scope::Family(family), Some(set), agg);
        }
        for (set, agg) in &beneath {
            self.step(node, path, depth, Scope::Any, Some(*set), *agg);
        }
        if self.lumps {
            let mut all = Agg::NONE;
            for agg in beneath.values() {
                all.merge(*agg);
            }
            let at_mount_root = depth == self.root_depth;
            let already_one_lump =
                all.count == 1 && node.blocks.len() == 1 && node.blocks[0].family == Family::Any;
            if all.count >= 2 || (at_mount_root && all.count == 1 && !already_one_lump) {
                self.emit_step(node, path, depth, Scope::Lump, None, all);
            }
        }
        if let Some(single) = &node.single {
            add_to(
                &mut beneath,
                single.finders,
                Agg::one(single.known_below(BIG_UNIT_BYTES)),
            );
        }
        beneath
    }

    fn step(
        &mut self,
        site: &Node,
        path: &Path,
        depth: usize,
        scope: Scope,
        set: Option<FinderSetId>,
        agg: Agg,
    ) {
        let set_id = set.expect("a family or all-names step has a set");
        if agg.count >= 2 && self.index.site_within_roots(set_id, path) {
            self.emit_step(site, path, depth, scope, set, agg);
        }
    }

    /// Puts the step into the window if its key places it there and it
    /// would shrink the index (the gain is counted only then: it walks the
    /// step's scope).
    fn emit_step(
        &mut self,
        site: &Node,
        path: &Path,
        depth: usize,
        scope: Scope,
        set: Option<FinderSetId>,
        agg: Agg,
    ) {
        let step = Step {
            big: !agg.small,
            depth,
            mount: self.mount,
            path: path.to_path_buf(),
            set_rank: set.map_or(0, |s| self.ranks[&s]),
            takes: Takes { scope, set },
        };
        let placed =
            self.best.len() < self.limit || self.best.peek().is_some_and(|worst| step < *worst);
        if !placed || gain(self.index, site, &step.takes, self.users) <= 0 {
            return;
        }
        self.best.push(step);
        if self.best.len() > self.limit {
            self.best.pop();
        }
    }
}

/// The units a step would take at `site` now.
fn in_scope(site: &Node, takes: &Takes) -> Agg {
    let mut agg = Agg::NONE;
    for block in site.blocks.iter().filter(|b| takes.block_at_site(b)) {
        agg.merge(Agg::one(block.known_below(BIG_UNIT_BYTES)));
    }
    below_in_scope(site, takes, true, &mut agg);
    agg
}

fn below_in_scope(node: &Node, takes: &Takes, direct: bool, agg: &mut Agg) {
    for (name, child) in &node.children {
        if let Some(single) = child
            .single
            .as_ref()
            .filter(|s| takes.single(name, s, direct))
        {
            agg.merge(Agg::one(single.known_below(BIG_UNIT_BYTES)));
        }
        if takes.deep() {
            for block in child.blocks.iter().filter(|b| takes.block_below(b)) {
                agg.merge(Agg::one(block.known_below(BIG_UNIT_BYTES)));
            }
            below_in_scope(child, takes, false, agg);
        }
    }
}

/// Exactly what taking the units in scope beneath `node`'s children frees
/// in the trie, the nodes emptied included.
fn freed_below(node: &Node, takes: &Takes, direct: bool) -> usize {
    let mut freed = 0;
    let mut emptied = 0;
    for (name, child) in &node.children {
        let (child_freed, empty) = freed_in(name, child, takes, direct);
        freed += child_freed;
        if empty {
            emptied += 1;
            freed += name_bytes(name);
        }
    }
    let len = node.children.len();
    freed + children_map_bytes(len) - children_map_bytes(len - emptied)
}

/// What taking from `node` (named `name`) frees, and whether it is then
/// empty (its own map and names already counted in the freed bytes).
fn freed_in(name: &[u8], node: &Node, takes: &Takes, direct: bool) -> (usize, bool) {
    let single_taken = node
        .single
        .as_ref()
        .is_some_and(|s| takes.single(name, s, direct));
    let mut freed = if single_taken { single_bytes() } else { 0 };
    let single_left = node.single.is_some() && !single_taken;
    if !takes.deep() {
        return (
            freed,
            !single_left && node.blocks.is_empty() && node.children.is_empty(),
        );
    }
    let kept: Vec<&Block> = node
        .blocks
        .iter()
        .filter(|b| !takes.block_below(b))
        .collect();
    freed += exact_blocks_bytes(&node.blocks) - exact_blocks_bytes(kept.iter().copied());
    let mut emptied = 0;
    for (child_name, child) in &node.children {
        let (child_freed, empty) = freed_in(child_name, child, takes, false);
        freed += child_freed;
        if empty {
            emptied += 1;
            freed += name_bytes(child_name);
        }
    }
    let len = node.children.len();
    freed += children_map_bytes(len) - children_map_bytes(len - emptied);
    (freed, !single_left && kept.is_empty() && emptied == len)
}

/// The exact change of the trie's bytes if the step absorbed everything in
/// scope at `site` now (positive: it shrinks).
fn trie_gain(site: &Node, takes: &Takes) -> isize {
    let freed = freed_below(site, takes, true);
    let before = exact_blocks_bytes(&site.blocks);
    let new_block = Block {
        site: Identity {
            dev: crate::host::Dev { major: 0, minor: 0 },
            ino: 0,
            birth: super::Birth::ParentIno(0),
        },
        finders: FinderSetId(0),
        family: takes.family(),
        members: 1,
        est_bytes: 0,
        files: 0,
        oldest_change: crate::host::Timestamp { secs: 0, nanos: 0 },
        newest_change: crate::host::Timestamp { secs: 0, nanos: 0 },
        tallied_at: None,
    };
    let after = exact_blocks_bytes(
        site.blocks
            .iter()
            .filter(|b| !takes.block_at_site(b))
            .chain(std::iter::once(&new_block)),
    );
    let signed = |b: usize| isize::try_from(b).expect("fits");
    signed(freed) + signed(before) - signed(after)
}

/// The step's gain over the trie and over the finder-set table:
/// positive when the index shrinks. `users` counts the units of each set
/// in the whole index. The sets only units in scope use are credited (the
/// lump's own sweep frees them). A lump's union set costs nothing when it
/// exists and a unit outside the scope keeps it (TASK-230); otherwise it
/// is charged as new, with an allocation more (conservatively).
fn gain(index: &Index, site: &Node, takes: &Takes, users: &Users) -> isize {
    let signed = |b: usize| isize::try_from(b).expect("fits");
    let trie = trie_gain(site, takes);
    if takes.set.is_some() {
        return trie;
    }
    let mut in_scope_users: Users = BTreeMap::new();
    count_in_scope(site, takes, &mut in_scope_users);
    let only_in_scope = |set: &FinderSetId, n: &usize| {
        let all = users.get(set).copied().unwrap_or(0);
        assert!(all >= *n, "finder-set users undercounted");
        all == *n
    };
    let union = index.set_union(in_scope_users.keys().copied());
    let kept_union = index
        .find_set(&union)
        .is_some_and(|id| !only_in_scope(&id, in_scope_users.get(&id).unwrap_or(&0)));
    let cost = if kept_union {
        0
    } else {
        Index::push_cost(&index.finder_sets, finder_set_bytes(&union)) + ALLOC_OVERHEAD_BYTES
    };
    let credit: usize = in_scope_users
        .iter()
        .filter(|(set, n)| only_in_scope(set, n))
        .map(|(set, _)| finder_set_bytes(index.finder_set(*set)))
        .sum();
    trie - signed(cost) + signed(credit)
}

/// Units in scope per finder set.
fn count_in_scope(site: &Node, takes: &Takes, counts: &mut BTreeMap<FinderSetId, usize>) {
    fn below(node: &Node, takes: &Takes, direct: bool, counts: &mut BTreeMap<FinderSetId, usize>) {
        for (name, child) in &node.children {
            if let Some(single) = child
                .single
                .as_ref()
                .filter(|s| takes.single(name, s, direct))
            {
                *counts.entry(single.finders).or_default() += 1;
            }
            if takes.deep() {
                for block in child.blocks.iter().filter(|b| takes.block_below(b)) {
                    *counts.entry(block.finders).or_default() += 1;
                }
                below(child, takes, false, counts);
            }
        }
    }
    for block in site.blocks.iter().filter(|b| takes.block_at_site(b)) {
        *counts.entry(block.finders).or_default() += 1;
    }
    below(site, takes, true, counts);
}

// ---- Execute: reach, re-validate (Host), absorb (pure) ----

fn node_at<'n>(root: &'n Node, names: &[&[u8]]) -> Option<&'n Node> {
    names
        .iter()
        .try_fold(root, |node, name| node.children.get(*name))
}

fn node_at_mut<'n>(root: &'n mut Node, names: &[&[u8]]) -> Option<&'n mut Node> {
    names
        .iter()
        .try_fold(root, |node, name| node.children.get_mut(*name))
}

/// Whether a failed call means the object is gone or cannot be validated
/// (its unit is evicted), rather than a failure of the host (the run
/// stops with the error, evicting nothing more: EMFILE, ENOMEM or EIO must
/// not empty a subtree).
fn means_gone(e: &HostError) -> bool {
    matches!(
        e.kernel_errno(),
        Some(
            Errno::ENOENT
                | Errno::ENOTDIR
                | Errno::EACCES
                | Errno::EPERM
                | Errno::ELOOP
                | Errno::EXDEV
        )
    )
}

/// A traversal failure: `Ok` if it means gone, the host error otherwise.
fn gone_or_error(e: ResolveError) -> Result<(), HostError> {
    match e {
        ResolveError::Host(e) if !means_gone(&e) => Err(e),
        _ => Ok(()),
    }
}

/// The fewest units a step takes: two, but a lump at the mount's root may
/// take one (a deep path becomes one block).
fn min_units(step: &Step, key: &MountKey) -> usize {
    if step.takes.scope == Scope::Lump && step.path == key.mount_point {
        1
    } else {
        2
    }
}

fn execute<H: Host>(
    index: &mut Index,
    host: &H,
    mounts: &[MountEntry],
    step: &Step,
    users: &mut Users,
    report: &mut CollapseReport,
) -> Result<(), HostError> {
    let key = index.mounts[step.mount].key.clone();
    let names = super::rel_names(&key.mount_point, &step.path);
    let Some(site) = node_at(&index.mounts[step.mount].root, &names) else {
        return Ok(());
    };
    let now = in_scope(site, &step.takes);
    if now.count < min_units(step, &key) || !now.small != step.big {
        // Taken by an earlier step, or its key changed: the next pass
        // plans it again in its right place.
        return Ok(());
    }
    if gain(index, site, &step.takes, users) <= 0 {
        // Would not shrink the index (any more): no I/O for it.
        return Ok(());
    }
    let recorded = site.single.as_ref().map(|s| s.identity);
    let reached = reach_site(host, mounts, &key, &step.path)?;
    let Some((fd, mount_id, identity)) =
        reached.filter(|(_, _, i)| recorded.is_none_or(|r| r == *i))
    else {
        // Gone, unreachable or replaced: nothing is collapsed there, and
        // everything at and beneath it is evicted.
        let before = index.units();
        index.edit_at(&key, &step.path, |node, delta| {
            for_each_set(node, &mut |set| unuse(users, set));
            let (bytes, units) = node.subtree_cost();
            *node = Node::default();
            delta.add(Delta {
                bytes: -isize::try_from(bytes).expect("fits"),
                units: -isize::try_from(units).expect("fits"),
            });
        });
        report.evicted += before - index.units();
        return Ok(());
    };
    let mount_points = mount_points_beneath(mounts, &key, &step.path);
    let mut delta = Delta::default();
    let site = node_at_mut(&mut index.mounts[step.mount].root, &names).expect("still there");
    let mut walk = Revalidate {
        host,
        mount_id,
        mount_points: &mount_points,
        takes: &step.takes,
        delta: &mut delta,
        users,
        evicted: 0,
        error: None,
    };
    // Blocks sited here stand for this directory's identity: one of
    // another (made before the directory was replaced) goes, whatever its
    // finders, before anything is absorbed.
    walk.evict_blocks_where(site, |b| b.site != identity);
    let mut path = step.path.clone();
    walk.children(site, &fd, identity.ino, &mut path, true);
    report.evicted += walk.evicted;
    let error = walk.error.take();
    index.apply(delta);
    if error.is_none() {
        absorb_step(index, step, &key, &names, identity, users, report);
    }
    // The site itself may be empty now (everything beneath was evicted).
    index.edit_at(&key, &step.path, |_, _| {});
    error.map_or(Ok(()), Err)
}

/// Step 4, after re-validation: absorbs what is left in scope if that
/// shrinks the index.
fn absorb_step(
    index: &mut Index,
    step: &Step,
    key: &MountKey,
    names: &[&[u8]],
    identity: Identity,
    users: &mut Users,
    report: &mut CollapseReport,
) {
    let site = node_at(&index.mounts[step.mount].root, names).expect("still there");
    if in_scope(site, &step.takes).count < min_units(step, key) {
        return;
    }
    let mut counts: Users = BTreeMap::new();
    count_in_scope(site, &step.takes, &mut counts);
    let union = step
        .takes
        .set
        .is_none()
        .then(|| index.set_union(counts.keys().copied()));
    let trie = trie_gain(site, &step.takes);
    if gain(index, site, &step.takes, users) <= 0 {
        return;
    }
    let accounted_before = index.accounted_bytes();
    let site = node_at_mut(&mut index.mounts[step.mount].root, names).expect("still there");
    let mut removed = Delta::default();
    let taken = take(site, &step.takes, &mut removed);
    index.apply(removed);
    // Room for a new union set: what was removed, and the sets only it
    // used, free more than it costs (see `gain`).
    let set = match (step.takes.set, union) {
        (Some(set), _) => set,
        (None, Some(union)) => {
            index.sweep_sets();
            index.intern_set(union)
        }
        (None, None) => unreachable!("a lump has a union"),
    };
    for (taken_set, n) in &counts {
        for _ in 0..*n {
            unuse(users, *taken_set);
        }
    }
    *users.entry(set).or_default() += 1;
    let units = taken.len();
    let mut taken = taken.into_iter().map(|mut b| {
        b.finders = set;
        b
    });
    let mut block = taken.next().expect("at least one unit");
    block.site = identity;
    block.family = step.takes.family();
    for other in taken {
        block.absorb(&other);
    }
    let site = node_at_mut(&mut index.mounts[step.mount].root, names).expect("still there");
    let mut pushed = Delta::default();
    site.change_blocks(&mut pushed, |blocks| blocks.push(block));
    index.apply(pushed);
    let mut net = removed;
    net.add(pushed);
    assert_eq!(-net.bytes, trie, "collapse gain mispredicted");
    assert!(net.bytes < 0, "a collapse grew the trie");
    assert!(
        index.accounted_bytes() < accounted_before,
        "a collapse grew the index (tables included)"
    );
    report.absorbed += units;
    report.collapses += 1;
    if step.takes.scope == Scope::Lump {
        report.lumps += 1;
    }
}

/// Moves every unit in scope at `site` out of the trie, as blocks (a single
/// as a block of one member).
fn take(site: &mut Node, takes: &Takes, delta: &mut Delta) -> Vec<Block> {
    let mut taken: Vec<Block> = Vec::new();
    site.change_blocks(delta, |blocks| {
        let (mine, rest): (Vec<Block>, Vec<Block>) = std::mem::take(blocks)
            .into_iter()
            .partition(|b| takes.block_at_site(b));
        *blocks = rest;
        taken.extend(mine);
    });
    take_below(site, takes, true, delta, &mut taken);
    taken
}

fn take_below(
    node: &mut Node,
    takes: &Takes,
    direct: bool,
    delta: &mut Delta,
    taken: &mut Vec<Block>,
) {
    let mut emptied: Vec<Name> = Vec::new();
    for (name, child) in node.children.iter_mut() {
        if child
            .single
            .as_ref()
            .is_some_and(|s| takes.single(name, s, direct))
        {
            let single = child.take_single(delta).expect("checked");
            // Site and family are set by the caller.
            taken.push(Block::of_single(single.identity, Family::Any, &single));
        }
        if takes.deep() {
            child.change_blocks(delta, |blocks| {
                let (mine, rest): (Vec<Block>, Vec<Block>) = std::mem::take(blocks)
                    .into_iter()
                    .partition(|b| takes.block_below(b));
                *blocks = rest;
                taken.extend(mine);
            });
            take_below(child, takes, false, delta, taken);
        }
        if child.is_empty() {
            emptied.push(name.clone());
        }
    }
    for name in emptied {
        node.remove_child(&name, delta);
    }
}

/// Reaches the site directory: its fd, its mount's id and its identity.
/// `None` for anything that is not the same mount's directory, reached
/// safely; an error for a failure of the host.
pub(super) fn reach_site<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    key: &MountKey,
    site: &Path,
) -> Result<Option<(H::Fd, u64, Identity)>, HostError> {
    if screen_mount_chain(mounts, site).is_err() {
        return Ok(None);
    }
    let Ok(mount) = mount_for(mounts, site) else {
        return Ok(None);
    };
    if mount.mount_point != key.mount_point || mount.dev != key.dev {
        return Ok(None);
    }
    if site == key.mount_point {
        let resolved = match resolve_in(host, mounts, site, Want::Directory) {
            Ok(resolved) => resolved,
            Err(e) => return gone_or_error(e).map(|()| None),
        };
        let found = resolved
            .stat
            .ino
            .and_then(|ino| Identity::of(&resolved.stat, ino))
            .map(|identity| (resolved.fd, resolved.mount.id, identity));
        return Ok(found);
    }
    let (Some(parent_path), Some(name)) = (site.parent(), site.file_name()) else {
        return Ok(None);
    };
    let parent = match resolve_in(host, mounts, parent_path, Want::Directory) {
        Ok(parent) => parent,
        Err(e) => return gone_or_error(e).map(|()| None),
    };
    let Some(parent_ino) = parent.stat.ino else {
        return Ok(None);
    };
    match open_beneath(
        host,
        &parent.fd,
        parent.mount.id,
        Path::new(name),
        Access::Path,
        Want::Directory,
    ) {
        Ok((fd, stat)) => {
            Ok(Identity::of(&stat, parent_ino).map(|identity| (fd, parent.mount.id, identity)))
        }
        Err(e) => gone_or_error(e).map(|()| None),
    }
}

/// Mount points strictly beneath `site` other than the trie's own mount.
fn mount_points_beneath(mounts: &[MountEntry], key: &MountKey, site: &Path) -> BTreeSet<PathBuf> {
    mounts
        .iter()
        .map(|m| &m.mount_point)
        .filter(|p| p.starts_with(site) && **p != site && **p != key.mount_point)
        .cloned()
        .collect()
}

/// Step 3: walks the units in scope beneath the site, evicting each one
/// whose re-validation fails.
struct Revalidate<'a, H: Host> {
    host: &'a H,
    mount_id: u64,
    mount_points: &'a BTreeSet<PathBuf>,
    takes: &'a Takes,
    delta: &'a mut Delta,
    /// The pass's count of units per finder set, kept exact on eviction.
    users: &'a mut Users,
    evicted: usize,
    /// A failure of the host: the walk stops, the step absorbs nothing.
    error: Option<HostError>,
}

impl<H: Host> Revalidate<'_, H> {
    /// Re-validates the units in scope beneath `dir` (open as `fd`, inode
    /// `dir_ino`, at `path`; `direct`: `dir` is the site).
    fn children(
        &mut self,
        dir: &mut Node,
        fd: &H::Fd,
        dir_ino: u64,
        path: &mut PathBuf,
        direct: bool,
    ) {
        let mut emptied: Vec<Name> = Vec::new();
        for (name, child) in dir.children.iter_mut() {
            if self.error.is_some() {
                break;
            }
            let single_in_scope = child
                .single
                .as_ref()
                .is_some_and(|s| self.takes.single(name, s, direct));
            let below_in_scope = self.takes.deep() && holds_below(child, self.takes);
            if !single_in_scope && !below_in_scope {
                continue;
            }
            path.push(OsStr::from_bytes(name));
            if self.mount_points.contains(path.as_path()) {
                // Never statx or open a mount point: it may be a dead FUSE
                // mount. Its units are on another mount now.
                if single_in_scope {
                    self.evict_single(child);
                }
                if below_in_scope {
                    self.evict_below(child);
                }
            } else {
                if single_in_scope {
                    let recorded = child.single.as_ref().map(|s| s.identity);
                    match self
                        .host
                        .statx(Base::Dir(fd), Path::new(OsStr::from_bytes(name)))
                    {
                        Ok(stat) => {
                            let found = Identity::of(&stat, dir_ino);
                            if found.is_none() || found != recorded {
                                self.evict_single(child);
                            }
                        }
                        Err(e) if means_gone(&e) => self.evict_single(child),
                        Err(e) => self.error = Some(e),
                    }
                }
                if below_in_scope && self.error.is_none() {
                    let opened = open_beneath(
                        self.host,
                        fd,
                        self.mount_id,
                        Path::new(OsStr::from_bytes(name)),
                        Access::Path,
                        Want::Directory,
                    );
                    match opened {
                        Ok((child_fd, stat)) => {
                            let ids = stat
                                .ino
                                .and_then(|ino| Some((ino, Identity::of(&stat, dir_ino)?)));
                            match ids {
                                Some((ino, identity)) => {
                                    let takes = self.takes;
                                    self.evict_blocks_where(child, |b| {
                                        takes.block_below(b) && b.site != identity
                                    });
                                    self.children(child, &child_fd, ino, path, false);
                                }
                                None => self.evict_below(child),
                            }
                        }
                        Err(e) => match gone_or_error(e) {
                            Ok(()) => self.evict_below(child),
                            Err(e) => self.error = Some(e),
                        },
                    }
                }
            }
            path.pop();
            if child.is_empty() {
                emptied.push(name.clone());
            }
        }
        for name in emptied {
            dir.remove_child(&name, self.delta);
        }
    }

    fn evict_single(&mut self, node: &mut Node) {
        let single = node.take_single(self.delta).expect("a single to evict");
        unuse(self.users, single.finders);
        self.evicted += 1;
    }

    /// Evicts the blocks at `node` that `failed` names.
    fn evict_blocks_where(&mut self, node: &mut Node, failed: impl Fn(&Block) -> bool) {
        let before = node.blocks.len();
        let users = &mut *self.users;
        node.change_blocks(self.delta, |blocks| {
            blocks.retain(|b| {
                let evict = failed(b);
                if evict {
                    unuse(users, b.finders);
                }
                !evict
            });
        });
        self.evicted += before - node.blocks.len();
    }

    /// Evicts every unit in scope strictly beneath `node`'s path (its
    /// blocks and everything in its children).
    fn evict_below(&mut self, node: &mut Node) {
        let takes = self.takes;
        self.evict_blocks_where(node, |b| takes.block_below(b));
        let mut emptied: Vec<Name> = Vec::new();
        for (name, child) in node.children.iter_mut() {
            if child
                .single
                .as_ref()
                .is_some_and(|s| takes.single(name, s, false))
            {
                self.evict_single(child);
            }
            self.evict_below(child);
            if child.is_empty() {
                emptied.push(name.clone());
            }
        }
        for name in emptied {
            node.remove_child(&name, self.delta);
        }
    }
}

/// One unit of `set` fewer.
fn unuse(users: &mut Users, set: FinderSetId) {
    let n = users.get_mut(&set).expect("a unit of a counted set");
    *n = n.checked_sub(1).expect("finder-set users went negative");
}

/// Calls `f` with the finder set of every unit at and beneath `node`.
fn for_each_set(node: &Node, f: &mut impl FnMut(FinderSetId)) {
    if let Some(single) = &node.single {
        f(single.finders);
    }
    for block in &node.blocks {
        f(block.finders);
    }
    for child in node.children.values() {
        for_each_set(child, f);
    }
}

/// Whether a unit in scope lies strictly beneath `node`'s path.
fn holds_below(node: &Node, takes: &Takes) -> bool {
    node.blocks.iter().any(|b| takes.block_below(b))
        || node.children.iter().any(|(name, c)| {
            c.single
                .as_ref()
                .is_some_and(|s| takes.single(name, s, false))
                || holds_below(c, takes)
        })
}
