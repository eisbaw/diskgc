//! A tier's queue (docs/architecture.md, The index: Trie and queue; Order
//! within a tier): a pure function of the index and the tier, rebuilt when
//! tiers change, never stored.
//!
//! A unit enters when one of its finders is a source of the tier (same
//! profile, profile hash and bound root), it lies on the tier's filesystem,
//! and its age range could pass the tier's idle filter (a block by its
//! oldest member for a positive filter, its newest for a negated one, an
//! untallied unit never excluded by a negated one; the per-member filters
//! run after expansion, never on the block).
//!
//! [`TierQueue::ready`] is in `oldest` order: by `newest_change` (a block
//! by its `oldest_change`), then mount, path, singles before blocks, then
//! family. That is the whole order for `oldest`. For `best-fit` the
//! executor's queue takes the same list with [`QueueEntry::estimate`]
//! (bytes, deletions): a block is many deletions, so it ranks after a
//! single of the same size; units without an estimate come after the
//! sized ones, in this order. Units with a stored refusal hint are in
//! [`TierQueue::hinted`], in the same order, for after the tier's others.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use super::unit::{Block, Family, FinderSetId, Single};
use super::{Index, MountKey, UnitRef};
use crate::host::{Dev, Timestamp};

/// A tier's idle filter, reduced to what a stored age can decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgeFilter {
    None,
    /// Positive (`filter:idle-*`): last changed at or before this time.
    IdleSince(Timestamp),
    /// Negated: changed after this time.
    ChangedSince(Timestamp),
}

/// A source term of the tier: a profile, its definition's hash and the
/// root it binds in this run.
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    pub profile: &'a str,
    pub profile_hash: u64,
    pub root: &'a Path,
}

#[derive(Debug, Clone, Copy)]
pub struct TierQuery<'a> {
    /// The filesystem the run works on.
    pub dev: Dev,
    pub sources: &'a [Source<'a>],
    pub age: AgeFilter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Queued {
    Single(Single),
    Block(Block),
}

/// A unit in a tier's queue: a copy, held outside the index's budget as
/// per-tier discovery holds its candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub mount: MountKey,
    /// The single's path, or the block's site.
    pub path: PathBuf,
    pub unit: Queued,
}

impl QueueEntry {
    /// The `oldest` key.
    pub fn order_key(&self) -> Timestamp {
        match &self.unit {
            Queued::Single(s) => s.newest_change,
            Queued::Block(b) => b.oldest_change,
        }
    }

    /// The `best-fit` estimate: bytes and deletions, if known.
    pub fn estimate(&self) -> Option<(u64, usize)> {
        match &self.unit {
            Queued::Single(s) => s.tally.map(|t| (t.est_bytes, 1)),
            Queued::Block(b) => b.tallied_at.map(|_| {
                let members = usize::try_from(b.members).expect("members fit usize");
                (b.est_bytes, members)
            }),
        }
    }

    fn family(&self) -> Option<&Family> {
        match &self.unit {
            Queued::Single(_) => None,
            Queued::Block(b) => Some(&b.family),
        }
    }

    fn cmp_order(&self, other: &Self) -> Ordering {
        self.order_key()
            .cmp(&other.order_key())
            .then_with(|| self.mount.cmp(&other.mount))
            .then_with(|| self.path.cmp(&other.path))
            .then_with(|| self.family().cmp(&other.family()))
    }
}

/// A tier's queue: [`Self::ready`] first, then [`Self::hinted`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierQueue {
    pub ready: Vec<QueueEntry>,
    pub hinted: Vec<QueueEntry>,
}

fn selected(index: &Index, set: FinderSetId, tier: &TierQuery<'_>) -> bool {
    index.finder_set(set).iter().any(|f| {
        tier.sources.iter().any(|s| {
            *f.profile == *s.profile
                && f.profile_hash == s.profile_hash
                && index.root_info(f.root).path == s.root
        })
    })
}

/// Whether the stored ages could pass `age`. Before a tally the stored
/// change is the root's own, a lower bound of the tree's newest change, so
/// an untallied unit is never excluded by a negated filter.
fn may_pass(oldest: Timestamp, newest: Timestamp, tallied: bool, age: AgeFilter) -> bool {
    match age {
        AgeFilter::None => true,
        AgeFilter::IdleSince(cutoff) => oldest <= cutoff,
        AgeFilter::ChangedSince(cutoff) => !tallied || newest > cutoff,
    }
}

/// The queue of `tier` over `index`.
pub fn derive(index: &Index, tier: &TierQuery<'_>) -> TierQueue {
    let mut queue = TierQueue::default();
    index.for_each_unit(|mount, path, unit| {
        if mount.dev != tier.dev {
            return;
        }
        let (set, oldest, newest, tallied, hinted, queued) = match unit {
            UnitRef::Single(s) => (
                s.finders,
                s.newest_change,
                s.newest_change,
                s.tally.is_some(),
                s.refusal.is_some(),
                Queued::Single(s.clone()),
            ),
            UnitRef::Block(b) => (
                b.finders,
                b.oldest_change,
                b.newest_change,
                b.tallied_at.is_some(),
                false,
                Queued::Block(b.clone()),
            ),
        };
        if !selected(index, set, tier) || !may_pass(oldest, newest, tallied, tier.age) {
            return;
        }
        let entry = QueueEntry {
            mount: mount.clone(),
            path: path.to_path_buf(),
            unit: queued,
        };
        if hinted {
            queue.hinted.push(entry);
        } else {
            queue.ready.push(entry);
        }
    });
    queue.ready.sort_by(QueueEntry::cmp_order);
    queue.hinted.sort_by(QueueEntry::cmp_order);
    queue
}
