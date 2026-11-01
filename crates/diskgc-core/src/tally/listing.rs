//! Discovery for the index (docs/architecture.md, The tally): one source
//! (a profile bound to one root) listed by its kind's own discovery
//! ([`crate::fs_kind::discover`], [`crate::cargo_target::discover`]), the
//! same code the reclaim walk runs, and what that listing means for the
//! index:
//!
//! - every candidate found is inserted (collapsing first at the ceiling);
//! - only a **complete** listing (no stop, nothing on the way refused but
//!   container storage, which is excluded on purpose, never unread) is
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
use crate::engine::{self, Seen, Work};
use crate::fs_kind::{self, Find};
use crate::host::{Host, HostError, MonoTime, MountEntry, Timestamp};
use crate::index::{Birth, Discovered, Finder, Identity, MountKey};
use crate::outcome::Stop;
use crate::places::mount_for;

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
            let read_all = discovery.nothing_refused_but_storage();
            for (path, candidate) in discovery.candidates {
                match candidate {
                    Ok(f) => found.push((f.path, f.identity, f.seen, f.found_at)),
                    Err(_) => {
                        listing.unindexed.insert(path);
                    }
                }
            }
            listing.complete = stopped.is_none() && read_all;
            listing.stopped = stopped;
        }
        SourceFind::CargoTarget => match cargo_target::discover(host, &roots, deadline) {
            Ok(discovery) => {
                listing.complete = discovery.nothing_refused_but_storage();
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

/// The index's identity of a candidate discovery found (docs/architecture.md,
/// The index: Identity): its birth time, else the inode of the directory it
/// was found in; `None` if neither was reported. The tally stores it, and
/// the reclaim walk compares a unit's stored one with it (`stale`).
pub fn index_identity(found: engine::Identity, seen: Seen) -> Option<Identity> {
    let birth = match (found.btime, seen.parent_ino) {
        (Some(t), _) => Birth::Time(t),
        (None, Some(parent)) => Birth::ParentIno(parent),
        (None, None) => return None,
    };
    Some(Identity {
        dev: found.id.dev,
        ino: found.id.ino,
        birth,
    })
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
    Some(Listed {
        path: path.to_owned(),
        mount,
        found: Discovered {
            identity: index_identity(found, seen)?,
            // Unknown: as if just changed, so it is ordered last, never
            // first, by `oldest`.
            newest_change: seen.changed.unwrap_or(now_wall),
            checked_at: at,
        },
    })
}

/// Operations a listing step does at least after a stalled one (one that
/// paused before doing any: reaching the root and reopening the way down
/// took its whole batch), as the sizing cursor does.
pub const LISTING_STEP_MIN: usize = 32;

/// A resumable listing of one source (the background tally's; Mark,
/// 2026-10-25): its kind's own discovery, paused between batches by the
/// batch's bounds, holding names but no file descriptor
/// ([`fs_kind::DiscoverCursor`], [`cargo_target::DiscoverCursor`]). The
/// round is **complete** when the walk reached its end and nothing on the
/// way was refused, across all its steps.
#[derive(Debug, Clone)]
pub struct ListingCursor {
    finder: Finder,
    walk: Walker,
    /// When the round started: singles of this finder not found since are
    /// lost when the round completes ([`super::recount::apply_round`]).
    started: MonoTime,
    /// Nothing refused so far.
    clean: bool,
    stalled: bool,
}

#[derive(Debug, Clone)]
enum Walker {
    Fs(fs_kind::DiscoverCursor),
    Cargo(cargo_target::DiscoverCursor),
}

/// What one step of a [`ListingCursor`] found.
#[derive(Debug, Default)]
pub struct ListingStep {
    pub listed: Vec<Listed>,
    pub unindexed: BTreeSet<PathBuf>,
    /// The walk reached its end.
    pub done: bool,
    /// The whole round, if done: no refusal in any step.
    pub complete: bool,
}

impl ListingCursor {
    /// A round of `source`, started at `now`.
    pub fn new(source: &TallySource, now: MonoTime) -> Self {
        let root = source.finder.root.to_path_buf();
        let walk = match &source.find {
            SourceFind::Fs(_) => Walker::Fs(fs_kind::DiscoverCursor::new(root, false)),
            SourceFind::CargoTarget => Walker::Cargo(cargo_target::DiscoverCursor::new(root)),
        };
        Self {
            finder: source.finder.clone(),
            walk,
            started: now,
            clean: true,
            stalled: false,
        }
    }

    pub fn finder(&self) -> &Finder {
        &self.finder
    }

    pub fn started(&self) -> MonoTime {
        self.started
    }

    /// An upper bound of the heap it owns (counted against the index's
    /// budget with the sizing cursor).
    pub fn counted_bytes(&self) -> usize {
        match &self.walk {
            Walker::Fs(c) => c.counted_bytes(),
            Walker::Cargo(c) => c.counted_bytes(),
        }
    }

    /// Walks on within `budget` (after a stalled step, at least
    /// [`LISTING_STEP_MIN`] operations whatever it says). `source` must be
    /// the one it was made for. An error is a failure of the host: the
    /// round must be dropped.
    pub fn step<H: Host>(
        &mut self,
        host: &H,
        source: &TallySource,
        budget: &mut super::budget::Budget,
    ) -> Result<ListingStep, HostError> {
        assert_eq!(
            source.finder, self.finder,
            "a listing cursor of another source"
        );
        let min = if self.stalled { LISTING_STEP_MIN } else { 0 };
        // Operations of progress this step (reopening is not progress).
        let mut done_ops = 0usize;
        let mut pause = |work: Work| match work {
            Work::Reopen => {
                budget.take(1);
                false
            }
            Work::Listed(n) => {
                budget.take(n);
                false
            }
            Work::Next(ops) => {
                if done_ops >= min && budget.spent(host) {
                    return true;
                }
                budget.take(ops);
                done_ops += ops;
                false
            }
        };
        let mut out = fs_kind::Discovery::default();
        let mut cargo = cargo_target::Discovery::default();
        let stepped = match (&mut self.walk, &source.find) {
            (Walker::Fs(cursor), SourceFind::Fs(find)) => cursor
                .step(host, find, &mut out, &mut pause)
                .map(|s| matches!(s, fs_kind::Stepped::Done)),
            (Walker::Cargo(cursor), SourceFind::CargoTarget) => cursor
                .step(host, &mut cargo, &mut pause)
                .map(|s| matches!(s, cargo_target::Stepped::Done)),
            _ => unreachable!("the walker is made from the source's kind"),
        };
        let done = match stepped {
            Ok(done) => done,
            Err(Stop::Failed { error, .. }) => return Err(error),
            Err(stop) => {
                return Err(HostError::invalid(
                    "list",
                    source.finder.root.display(),
                    stop.to_string(),
                ));
            }
        };
        self.stalled = !done && done_ops == 0;
        let mut found: Vec<(PathBuf, engine::Identity, Seen, MonoTime)> = Vec::new();
        let mut step = ListingStep::default();
        let read_all = out.nothing_refused_but_storage() && cargo.nothing_refused_but_storage();
        for (path, candidate) in out.candidates {
            match candidate {
                Ok(f) => found.push((f.path, f.identity, f.seen, f.found_at)),
                Err(_) => {
                    step.unindexed.insert(path);
                }
            }
        }
        let now = host.now();
        for target in cargo.targets {
            found.push((target.path, target.identity, target.seen, now));
        }
        if !read_all {
            self.clean = false;
        }
        if !found.is_empty() {
            let mounts = host.mountinfo()?;
            let now_wall = host.wall_clock();
            for (path, identity, seen, at) in found {
                match indexed(&mounts, &path, identity, seen, at, now_wall) {
                    Some(listed) => step.listed.push(listed),
                    None => {
                        step.unindexed.insert(path);
                    }
                }
            }
        }
        step.done = done;
        step.complete = done && self.clean;
        Ok(step)
    }
}
