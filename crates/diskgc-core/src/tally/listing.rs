//! Discovery for the index (docs/architecture.md, The tally): one source
//! (a profile bound to one root) listed by its kind's own discovery
//! ([`crate::fs_kind::discover`], [`crate::cargo_target::discover`]), the
//! same code the reclaim walk runs, and what that listing means for the
//! index:
//!
//! - every candidate found is inserted (collapsing first at the ceiling);
//! - only a **complete** listing (no stop, nothing on the way refused) is
//!   evidence of absence: a single of this finder below the root that it no
//!   longer finds loses the finder (and goes with its last one), and the
//!   blocks it stands for are recounted or dissolved
//!   ([`super::recount`]); a partial listing only adds.
//!
//! A candidate whose identity the index cannot form (no inode, or no birth
//! time and no parent inode) is not inserted; discovery found it, so it is
//! not evicted either, and the reclaim walk's own discovery finds it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::cargo_target;
use crate::engine::{self, Seen};
use crate::fs_kind::{self, Find};
use crate::host::{Host, HostError, MonoTime, MountEntry, Timestamp};
use crate::index::{Birth, Discovered, Finder, Identity, MountKey};
use crate::outcome::Stop;
use crate::resolve::mount_for;

/// What finds a source's candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceFind {
    Fs(Box<Find>),
    CargoTarget,
}

/// A source profile bound to one root: what the tally lists, and who the
/// index records as having found its candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TallySource {
    pub finder: Finder,
    pub find: SourceFind,
}

/// A candidate a listing found, as the index takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub path: PathBuf,
    pub mount: MountKey,
    pub found: Discovered,
}

/// What listing one source found.
#[derive(Debug, Default)]
pub struct Listing {
    /// No stop, and nothing on the way (the root, a directory, a candidate's
    /// own `where`) refused: evidence of absence for what it did not find.
    pub complete: bool,
    pub listed: Vec<Listed>,
    /// Candidates found that the index cannot take (refused by discovery
    /// itself, or without an identity it can form): never evicted for not
    /// being listed.
    pub unindexed: BTreeSet<PathBuf>,
    /// Why it stopped, if it did (the deadline, a failure of the host).
    pub stopped: Option<Stop>,
}

/// Lists `source` (see the module doc), within `deadline` (`None`: none).
pub fn list<H: Host>(
    host: &H,
    source: &TallySource,
    deadline: Option<MonoTime>,
) -> Result<Listing, HostError> {
    let roots = [source.finder.root.to_path_buf()];
    let mut listing = Listing::default();
    let mut found: Vec<(PathBuf, engine::Identity, Seen, MonoTime)> = Vec::new();
    match &source.find {
        SourceFind::Fs(find) => {
            let (discovery, stopped) = fs_kind::discover(host, &roots, find, deadline);
            for (path, candidate) in discovery.candidates {
                match candidate {
                    Ok(f) => found.push((f.path, f.identity, f.seen, f.found_at)),
                    Err(_) => {
                        listing.unindexed.insert(path);
                    }
                }
            }
            listing.complete = stopped.is_none() && discovery.refused.is_empty();
            listing.stopped = stopped;
        }
        SourceFind::CargoTarget => match cargo_target::discover(host, &roots, deadline) {
            Ok(discovery) => {
                listing.complete = discovery.refused.is_empty();
                for target in discovery.targets {
                    found.push((target.path, target.identity, target.seen, host.now()));
                }
            }
            Err(stop) => listing.stopped = Some(stop),
        },
    }
    if found.is_empty() {
        return Ok(listing);
    }
    let mounts = host.mountinfo()?;
    let now_wall = host.wall_clock();
    for (path, identity, seen, at) in found {
        match indexed(&mounts, &path, identity, seen, at, now_wall) {
            Some(listed) => listing.listed.push(listed),
            None => {
                listing.unindexed.insert(path);
            }
        }
    }
    Ok(listing)
}

/// The index's view of a found candidate; `None` if it cannot form one.
fn indexed(
    mounts: &[MountEntry],
    path: &std::path::Path,
    found: engine::Identity,
    seen: Seen,
    at: MonoTime,
    now_wall: Timestamp,
) -> Option<Listed> {
    let mount = MountKey::of(mount_for(mounts, path).ok()?);
    if mount.dev != found.id.dev || path == mount.mount_point {
        return None;
    }
    let birth = match (found.btime, seen.parent_ino) {
        (Some(t), _) => Birth::Time(t),
        (None, Some(parent)) => Birth::ParentIno(parent),
        (None, None) => return None,
    };
    Some(Listed {
        path: path.to_owned(),
        mount,
        found: Discovered {
            identity: Identity {
                dev: found.id.dev,
                ino: found.id.ino,
                birth,
            },
            // Unknown: as if just changed, so it is ordered last, never
            // first, by `oldest`.
            newest_change: seen.changed.unwrap_or(now_wall),
            checked_at: at,
        },
    })
}
