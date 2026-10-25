//! The index's data: a [`Single`] per candidate, a [`Block`] per collapsed
//! group of candidates, and what both refer to (docs/architecture.md, The
//! index). Plain values with the block arithmetic; no I/O, no trie.

use std::path::Path;

use crate::engine::Reason;
use crate::host::{Dev, MonoTime, Stat, Timestamp};

/// What tells a stored unit from another object under the same name
/// (docs/architecture.md, The index: Identity). A match key for the index,
/// never the identity a deletion check compares against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity {
    pub dev: Dev,
    pub ino: u64,
    pub birth: Birth,
}

/// The third part of an [`Identity`]: the birth time where the filesystem
/// reports one, otherwise the inode number of the directory it was found
/// in. No mount id: those are per boot and reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Birth {
    Time(Timestamp),
    /// The parent directory's inode number. For the root of a mount (whose
    /// `..` is on another filesystem) it is the object's own inode number.
    ParentIno(u64),
}

impl Identity {
    /// The identity in `stat`, of an object found in the directory with
    /// inode number `parent_ino`. `None` when the kernel did not report the
    /// inode number.
    pub fn of(stat: &Stat, parent_ino: u64) -> Option<Self> {
        let ino = stat.ino?;
        let birth = match stat.btime {
            Some(t) => Birth::Time(t),
            None => Birth::ParentIno(parent_ino),
        };
        Some(Self {
            dev: stat.dev,
            ino,
            birth,
        })
    }
}

/// A finder set in the index's table: the units of one set share it, so a
/// unit stores four bytes, not its finders. Each new set gets the next
/// number, never reused, so an id kept past the set's sweep
/// ([`super::Index::sweep_tables`]) is stale, never another set's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FinderSetId(pub(super) u32);

/// Who found a unit: the profile, the hash of its definition (kind,
/// `find`, `where`, `action`) and the root it was bound to
/// (docs/architecture.md, The index: Finders).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Finder {
    pub profile: Box<str>,
    pub profile_hash: u64,
    /// The root it was bound to (absolute).
    pub root: Box<Path>,
}

/// What the tally learned about a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    /// Allocated bytes, each inode counted once.
    pub est_bytes: u64,
    pub files: u64,
    pub tallied_at: MonoTime,
}

/// A tally refusal, kept as a hint (docs/architecture.md, Refusals): it
/// only ranks the unit after its tier's others, never skips it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    pub reason: Reason,
    pub at: MonoTime,
}

/// One candidate. Its name is the trie key it is stored under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Single {
    pub identity: Identity,
    pub(super) finders: FinderSetId,
    /// The newest mtime or ctime in its tree once tallied; until then the
    /// provisional key, `max(mtime, ctime)` of its root.
    pub newest_change: Timestamp,
    /// `None`: never tallied (or its walk did not finish).
    pub tally: Option<Tally>,
    pub checked_at: MonoTime,
    pub refusal: Option<Refusal>,
}

impl Single {
    pub fn finders(&self) -> FinderSetId {
        self.finders
    }

    /// A known estimate below `bytes` (an untallied unit is never small).
    pub(super) fn known_below(&self, bytes: u64) -> bool {
        self.tally.is_some_and(|t| t.est_bytes < bytes)
    }
}

/// Which names a block stands for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    /// The candidates directly beneath the site whose name family
    /// ([`super::name_family`]) is this.
    Name(Box<[u8]>),
    /// Every candidate of its finders beneath the site, at any depth.
    Any,
}

/// Many candidates, collapsed: every candidate of its finders beneath the
/// directory `site` that its [`Family`] names (a lump's finders are the
/// union of several sets, and its site may lie above some of their roots). Names and identities of the
/// members are not stored; the reclaim walk finds them again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The identity of the site directory when it was collapsed.
    pub site: Identity,
    pub(super) finders: FinderSetId,
    pub family: Family,
    pub members: u64,
    /// Sums over the members that were tallied.
    pub est_bytes: u64,
    pub files: u64,
    /// Min and max of the members' `newest_change`.
    pub oldest_change: Timestamp,
    pub newest_change: Timestamp,
    /// The oldest `tallied_at` among the members; `None` if any member was
    /// never tallied (then `est_bytes` is a lower bound).
    pub tallied_at: Option<MonoTime>,
}

impl Block {
    pub fn finders(&self) -> FinderSetId {
        self.finders
    }

    /// A block holding the one member `single`.
    pub(super) fn of_single(site: Identity, family: Family, single: &Single) -> Self {
        Self {
            site,
            finders: single.finders,
            family,
            members: 1,
            est_bytes: single.tally.map_or(0, |t| t.est_bytes),
            files: single.tally.map_or(0, |t| t.files),
            oldest_change: single.newest_change,
            newest_change: single.newest_change,
            tallied_at: single.tally.map(|t| t.tallied_at),
        }
    }

    /// Adds the members, sums and age range of `other` (same finders).
    pub(super) fn absorb(&mut self, other: &Block) {
        assert_eq!(self.finders, other.finders, "absorbing another finder set");
        let add = |a: u64, b: u64| a.checked_add(b).expect("index block total overflows u64");
        self.members = add(self.members, other.members);
        self.est_bytes = add(self.est_bytes, other.est_bytes);
        self.files = add(self.files, other.files);
        self.oldest_change = self.oldest_change.min(other.oldest_change);
        self.newest_change = self.newest_change.max(other.newest_change);
        self.tallied_at = match (self.tallied_at, other.tallied_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            _ => None,
        };
    }

    /// A known estimate below `bytes`.
    pub(super) fn known_below(&self, bytes: u64) -> bool {
        self.tallied_at.is_some() && self.est_bytes < bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ts(secs: i64) -> Timestamp {
        Timestamp { secs, nanos: 0 }
    }

    fn single(est: Option<u64>, change: i64, tallied: u64) -> Single {
        Single {
            identity: Identity {
                dev: Dev { major: 0, minor: 1 },
                ino: 7,
                birth: Birth::ParentIno(2),
            },
            finders: FinderSetId(0),
            newest_change: ts(change),
            tally: est.map(|est_bytes| Tally {
                est_bytes,
                files: 1,
                tallied_at: MonoTime(Duration::from_secs(tallied)),
            }),
            checked_at: MonoTime(Duration::ZERO),
            refusal: None,
        }
    }

    #[test]
    fn a_block_sums_its_members_and_spans_their_ages() {
        let site = single(None, 0, 0).identity;
        let mut block = Block::of_single(site, Family::Any, &single(Some(10), 50, 9));
        block.absorb(&Block::of_single(
            site,
            Family::Any,
            &single(Some(5), 20, 3),
        ));
        block.absorb(&Block::of_single(
            site,
            Family::Any,
            &single(Some(1), 90, 4),
        ));
        assert_eq!((block.members, block.est_bytes, block.files), (3, 16, 3));
        assert_eq!((block.oldest_change, block.newest_change), (ts(20), ts(90)));
        assert_eq!(block.tallied_at, Some(MonoTime(Duration::from_secs(3))));
        assert!(block.known_below(17) && !block.known_below(16));
    }

    #[test]
    fn an_untallied_member_makes_the_block_untallied_and_never_small() {
        let site = single(None, 0, 0).identity;
        let mut block = Block::of_single(site, Family::Any, &single(Some(10), 50, 9));
        block.absorb(&Block::of_single(site, Family::Any, &single(None, 20, 0)));
        assert_eq!((block.members, block.est_bytes), (2, 10));
        assert_eq!(block.tallied_at, None);
        assert!(!block.known_below(u64::MAX));
        assert!(!single(None, 0, 0).known_below(u64::MAX));
    }
}
