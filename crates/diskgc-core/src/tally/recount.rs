//! What complete discovery listings mean for the index (docs/architecture.md,
//! Eviction; No re-splitting in place; Mark, 2026-10-25: a complete listing
//! of a block's site recounts its members and estimates, a partial one
//! leaves them):
//!
//! - a single of a listed finder below that finder's root that the listing
//!   did not find loses the finder ([`Index::drop_finder`]);
//! - a block all of whose finders were listed completely is matched with
//!   the listed candidates it stands for (each candidate counts for the
//!   shallowest block that stands for it, as insertion's coverage does). A
//!   block of one finder whose members fit under the low-water mark is
//!   dissolved into them as singles ([`Index::dissolve`]); any other is
//!   recounted ([`Index::recount_block`]).
//!
//! A block of several finders (a unit two profiles found, a lump) is
//! recounted only when every one of them was listed completely in the same
//! round: the synchronous tally's pass 1 is one round; in the background
//! each listing is its own round, so such blocks keep their counts there
//! (their sites are still re-validated by collapse).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::listing::Listed;
use crate::index::{
    Discovered, Dissolved, Family, Finder, FinderSetId, Index, MountKey, UnitRef, name_family,
};

/// A complete listing: its finder, what it found, and what it found but the
/// index could not take.
#[derive(Debug, Clone, Copy)]
pub struct Complete<'a> {
    pub finder: &'a Finder,
    pub listed: &'a [Listed],
    pub unindexed: &'a BTreeSet<PathBuf>,
}

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recount {
    /// Singles that lost a finder (and kept others).
    pub finders_dropped: usize,
    /// Singles evicted (their last finder dropped).
    pub singles_evicted: usize,
    pub blocks_dissolved: usize,
    pub blocks_recounted: usize,
    pub blocks_evicted: usize,
}

/// A block, as found in the index.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BlockAt {
    mount: MountKey,
    site: PathBuf,
    finders: FinderSetId,
    family: Family,
}

/// Applies the complete listings of one round (see the module doc).
pub fn apply(index: &mut Index, round: &[Complete<'_>]) -> Recount {
    let mut done = Recount::default();
    if round.is_empty() {
        return done;
    }
    let complete: BTreeSet<&Finder> = round.iter().map(|c| c.finder).collect();
    // What each listing found, indexed or not: never "lost".
    let found_by: BTreeMap<&Finder, BTreeSet<&Path>> = round
        .iter()
        .map(|c| {
            let paths = c
                .listed
                .iter()
                .map(|l| l.path.as_path())
                .chain(c.unindexed.iter().map(PathBuf::as_path))
                .collect();
            (c.finder, paths)
        })
        .collect();
    // Singles a listing did not find.
    let mut lost: Vec<(MountKey, PathBuf, Finder)> = Vec::new();
    // Blocks all of whose finders are complete, with their members.
    let mut members: BTreeMap<BlockAt, BTreeMap<PathBuf, Discovered>> = BTreeMap::new();
    index.for_each_unit(|mount, path, unit| match unit {
        UnitRef::Single(single) => {
            for finder in index.finder_set(single.finders()) {
                let Some(found) = found_by.get(finder) else {
                    continue;
                };
                if path.starts_with(&finder.root) && !found.contains(path) {
                    lost.push((mount.clone(), path.to_owned(), finder.clone()));
                }
            }
        }
        UnitRef::Block(block) => {
            if index
                .finder_set(block.finders())
                .iter()
                .all(|f| complete.contains(f))
            {
                members
                    .entry(BlockAt {
                        mount: mount.clone(),
                        site: path.to_owned(),
                        finders: block.finders(),
                        family: block.family.clone(),
                    })
                    .or_default();
            }
        }
    });
    for listing in round {
        for listed in listing.listed {
            if let Some(block) = covering(index, listing.finder, listed)
                && let Some(of_block) = members.get_mut(&block)
            {
                of_block.insert(listed.path.clone(), listed.found);
            }
        }
    }
    for (mount, path, finder) in lost {
        match index.drop_finder(&mount, &path, &finder) {
            crate::index::FinderDropped::Kept => done.finders_dropped += 1,
            crate::index::FinderDropped::Evicted => done.singles_evicted += 1,
            crate::index::FinderDropped::Missing => {}
        }
    }
    for (block, found) in members {
        let one_finder = index.finder_set(block.finders).len() == 1;
        if one_finder && !found.is_empty() {
            let as_singles: Vec<(PathBuf, Discovered)> = found.clone().into_iter().collect();
            if index.dissolve(
                &block.mount,
                &block.site,
                block.finders,
                &block.family,
                &as_singles,
            ) == Dissolved::Done
            {
                done.blocks_dissolved += 1;
                continue;
            }
        }
        let listed = u64::try_from(found.len()).expect("a count fits u64");
        match index.recount_block(
            &block.mount,
            &block.site,
            block.finders,
            &block.family,
            listed,
        ) {
            crate::index::Recounted::Changed { .. } => done.blocks_recounted += 1,
            crate::index::Recounted::Evicted => done.blocks_evicted += 1,
            crate::index::Recounted::Same | crate::index::Recounted::Missing => {}
        }
    }
    done
}

/// The shallowest block that stands for `listed`, found by `finder`: one
/// of a set holding the finder, sited at a directory above it, of family
/// `any`, or of its name family when directly beneath the site.
fn covering(index: &Index, finder: &Finder, listed: &Listed) -> Option<BlockAt> {
    let rel = listed.path.strip_prefix(&listed.mount.mount_point).ok()?;
    let names: Vec<&std::ffi::OsStr> = rel.iter().collect();
    let last = names.last()?.as_encoded_bytes();
    let mut site = listed.mount.mount_point.clone();
    for (depth, name) in names.iter().enumerate() {
        let below = names.len() - depth;
        for block in index.blocks_at(&listed.mount, &site) {
            let stands = index.finder_set(block.finders()).contains(finder)
                && match &block.family {
                    Family::Any => true,
                    Family::Name(family) => below == 1 && **family == *name_family(last),
                };
            if stands {
                return Some(BlockAt {
                    mount: listed.mount.clone(),
                    site,
                    finders: block.finders(),
                    family: block.family.clone(),
                });
            }
        }
        site.push(name);
    }
    None
}
