//! A tier's queue over the index for the reclaim walk (docs/architecture.md,
//! Order within a tier): the units in the tier's order, then those with a
//! stored refusal hint; an expanded block's members join it, under
//! `oldest` by `max(mtime, ctime)` of their own root, under `best-fit` all
//! before the next pick.
//!
//! Under `oldest` the first tier's stale units can be sized as the queue
//! reaches them (TASK-251, [`TierQueue::settle`]): each unit's key in the
//! queue must then be a lower bound of the key its sizing gives it, and
//! the queue gives out the units in the order of their sized keys, sizing
//! only as many as that order needs. The walk owns the sizing and what
//! makes a key a lower bound (`super::DeferredSizing`); this module only
//! merges.
//!
//! Generic over the member payload `M` (the walk's: a found member and its
//! block), so the merge is tested without a filesystem.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use crate::config::Order;
use crate::executor::CandidateQueue;
use crate::host::{DevIno, Timestamp};
use crate::index::queue::{self, QueueEntry};

/// One item of a tier's queue: a unit of the index, or a member of an
/// expanded block found now.
pub(super) enum Item<M> {
    Unit(QueueEntry),
    Member(M),
}

/// A unit of the `oldest` queue in the index's order
/// ([`QueueEntry::cmp_order`]).
#[derive(Debug)]
struct ByOrder(QueueEntry);

impl Ord for ByOrder {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp_order(&other.0)
    }
}

impl PartialOrd for ByOrder {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ByOrder {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ByOrder {}

/// What deciding on the next unit left in the `oldest` queue came to
/// ([`TierQueue::settle`]).
#[derive(Debug)]
pub(super) enum Settle {
    /// Its key is final (sized recently, a block, or no more sizing in this
    /// run): it is taken as it is.
    Final,
    /// Decided: it waits under this entry's key: its sized
    /// `newest_change`, or its queue key when the sizing refused it or
    /// found it gone (the re-match then gives its verdict: skipped as this
    /// run's tally refused it, or `stale`).
    Ready(QueueEntry),
    /// The run stops.
    Stop,
}

pub(super) struct TierQueue<M> {
    order: Order,
    /// `oldest`: the units in the index's `oldest` order, by their queue
    /// keys.
    oldest: VecDeque<QueueEntry>,
    /// `oldest`: the units decided on the way ([`Self::settle`]), by their
    /// new keys, each until no unit left in `oldest` can come before it.
    sized: BTreeSet<ByOrder>,
    /// `best-fit`: the units by their estimates.
    best_fit: CandidateQueue<QueueEntry>,
    members: BTreeMap<(Timestamp, u64), M>,
    hinted: VecDeque<QueueEntry>,
    /// Ties of members keep the order expansion found them in.
    sequence: u64,
    /// Identities queued as members, so a member is queued once.
    queued: HashSet<DevIno>,
}

impl<M> TierQueue<M> {
    /// `queue`'s ready units must be in [`QueueEntry::cmp_order`] by the
    /// keys they carry (as [`queue::derive`] gives them).
    pub(super) fn new(order: Order, queue: queue::TierQueue) -> Self {
        let (oldest, best_fit) = match order {
            Order::Oldest => (
                queue.ready.into(),
                CandidateQueue::new(order, Vec::new(), QueueEntry::estimate),
            ),
            Order::BestFit => (
                VecDeque::new(),
                CandidateQueue::new(order, queue.ready, QueueEntry::estimate),
            ),
        };
        Self {
            order,
            oldest,
            sized: BTreeSet::new(),
            best_fit,
            members: BTreeMap::new(),
            hinted: queue.hinted.into(),
            sequence: 0,
            queued: HashSet::new(),
        }
    }

    /// Queues a member of an expanded block under `key` (`max(mtime,
    /// ctime)` of its root), once per identity `id`.
    pub(super) fn push_member(&mut self, id: DevIno, key: Timestamp, member: M) {
        if !self.queued.insert(id) {
            return;
        }
        self.sequence += 1;
        self.members.insert((key, self.sequence), member);
    }

    /// Whether the earliest unit decided on the way comes before the next
    /// one left in `oldest`.
    fn sized_first(&self) -> bool {
        match (self.sized.first(), self.oldest.front()) {
            (Some(sized), Some(unit)) => sized.0.cmp_order(unit).is_le(),
            (sized, _) => sized.is_some(),
        }
    }

    /// The next unit under `oldest`.
    fn oldest_head(&self) -> Option<&QueueEntry> {
        if self.sized_first() {
            self.sized.first().map(|s| &s.0)
        } else {
            self.oldest.front()
        }
    }

    /// Makes the next unit under `oldest` final, before [`Self::pop`]:
    /// while the next unit left in `oldest` comes before every unit decided
    /// on the way and before the next member, `decide` is asked about it
    /// (its key in the queue must be a lower bound of the key sizing gives
    /// it); one decided moves to `sized` under its new key and the next one
    /// is looked at. So the units come out in the order of their sized
    /// keys, merged with the members (a unit first on equal keys), deciding
    /// only on as many as that order needs. `false`: the run stops.
    pub(super) fn settle(&mut self, mut decide: impl FnMut(&QueueEntry) -> Settle) -> bool {
        loop {
            let Some(front) = self.oldest.front() else {
                return true;
            };
            if self.sized_first() {
                return true;
            }
            if self
                .members
                .first_key_value()
                .is_some_and(|((key, _), _)| *key < front.order_key())
            {
                return true; // a member comes first whatever the front's size
            }
            match decide(front) {
                Settle::Final => return true,
                Settle::Ready(entry) => {
                    self.oldest.pop_front();
                    self.sized.insert(ByOrder(entry));
                }
                Settle::Stop => return false,
            }
        }
    }

    pub(super) fn pop(&mut self, need: u64) -> Option<Item<M>> {
        let member = |queue: &mut Self| queue.members.pop_first().map(|(_, m)| Item::Member(m));
        match self.order {
            Order::Oldest => {
                let unit_first = match (self.oldest_head(), self.members.first_key_value()) {
                    (Some(unit), Some(((key, _), _))) => unit.order_key() <= *key,
                    (Some(_), None) => true,
                    (None, _) => false,
                };
                if unit_first {
                    let unit = if self.sized_first() {
                        self.sized.pop_first().map(|s| s.0)
                    } else {
                        self.oldest.pop_front()
                    };
                    return unit.map(Item::Unit);
                }
                if let Some(item) = member(self) {
                    return Some(item);
                }
            }
            Order::BestFit => {
                if let Some(item) = member(self) {
                    return Some(item);
                }
                if let Some(unit) = self.best_fit.pop(need) {
                    return Some(Item::Unit(unit));
                }
            }
        }
        self.hinted.pop_front().map(Item::Unit)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use proptest::prelude::*;

    use super::*;
    use crate::host::Dev;
    use crate::index::queue::Queued;
    use crate::index::{Birth, Identity, MountKey, Single};

    const DEV: Dev = Dev {
        major: 0,
        minor: 42,
    };

    fn ts(secs: i64) -> Timestamp {
        Timestamp { secs, nanos: 0 }
    }

    fn unit(n: usize, key: i64) -> QueueEntry {
        QueueEntry {
            mount: MountKey {
                dev: DEV,
                mount_point: PathBuf::from("/tmp"),
            },
            path: PathBuf::from(format!("/tmp/u{n:03}")),
            unit: Queued::Single(Single::for_test(
                Identity {
                    dev: DEV,
                    ino: n as u64 + 1,
                    birth: Birth::ParentIno(1),
                },
                ts(key),
            )),
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Out {
        Unit(PathBuf),
        Member(u64),
    }

    /// What the queue gives out with `units` (stored key, sized key, and
    /// whether sizing is still to do) and `members` (key) all queued from
    /// the start.
    fn run(units: &[(i64, i64, bool)], members: &[i64]) -> Vec<Out> {
        let mut entries: Vec<QueueEntry> = units
            .iter()
            .enumerate()
            .map(|(n, (stored, _, _))| unit(n, *stored))
            .collect();
        entries.sort_by(QueueEntry::cmp_order);
        let sized: BTreeMap<PathBuf, (i64, bool)> = units
            .iter()
            .enumerate()
            .map(|(n, (_, sized, stale))| (unit(n, 0).path, (*sized, *stale)))
            .collect();
        let mut queue: TierQueue<u64> = TierQueue::new(
            Order::Oldest,
            queue::TierQueue {
                ready: entries,
                hinted: Vec::new(),
            },
        );
        for (n, key) in members.iter().enumerate() {
            let id = DevIno {
                dev: DEV,
                ino: 1_000 + n as u64,
            };
            queue.push_member(id, ts(*key), n as u64);
        }
        let mut out = Vec::new();
        let mut decided = 0usize;
        loop {
            assert!(queue.settle(|front| {
                let (key, stale) = sized[&front.path];
                if !stale {
                    return Settle::Final;
                }
                decided += 1;
                let n: usize = front.path.to_str().unwrap()[6..].parse().unwrap();
                Settle::Ready(unit(n, key))
            }));
            match queue.pop(0) {
                Some(Item::Unit(u)) => out.push(Out::Unit(u.path)),
                Some(Item::Member(m)) => out.push(Out::Member(m)),
                None => break,
            }
        }
        assert!(decided <= units.len());
        out
    }

    /// The order full sizing gives: units by (sized key, path), members by
    /// (key, queueing order), merged with a unit first on equal keys.
    fn expected(units: &[(i64, i64, bool)], members: &[i64]) -> Vec<Out> {
        let mut u: Vec<(i64, PathBuf)> = units
            .iter()
            .enumerate()
            .map(|(n, (stored, sized, stale))| {
                (if *stale { *sized } else { *stored }, unit(n, 0).path)
            })
            .collect();
        u.sort();
        let mut m: Vec<(i64, u64)> = members
            .iter()
            .enumerate()
            .map(|(n, k)| (*k, n as u64))
            .collect();
        m.sort();
        let (mut i, mut j, mut out) = (0, 0, Vec::new());
        while i < u.len() || j < m.len() {
            if j == m.len() || (i < u.len() && u[i].0 <= m[j].0) {
                out.push(Out::Unit(u[i].1.clone()));
                i += 1;
            } else {
                out.push(Out::Member(m[j].1));
                j += 1;
            }
        }
        out
    }

    proptest! {
        /// Stored keys at most the sized keys (a lower bound), small key
        /// ranges for ties, some units already final, members in between:
        /// the queue gives out exactly the order of full sizing.
        #[test]
        fn settle_gives_the_order_of_the_sized_keys(
            units in prop::collection::vec((0i64..20, 0i64..6, any::<bool>()), 0..40),
            members in prop::collection::vec(0i64..26, 0..10),
        ) {
            let units: Vec<(i64, i64, bool)> = units
                .into_iter()
                .map(|(stored, grow, stale)| (stored, stored + if stale { grow } else { 0 }, stale))
                .collect();
            prop_assert_eq!(run(&units, &members), expected(&units, &members));
        }
    }

    /// Only as many units are sized as the order needs: with the first
    /// unit's sized key below every other stored key, taking one unit
    /// sizes one.
    #[test]
    fn settle_sizes_only_what_the_next_pop_needs() {
        let units: Vec<QueueEntry> = (0..5).map(|n| unit(n, 10 + n as i64)).collect();
        let mut queue: TierQueue<u64> = TierQueue::new(
            Order::Oldest,
            queue::TierQueue {
                ready: units,
                hinted: Vec::new(),
            },
        );
        let mut decided = 0;
        assert!(queue.settle(|front| {
            decided += 1;
            Settle::Ready(front.clone())
        }));
        assert!(matches!(queue.pop(0), Some(Item::Unit(u)) if u.path.ends_with("u000")));
        assert_eq!(decided, 1);
    }
}
