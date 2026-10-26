//! Sizing a unit's tree (a re-tally, docs/architecture.md, The tally):
//! allocated bytes (each inode once), entries and the newest `mtime` or
//! `ctime`, by a walk that resumes across batches through a [`Cursor`].
//!
//! The cursor holds no file descriptor between steps, only names: the
//! names from the unit's root down to the directory being walked and, for
//! each directory on that path, its listing (sorted by bytes) and how far
//! the walk got in it. Each step reaches the root again from its mount (as
//! discovery does: no symlink followed, no mount crossed), checks the
//! root's identity, opens the directories on the cursor's path again and
//! continues in the kept listings. So a batch that pauses holds nothing
//! open while the tally sleeps (a reclaim's in-use check would see an open
//! directory inside a candidate), a directory is listed once per walk
//! (never again per batch: a huge flat directory costs one listing, not
//! one per batch), and entries vanishing between steps only end their
//! branch (`ENOENT` and the like). Entries created after their directory
//! was listed are missed: an estimate. The kept listings are counted
//! against the index's budget with the cursor ([`Cursor::counted_bytes`]).
//!
//! **Progress.** A step that paused before doing any entry (reaching the
//! root and reopening the cursor's path took its whole batch) marks the
//! cursor stalled; the next step then does at least [`STEP_ENTRIES_MIN`]
//! entries whatever its batch's bounds say, so such a walk still ends
//! (its batches exceed the bounds by that much, every other batch). A
//! listing is charged as one operation per name.
//!
//! What is counted is an estimate for the order of work, never a check:
//! a mount point beneath the root is neither statted nor entered (it may be
//! a dead FUSE mount), nor is a directory of another device; directories
//! deeper than [`LEVELS_MAX`] below the root are counted but not entered,
//! and hard links are recognised for at most [`HARDLINKS_TRACKED_MAX`]
//! inodes (more are counted each time). A tree of more than the entry
//! budget, one holding a `systemd-private-*` name, or one with an entry
//! this user may not read (`EACCES`, `EPERM`) is [`Step::Refused`] (the
//! deletion would refuse it: `too_many_entries`, `contains_special`,
//! `unreadable`);
//! such a name is never statted, as the eligibility walk never stats it
//! (docs/safety.md, PrivateTmp).

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::mem::size_of;
use std::path::{Path, PathBuf};

use super::budget::Budget;
use crate::engine::{Reason, is_private_tmp};
use crate::host::{
    Access, Base, DirListing, Errno, FileType, Host, HostError, MountEntry, Stat, Timestamp,
};
use crate::index::collapse::{gone_or_error, means_gone};
use crate::index::memory::{btree_bytes, btree_node_bytes, heap, vec_bytes};
use crate::index::{Identity, MountKey};
use crate::resolve::{ResolveError, Resolved, Want, is_mount_point, open_beneath, resolve_in};

/// Directories below a unit's root the walk enters (each holds an fd
/// during a step). Deeper ones are counted, not entered.
pub const LEVELS_MAX: usize = 64;

/// Entries a step after a stalled one does at least (see the module doc,
/// Progress).
pub const STEP_ENTRIES_MIN: usize = 32;

/// Inodes with several links remembered per walk, to count each once
/// (8 bytes each in a B-tree: about 200 KiB at most).
pub const HARDLINKS_TRACKED_MAX: usize = 16_384;

/// What a sized tree came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sized {
    /// Allocated bytes, each inode with several links counted once.
    pub est_bytes: u64,
    /// Entries, the root included.
    pub files: u64,
    /// The newest `mtime` or `ctime` seen; `None` if none was reported.
    pub newest_change: Option<Timestamp>,
}

/// What one step of a walk came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The budget ended first; the cursor resumes where it stopped.
    Paused,
    Done(Sized),
    /// The unit's root is gone, replaced, or a mount point now.
    Gone,
    /// The deletion would refuse the tree as it is: more entries than the
    /// walk's entry budget (`too_many_entries`), a `systemd-private-*`
    /// name inside (`contains_special`; never statted or entered, as the
    /// eligibility walk never does), or an entry this user may not read
    /// (`unreadable`).
    Refused(Reason),
}

/// A directory on the cursor's path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Level {
    /// Its name in its parent (`None`: the unit's root).
    name: Option<OsString>,
    /// Its entries, sorted by bytes, once listed.
    names: Option<Vec<OsString>>,
    /// How many of `names` are done.
    next: usize,
}

/// A resumable walk of one unit's tree (see the module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    mount: MountKey,
    root: PathBuf,
    identity: Identity,
    /// Empty until the root was counted; then the root and the directories
    /// below it the walk is in.
    levels: Vec<Level>,
    entries: u64,
    bytes: u64,
    newest: Option<Timestamp>,
    hardlinks: BTreeSet<u64>,
    /// The last step paused before doing any entry.
    stalled: bool,
}

impl Cursor {
    /// A walk of the unit at `root` (absolute, on `mount`), recorded with
    /// `identity`.
    pub fn new(mount: MountKey, root: PathBuf, identity: Identity) -> Self {
        assert!(
            root.starts_with(&mount.mount_point) && root != mount.mount_point,
            "a unit {} not strictly below its mount {}",
            root.display(),
            mount.mount_point.display()
        );
        Self {
            mount,
            root,
            identity,
            levels: Vec::new(),
            entries: 0,
            bytes: 0,
            newest: None,
            hardlinks: BTreeSet::new(),
            stalled: false,
        }
    }

    pub fn mount(&self) -> &MountKey {
        &self.mount
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn identity(&self) -> Identity {
        self.identity
    }

    /// An upper bound of the heap the cursor owns (docs/architecture.md:
    /// live cursors count against the index's budget), as the index counts
    /// its own ([`crate::index::memory`]).
    pub fn counted_bytes(&self) -> usize {
        let name = |n: &Option<OsString>| n.as_ref().map_or(0, |n| heap(n.capacity()));
        heap(self.root.capacity())
            + heap(self.mount.mount_point.capacity())
            + vec_bytes(self.levels.capacity(), size_of::<Level>())
            + self
                .levels
                .iter()
                .map(|l| {
                    name(&l.name)
                        + l.names.as_ref().map_or(0, |names| {
                            vec_bytes(names.capacity(), size_of::<OsString>())
                                + names.iter().map(|n| heap(n.capacity())).sum::<usize>()
                        })
                })
                .sum::<usize>()
            + btree_bytes(self.hardlinks.len(), btree_node_bytes(size_of::<u64>(), 0))
    }

    /// Walks on within `budget` (but see the module doc, Progress), at most
    /// `entries_max` entries in all. An error is a failure of the host
    /// (`EIO`, `EMFILE`): the cursor may have counted part of the step and
    /// must be dropped, not resumed.
    pub fn step<H: Host>(
        &mut self,
        host: &H,
        mounts: &[MountEntry],
        budget: &mut Budget,
        entries_max: usize,
    ) -> Result<Step, HostError> {
        if budget.spent(host) {
            return Ok(Step::Paused);
        }
        let Some((parent, stat)) = reach(host, mounts, &self.root, self.identity, budget)? else {
            return Ok(Step::Gone);
        };
        if self.levels.is_empty() {
            if self.count(&stat) > entries_max {
                return Ok(Step::Refused(Reason::TooManyEntries));
            }
            if stat.file_type != Some(FileType::Directory) {
                return Ok(Step::Done(self.sized()));
            }
            self.levels.push(Level {
                name: None,
                names: None,
                next: 0,
            });
        }
        let name = self.root.file_name().expect("a unit has a name");
        budget.take(1);
        let root_fd = match open_beneath(
            host,
            &parent.fd,
            parent.mount.id,
            Path::new(name),
            Access::Path,
            Want::Directory,
        ) {
            Ok((fd, _)) => fd,
            Err(e) => return gone_or_error(e).map(|()| Step::Gone),
        };
        let mount_points = mount_points_beneath(mounts, &self.mount, &self.root);
        let walk = Walk {
            host,
            mount_id: parent.mount.id,
            mount_points: &mount_points,
            entries_max,
        };
        walk.run(self, root_fd, budget)
    }

    /// Counts `stat`; returns the entries counted so far.
    fn count(&mut self, stat: &Stat) -> usize {
        self.entries += 1;
        let once = match (stat.file_type, stat.nlink, stat.ino) {
            (Some(t), Some(n), Some(ino)) if t != FileType::Directory && n > 1 => {
                if self.hardlinks.contains(&ino) {
                    false
                } else {
                    if self.hardlinks.len() < HARDLINKS_TRACKED_MAX {
                        self.hardlinks.insert(ino);
                    }
                    true
                }
            }
            _ => true,
        };
        if once {
            let bytes = stat.blocks.unwrap_or(0).saturating_mul(512);
            self.bytes = self.bytes.saturating_add(bytes);
        }
        if let Some(changed) = stat.mtime.zip(stat.ctime).map(|(m, c)| m.max(c)) {
            self.newest = Some(self.newest.map_or(changed, |n| n.max(changed)));
        }
        usize::try_from(self.entries).unwrap_or(usize::MAX)
    }

    fn sized(&self) -> Sized {
        Sized {
            est_bytes: self.bytes,
            files: self.entries,
            newest_change: self.newest,
        }
    }

    /// The path of the directory at `depth` on the cursor's path.
    fn dir_path(&self, depth: usize) -> PathBuf {
        let mut path = self.root.clone();
        for level in &self.levels[1..=depth] {
            path.push(
                level
                    .name
                    .as_ref()
                    .expect("a level below the root has a name"),
            );
        }
        path
    }
}

/// A unit reached: its parent directory, and its own `statx`.
pub(super) type Reached<F> = (Resolved<F>, Stat);

/// Reaches the unit at `root` (its parent from the mount, then a `statx`
/// of its name) and checks it has `identity`. `None`: gone, replaced, or
/// a mount point now (never statted).
pub(super) fn reach<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    root: &Path,
    identity: Identity,
    budget: &mut Budget,
) -> Result<Option<Reached<H::Fd>>, HostError> {
    let (Some(parent_path), Some(name)) = (root.parent(), root.file_name()) else {
        return Ok(None);
    };
    if is_mount_point(mounts, root) {
        return Ok(None);
    }
    budget.take(parent_path.components().count() + 1);
    let parent = match resolve_in(host, mounts, parent_path, Want::Directory) {
        Ok(parent) => parent,
        Err(e) => return gone_or_error(e).map(|()| None),
    };
    let Some(parent_ino) = parent.stat.ino else {
        return Ok(None);
    };
    budget.take(1);
    let stat = match host.statx(Base::Dir(&parent.fd), Path::new(name)) {
        Ok(stat) => stat,
        Err(e) if means_gone(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    if Identity::of(&stat, parent_ino) != Some(identity) {
        return Ok(None);
    }
    Ok(Some((parent, stat)))
}

/// Whether a failed call inside a unit means this user may not read it:
/// the deletion would refuse the unit (`unreadable`, fail closed,
/// docs/safety.md), so the estimate is a refusal, never an empty tree.
fn unreadable(e: &HostError) -> bool {
    e.is(Errno::EACCES) || e.is(Errno::EPERM)
}

/// Why a directory inside a unit could not be opened.
enum Opened {
    Unreadable,
    Failed(HostError),
}

/// Mount points strictly beneath `root` (but the unit's own mount).
fn mount_points_beneath(mounts: &[MountEntry], key: &MountKey, root: &Path) -> BTreeSet<PathBuf> {
    mounts
        .iter()
        .map(|m| &m.mount_point)
        .filter(|p| p.starts_with(root) && **p != root && **p != key.mount_point)
        .cloned()
        .collect()
}

struct Walk<'a, H: Host> {
    host: &'a H,
    mount_id: u64,
    mount_points: &'a BTreeSet<PathBuf>,
    entries_max: usize,
}

impl<H: Host> Walk<'_, H> {
    fn run(
        &self,
        cursor: &mut Cursor,
        root_fd: H::Fd,
        budget: &mut Budget,
    ) -> Result<Step, HostError> {
        let mut open: Vec<H::Fd> = vec![root_fd];
        // Open the cursor's path again (bounded by LEVELS_MAX).
        while open.len() < cursor.levels.len() {
            let depth = open.len();
            let name = cursor.levels[depth].name.clone().expect("named");
            budget.take(1);
            let opened = match self.open_dir(&open[depth - 1], &name) {
                Err(Opened::Unreadable) => return Ok(Step::Refused(Reason::Unreadable)),
                Err(Opened::Failed(e)) => return Err(e),
                Ok(opened) => opened,
            };
            match opened {
                Some(fd) => open.push(fd),
                // Gone since the last step: its branch is done.
                None => cursor.levels.truncate(depth),
            }
        }
        // Entries done in this step; after a stalled step at least
        // STEP_ENTRIES_MIN, whatever the budget says (the module doc,
        // Progress).
        let min = if cursor.stalled { STEP_ENTRIES_MIN } else { 0 };
        let mut done = 0usize;
        let stop = |budget: &Budget, done: usize| done >= min && budget.spent(self.host);
        let step = self.walk(cursor, &mut open, budget, &mut done, &stop);
        cursor.stalled = matches!(step, Ok(Step::Paused)) && done == 0;
        step
    }

    fn walk(
        &self,
        cursor: &mut Cursor,
        open: &mut Vec<H::Fd>,
        budget: &mut Budget,
        done: &mut usize,
        stop: &dyn Fn(&Budget, usize) -> bool,
    ) -> Result<Step, HostError> {
        loop {
            let depth = open.len() - 1;
            if cursor.levels[depth].names.is_none() {
                if stop(budget, *done) {
                    return Ok(Step::Paused);
                }
                let left = self.entries_max.saturating_sub(cursor.entries as usize);
                let mut names = match self.host.read_dir(&open[depth], left) {
                    Ok(DirListing::Names(names)) => names,
                    Ok(DirListing::TooMany) => return Ok(Step::Refused(Reason::TooManyEntries)),
                    Err(e) if unreadable(&e) => return Ok(Step::Refused(Reason::Unreadable)),
                    Err(e) if means_gone(&e) => Vec::new(),
                    Err(e) => return Err(e),
                };
                budget.take(names.len().max(1));
                names.sort_unstable_by(|a, b| a.as_encoded_bytes().cmp(b.as_encoded_bytes()));
                cursor.levels[depth].names = Some(names);
            }
            let level = &cursor.levels[depth];
            let Some(name) = level
                .names
                .as_ref()
                .expect("listed")
                .get(level.next)
                .cloned()
            else {
                // This directory is done.
                open.pop();
                cursor.levels.pop();
                if cursor.levels.is_empty() {
                    return Ok(Step::Done(cursor.sized()));
                }
                continue;
            };
            if is_private_tmp(&name) {
                return Ok(Step::Refused(Reason::ContainsSpecial));
            }
            let path = cursor.dir_path(depth).join(&name);
            if self.mount_points.contains(&path) {
                cursor.levels[depth].next += 1;
                continue;
            }
            if stop(budget, *done) {
                return Ok(Step::Paused);
            }
            budget.take(1);
            *done += 1;
            let stat = match self.host.statx(Base::Dir(&open[depth]), Path::new(&name)) {
                Ok(stat) => stat,
                Err(e) if unreadable(&e) => return Ok(Step::Refused(Reason::Unreadable)),
                Err(e) if means_gone(&e) => {
                    cursor.levels[depth].next += 1;
                    continue;
                }
                Err(e) => return Err(e),
            };
            cursor.levels[depth].next += 1;
            if cursor.count(&stat) > self.entries_max {
                return Ok(Step::Refused(Reason::TooManyEntries));
            }
            let enter = stat.file_type == Some(FileType::Directory)
                && stat.dev == cursor.identity.dev
                && cursor.levels.len() < LEVELS_MAX;
            if !enter {
                continue;
            }
            // Counted: from here on it is on the cursor's path, so a pause
            // before it is listed resumes inside it, not at it again.
            budget.take(1);
            // Gone since its statx: counted, nothing below.
            let opened = match self.open_dir(&open[depth], &name) {
                Err(Opened::Unreadable) => return Ok(Step::Refused(Reason::Unreadable)),
                Err(Opened::Failed(e)) => return Err(e),
                Ok(opened) => opened,
            };
            if let Some(fd) = opened {
                open.push(fd);
                cursor.levels.push(Level {
                    name: Some(name),
                    names: None,
                    next: 0,
                });
            }
        }
    }

    /// Opens the directory `name` below `dir`; `None` if it is gone or not
    /// a directory of this mount any more.
    fn open_dir(&self, dir: &H::Fd, name: &OsStr) -> Result<Option<H::Fd>, Opened> {
        match open_beneath(
            self.host,
            dir,
            self.mount_id,
            Path::new(name),
            Access::Path,
            Want::Directory,
        ) {
            Ok((fd, _)) => Ok(Some(fd)),
            Err(ResolveError::Host(e)) if unreadable(&e) => Err(Opened::Unreadable),
            Err(e) => gone_or_error(e).map(|()| None).map_err(Opened::Failed),
        }
    }
}
