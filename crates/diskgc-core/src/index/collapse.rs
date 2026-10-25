//! Collapse at the ceiling (docs/architecture.md, Memory budget and
//! collapse): replace the units of one finder set beneath a site directory
//! by one [`Block`], until the index is at or below its low-water mark.
//!
//! One collapse, at site `D` for finder set `F`:
//!
//! 1. **Plan** (pure): the candidate sites are the directories with at
//!    least two units of one finder set beneath them, not above any of the
//!    set's roots. Each gives a family step (the singles of one name family
//!    directly beneath `D`, with a block of that family already there) and
//!    an all-names step (every unit of `F` beneath `D`). They are taken in
//!    the order of the key: sites whose units are all known to be below
//!    [`BIG_UNIT_BYTES`] first, then the deepest, then the path, then the
//!    family step before the all-names step (ties: finder set, family).
//!    A pass plans at most [`PLAN_LIMIT`] steps (a bounded working set);
//!    the next pass plans again from what is left.
//! 2. **Reach `D`** (Host): from its mount's root without following a
//!    symlink or crossing a mount ([`crate::resolve`]), after screening the
//!    mount chain from mountinfo. Gone, unreachable, on another mount, or
//!    of another identity than a unit recorded at `D`: every unit at and
//!    beneath `D` is evicted and nothing is collapsed there.
//! 3. **Re-validate** (Host): every unit the step would absorb is reached
//!    again beneath `D` (a single by `statx` of its name in its parent, a
//!    finer block's site by opening it) and must have its recorded
//!    identity. A name that is a mount point now is never touched. A unit
//!    that fails, for any reason, is **evicted** before anything is
//!    absorbed, so it contributes nothing to the block.
//! 4. **Absorb** (pure): if at least two units are left, they become one
//!    block at `D` whose totals are the sums of theirs.
//!
//! A collapse never makes a check weaker: a block's members are found and
//! judged afresh when the reclaim walk expands it. Re-validation keeps the
//! block's estimates honest; it grants nothing.
//!
//! Termination: every pass either lowers the number of units or ends the
//! run, so a run ends after at most one pass per unit (asserted).

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::trie::{Delta, Name, Node};
use super::unit::{Block, Family, FinderSetId, Identity};
use super::{Index, MountKey, name_family};
use crate::host::{Access, Base, Host, HostError, MountEntry};
use crate::resolve::{Want, mount_for, open_beneath, resolve_in, screen_mount_chain};

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
    /// Units absorbed into blocks.
    pub absorbed: usize,
    /// Units evicted because their re-validation failed or their site was
    /// gone.
    pub evicted: usize,
    /// Whether the index ended at or below its low-water mark (otherwise
    /// nothing was left to collapse).
    pub reached_low_water: bool,
}

/// Which units a step takes beneath its site.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    /// Singles directly beneath of this name family, and that family's
    /// block at the site.
    Family(Name),
    /// Every unit of the finder set beneath the site.
    Any,
}

/// A planned step.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    /// Whether some unit is not known to be small (0: all small).
    big: bool,
    depth: usize,
    mount: usize,
    path: PathBuf,
    scope: Scope,
    finders: FinderSetId,
    /// The finder set's place in content order, for a tie-break that
    /// does not depend on the order sets were registered in.
    set_rank: usize,
}

impl Step {
    fn key(&self) -> (bool, Reverse<usize>, &Path, &Scope, usize) {
        (
            self.big,
            Reverse(self.depth),
            &self.path,
            &self.scope,
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

/// Units of one finder set: how many, and whether all are known small.
#[derive(Debug, Clone, Copy)]
struct Agg {
    count: usize,
    small: bool,
}

impl Agg {
    fn one(small: bool) -> Self {
        Self { count: 1, small }
    }

    fn merge(&mut self, other: Agg) {
        self.count += other.count;
        self.small &= other.small;
    }
}

fn merge_into(into: &mut BTreeMap<FinderSetId, Agg>, from: BTreeMap<FinderSetId, Agg>) {
    for (set, agg) in from {
        into.entry(set).and_modify(|a| a.merge(agg)).or_insert(agg);
    }
}

impl Index {
    /// Collapses until the index is at or below its low-water mark, or
    /// nothing is left to collapse. In `watch` the tally calls it as work
    /// of its own; [`Index::insert`] calls it at the ceiling.
    pub fn collapse_to_low_water<H: Host>(
        &mut self,
        host: &H,
    ) -> Result<CollapseReport, HostError> {
        to_low_water(self, host)
    }

    fn site_within_roots(&self, set: FinderSetId, site: &Path) -> bool {
        self.finder_set(set)
            .iter()
            .all(|f| site.starts_with(&self.root_info(f.root).path))
    }

    fn set_ranks(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.finder_sets.len()).collect();
        order.sort_by(|a, b| self.finder_sets[*a].cmp(&self.finder_sets[*b]));
        let mut rank = vec![0; order.len()];
        for (r, i) in order.into_iter().enumerate() {
            rank[i] = r;
        }
        rank
    }
}

pub(super) fn to_low_water<H: Host>(
    index: &mut Index,
    host: &H,
) -> Result<CollapseReport, HostError> {
    let mut report = CollapseReport::default();
    if index.at_low_water() {
        report.reached_low_water = true;
        return Ok(report);
    }
    let mounts = host.mountinfo()?;
    let max_passes = index.units() + 1;
    loop {
        report.passes += 1;
        assert!(
            report.passes <= max_passes,
            "collapse did not terminate: {} passes over {max_passes} units",
            report.passes
        );
        let units_before = index.units();
        for step in plan(index, PLAN_LIMIT) {
            if index.at_low_water() {
                break;
            }
            execute(index, host, &mounts, &step, &mut report);
        }
        if index.at_low_water() {
            report.reached_low_water = true;
            return Ok(report);
        }
        if index.units() == units_before {
            return Ok(report);
        }
    }
}

// ---- Plan (pure) ----

/// The best `limit` steps, in key order.
fn plan(index: &Index, limit: usize) -> Vec<Step> {
    let ranks = index.set_ranks();
    let mut best: BinaryHeap<Step> = BinaryHeap::new();
    for (mount, trie) in index.mounts.iter().enumerate() {
        let mut path = trie.key.mount_point.clone();
        let depth = path.components().count();
        plan_node(
            index,
            &ranks,
            mount,
            &trie.root,
            &mut path,
            depth,
            &mut |step| {
                best.push(step);
                if best.len() > limit {
                    best.pop();
                }
            },
        );
    }
    best.into_sorted_vec()
}

/// Plans the steps at `node` and below; returns the units of each finder
/// set in its subtree (its own single included).
fn plan_node(
    index: &Index,
    ranks: &[usize],
    mount: usize,
    node: &Node,
    path: &mut PathBuf,
    depth: usize,
    emit: &mut impl FnMut(Step),
) -> BTreeMap<FinderSetId, Agg> {
    let mut beneath: BTreeMap<FinderSetId, Agg> = BTreeMap::new();
    let mut families: BTreeMap<(FinderSetId, Name), Agg> = BTreeMap::new();
    for block in &node.blocks {
        let agg = Agg::one(block.known_below(BIG_UNIT_BYTES));
        merge_into(&mut beneath, BTreeMap::from([(block.finders, agg)]));
        if let Family::Name(family) = &block.family {
            families
                .entry((block.finders, family.clone()))
                .and_modify(|a| a.merge(agg))
                .or_insert(agg);
        }
    }
    for (name, child) in &node.children {
        path.push(OsStr::from_bytes(name));
        let sub = plan_node(index, ranks, mount, child, path, depth + 1, emit);
        path.pop();
        merge_into(&mut beneath, sub);
        if let Some(single) = &child.single {
            let agg = Agg::one(single.known_below(BIG_UNIT_BYTES));
            families
                .entry((single.finders, name_family(name)))
                .and_modify(|a| a.merge(agg))
                .or_insert(agg);
        }
    }
    let mut step = |scope: Scope, finders: FinderSetId, agg: Agg| {
        if agg.count >= 2 && index.site_within_roots(finders, path) {
            emit(Step {
                big: !agg.small,
                depth,
                mount,
                path: path.clone(),
                scope,
                finders,
                set_rank: ranks[finders.0 as usize],
            });
        }
    };
    for ((finders, family), agg) in families {
        step(Scope::Family(family), finders, agg);
    }
    for (finders, agg) in &beneath {
        step(Scope::Any, *finders, *agg);
    }
    if let Some(single) = &node.single {
        merge_into(
            &mut beneath,
            BTreeMap::from([(single.finders, Agg::one(single.known_below(BIG_UNIT_BYTES)))]),
        );
    }
    beneath
}

/// Whether a block at the site is taken by a step.
fn block_in_scope(block: &Block, finders: FinderSetId, scope: &Scope) -> bool {
    block.finders == finders
        && match scope {
            Scope::Any => true,
            Scope::Family(family) => block.family == Family::Name(family.clone()),
        }
}

/// The units a step would take at `site` now.
fn in_scope(site: &Node, finders: FinderSetId, scope: &Scope) -> Agg {
    let mut agg = Agg {
        count: 0,
        small: true,
    };
    for block in site
        .blocks
        .iter()
        .filter(|b| block_in_scope(b, finders, scope))
    {
        agg.merge(Agg::one(block.known_below(BIG_UNIT_BYTES)));
    }
    for (name, child) in &site.children {
        match scope {
            Scope::Family(family) => {
                if let Some(single) = child.single.as_ref().filter(|s| s.finders == finders)
                    && name_family(name) == *family
                {
                    agg.merge(Agg::one(single.known_below(BIG_UNIT_BYTES)));
                }
            }
            Scope::Any => {
                if let Some(single) = child.single.as_ref().filter(|s| s.finders == finders) {
                    agg.merge(Agg::one(single.known_below(BIG_UNIT_BYTES)));
                }
                let below = in_scope(child, finders, scope);
                if below.count > 0 {
                    agg.merge(below);
                }
            }
        }
    }
    agg
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

fn execute<H: Host>(
    index: &mut Index,
    host: &H,
    mounts: &[MountEntry],
    step: &Step,
    report: &mut CollapseReport,
) {
    let key = index.mounts[step.mount].key.clone();
    let names = super::rel_names(&key.mount_point, &step.path);
    let Some(site) = node_at(&index.mounts[step.mount].root, &names) else {
        return;
    };
    let now = in_scope(site, step.finders, &step.scope);
    if now.count < 2 || !now.small != step.big {
        // Taken by an earlier step, or its key changed: the next pass
        // plans it again in its right place.
        return;
    }
    let recorded = site
        .single
        .as_ref()
        .map(|s| s.identity)
        .or_else(|| site.blocks.first().map(|b| b.site));
    let reached = reach_site(host, mounts, &key, &step.path);
    let mut delta = Delta::default();
    let identity = match reached {
        Some((fd, mount_id, identity)) if recorded.is_none_or(|r| r == identity) => {
            let mount_points = mount_points_beneath(mounts, &key, &step.path);
            let site =
                node_at_mut(&mut index.mounts[step.mount].root, &names).expect("still there");
            let mut walk = Revalidate {
                host,
                mount_id,
                mount_points: &mount_points,
                finders: step.finders,
                scope: &step.scope,
                delta: &mut delta,
                evicted: 0,
            };
            let mut path = step.path.clone();
            walk.children(site, &fd, identity.ino, &mut path);
            report.evicted += walk.evicted;
            identity
        }
        _ => {
            // Gone, unreachable or replaced: nothing is collapsed there.
            let before = index.units();
            index.edit_at(&key, &step.path, |node, delta| {
                let (bytes, units) = node.subtree_cost();
                *node = Node::default();
                delta.add(Delta {
                    bytes: -isize::try_from(bytes).expect("fits"),
                    units: -isize::try_from(units).expect("fits"),
                });
            });
            report.evicted += before - index.units();
            return;
        }
    };
    let site = node_at_mut(&mut index.mounts[step.mount].root, &names).expect("still there");
    if in_scope(site, step.finders, &step.scope).count >= 2 {
        report.absorbed += absorb(site, step.finders, &step.scope, identity, &mut delta);
        report.collapses += 1;
    }
    index.apply(delta);
    // The site itself may be empty now (everything beneath was evicted).
    index.edit_at(&key, &step.path, |_, _| {});
}

/// Reaches the site directory: its fd, its mount's id and its identity.
/// `None` for anything that is not the same mount's directory, reached
/// safely.
fn reach_site<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    key: &MountKey,
    site: &Path,
) -> Option<(H::Fd, u64, Identity)> {
    screen_mount_chain(mounts, site).ok()?;
    let mount = mount_for(mounts, site).ok()?;
    if mount.mount_point != key.mount_point || mount.dev != key.dev {
        return None;
    }
    if site == key.mount_point {
        let resolved = resolve_in(host, mounts, site, Want::Directory).ok()?;
        let ino = resolved.stat.ino?;
        let identity = Identity::of(&resolved.stat, ino)?;
        return Some((resolved.fd, resolved.mount.id, identity));
    }
    let parent = resolve_in(host, mounts, site.parent()?, Want::Directory).ok()?;
    let parent_ino = parent.stat.ino?;
    let name = Path::new(site.file_name()?);
    let (fd, stat) = open_beneath(
        host,
        &parent.fd,
        parent.mount.id,
        name,
        Access::Path,
        Want::Directory,
    )
    .ok()?;
    let identity = Identity::of(&stat, parent_ino)?;
    Some((fd, parent.mount.id, identity))
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
    finders: FinderSetId,
    scope: &'a Scope,
    delta: &'a mut Delta,
    evicted: usize,
}

impl<H: Host> Revalidate<'_, H> {
    /// Re-validates the units in scope beneath `dir` (open as `fd`, inode
    /// `dir_ino`, at `path`).
    fn children(&mut self, dir: &mut Node, fd: &H::Fd, dir_ino: u64, path: &mut PathBuf) {
        let mut emptied: Vec<Name> = Vec::new();
        for (name, child) in dir.children.iter_mut() {
            let single_in_scope = child.single.as_ref().is_some_and(|s| {
                s.finders == self.finders
                    && match self.scope {
                        Scope::Any => true,
                        Scope::Family(family) => name_family(name) == *family,
                    }
            });
            let below_in_scope = *self.scope == Scope::Any && holds_set_below(child, self.finders);
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
                    let found = self
                        .host
                        .statx(Base::Dir(fd), Path::new(OsStr::from_bytes(name)))
                        .ok()
                        .and_then(|stat| Identity::of(&stat, dir_ino));
                    if found.is_none() || found != recorded {
                        self.evict_single(child);
                    }
                }
                if below_in_scope {
                    let opened = open_beneath(
                        self.host,
                        fd,
                        self.mount_id,
                        Path::new(OsStr::from_bytes(name)),
                        Access::Path,
                        Want::Directory,
                    )
                    .ok()
                    .and_then(|(child_fd, stat)| {
                        let ino = stat.ino?;
                        Some((child_fd, ino, Identity::of(&stat, dir_ino)?))
                    });
                    match opened {
                        Some((child_fd, ino, identity)) => {
                            self.evict_blocks(child, |b| b.site != identity);
                            self.children(child, &child_fd, ino, path);
                        }
                        None => self.evict_below(child),
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
        node.take_single(self.delta);
        self.evicted += 1;
    }

    fn evict_blocks(&mut self, node: &mut Node, failed: impl Fn(&Block) -> bool) {
        let finders = self.finders;
        let before = node.blocks.len();
        node.change_blocks(self.delta, |blocks| {
            blocks.retain(|b| !(b.finders == finders && failed(b)));
        });
        self.evicted += before - node.blocks.len();
    }

    /// Evicts every unit of the finder set strictly beneath `node`'s path
    /// (its blocks and everything in its children).
    fn evict_below(&mut self, node: &mut Node) {
        self.evict_blocks(node, |_| true);
        let mut emptied: Vec<Name> = Vec::new();
        for (name, child) in node.children.iter_mut() {
            if child
                .single
                .as_ref()
                .is_some_and(|s| s.finders == self.finders)
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

/// Whether a unit of `finders` lies strictly beneath `node`'s path.
fn holds_set_below(node: &Node, finders: FinderSetId) -> bool {
    node.blocks.iter().any(|b| b.finders == finders)
        || node.children.values().any(|c| {
            c.single.as_ref().is_some_and(|s| s.finders == finders) || holds_set_below(c, finders)
        })
}

/// Step 4: takes every unit in scope beneath `site` (all re-validated)
/// into one block at it. Returns the number of units taken.
fn absorb(
    site: &mut Node,
    finders: FinderSetId,
    scope: &Scope,
    identity: Identity,
    delta: &mut Delta,
) -> usize {
    let family = match scope {
        Scope::Family(family) => Family::Name(family.clone()),
        Scope::Any => Family::Any,
    };
    let mut taken: Vec<Block> = Vec::new();
    site.change_blocks(delta, |blocks| {
        let (mine, rest): (Vec<Block>, Vec<Block>) = std::mem::take(blocks)
            .into_iter()
            .partition(|b| block_in_scope(b, finders, scope));
        *blocks = rest;
        taken.extend(mine);
    });
    take_below(site, finders, scope, identity, &family, delta, &mut taken);
    let units = taken.len();
    assert!(units >= 2, "absorbing fewer than two units");
    let mut taken = taken.into_iter();
    let mut block = taken.next().expect("two or more");
    block.site = identity;
    block.family = family;
    for other in taken {
        block.absorb(&other);
    }
    site.change_blocks(delta, |blocks| blocks.push(block));
    units
}

/// Moves the units in scope out of `node`'s children (recursively for
/// [`Scope::Any`]) into `taken`, as one-member blocks or as they are.
fn take_below(
    node: &mut Node,
    finders: FinderSetId,
    scope: &Scope,
    site: Identity,
    family: &Family,
    delta: &mut Delta,
    taken: &mut Vec<Block>,
) {
    let mut emptied: Vec<Name> = Vec::new();
    for (name, child) in node.children.iter_mut() {
        let single_in_scope = child.single.as_ref().is_some_and(|s| {
            s.finders == finders
                && match scope {
                    Scope::Any => true,
                    Scope::Family(f) => name_family(name) == *f,
                }
        });
        if single_in_scope {
            let single = child.take_single(delta).expect("checked");
            taken.push(Block::of_single(site, family.clone(), &single));
        }
        if *scope == Scope::Any {
            child.change_blocks(delta, |blocks| {
                let (mine, rest): (Vec<Block>, Vec<Block>) = std::mem::take(blocks)
                    .into_iter()
                    .partition(|b| b.finders == finders);
                *blocks = rest;
                taken.extend(mine);
            });
            take_below(child, finders, scope, site, family, delta, taken);
        }
        if child.is_empty() {
            emptied.push(name.clone());
        }
    }
    for name in emptied {
        node.remove_child(&name, delta);
    }
}
