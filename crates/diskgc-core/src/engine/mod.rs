//! The deletion engine for filesystem candidates (docs/safety.md, Filesystem
//! candidates: the eligibility walk and Deletion).
//!
//! Two steps, composed by the caller with whatever goes between them (the
//! candidate's `flock`, the in-use check over [`WalkRecord::objects`], the
//! liveness of any socket or FIFO in [`WalkRecord::ipc_entries`]):
//!
//! 1. [`walk`]: the eligibility walk. It reads the candidate's whole tree
//!    fd-relative and either refuses the candidate with a [`Skip`] reason or
//!    returns a [`WalkRecord`]: every entry's name, [`Identity`] and
//!    allocated size.
//! 2. [`delete`]: deletes exactly the entries of the record, depth first.
//!    Every directory is reopened relative to its parent by its recorded
//!    name and matched against its recorded identity; every `unlinkat` comes
//!    right after a `statx` of the same name whose identity matches the
//!    record (`checked_unlink`, the only caller of `Delete::unlinkat`). A
//!    mismatch anywhere stops the delete: the candidate is `changed`.
//!
//! **Shape of the traversal.** Both steps are loops over an explicit stack
//! of directory frames, never recursion, so a tree deeper than `PATH_MAX`
//! (or than the thread's stack) is no problem: no full path is ever built
//! for the kernel, only single names below an open directory. At most
//! [`FD_WINDOW`] frames below the candidate hold an fd, plus the candidate
//! itself (held for the whole step, so a candidate renamed mid-delete is
//! still the object deleted). When a frame without an fd is needed again
//! (going back up past the window), it is reopened from its nearest open
//! ancestor, one name at a time, each matched against the record; `..` is
//! never used. The cost is quadratic in the depth of a single chain and
//! nothing for trees shallower than the window.
//!
//! **Order.** Each directory's names are sorted (bytes), so the call order,
//! and with it every call position of the test driver and the model, is the
//! same on every filesystem and run, and the same on `ModelHost` and
//! `LinuxHost`.
//!
//! **Bounds.** A candidate with more than [`Rules::max_entries`] entries is
//! skipped (`too_many_entries`) as soon as a listing would exceed what is
//! left of that budget: the record and the listings stay bounded in memory,
//! and a writer filling a directory faster than it is read cannot keep the
//! walk going.

mod delete;
mod frames;
mod walk;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::host::{DevIno, FileType, Host, MonoTime, Stat, Timestamp};
use crate::resolve::Resolved;

pub use delete::{DeleteOptions, Deletion, End, delete};
pub(crate) use walk::host_failure;
pub use walk::{
    CACHEDIR_TAG, CARGO_LOCK_NAMES, KEEP_MARKER, Rules, WalkStop, is_cargo_lock_name, walk,
};

/// Frames below the candidate that hold an fd at the same time (see the
/// module docs). A frame's fd is closed before the next one is opened, so
/// with the candidate's, its parent's and the one fd a listing opens for
/// itself, the engine holds at most `FD_WINDOW + 3` fds, whatever the depth
/// of the tree.
pub const FD_WINDOW: usize = 16;

/// The default for [`Rules::max_entries`]. The record costs about 100 bytes
/// per entry plus its name, so this bounds it at a few hundred MB; a larger
/// candidate is skipped (`too_many_entries`) rather than risking the
/// process's memory.
pub const DEFAULT_MAX_ENTRIES: usize = 2_000_000;

/// A filesystem candidate as the engine reaches it: by its name in its
/// parent directory, which the caller resolved. The path is for reports
/// only; the engine never uses it to reach anything.
#[derive(Debug)]
pub struct Candidate<'a, F> {
    parent: &'a F,
    mount_id: u64,
    /// The filesystem type of the candidate's mount (mountinfo).
    fs_type: &'a str,
    path: &'a Path,
    name: &'a OsStr,
}

impl<'a, F> Candidate<'a, F> {
    /// The candidate at `path`, whose parent directory is `parent` (as
    /// [`crate::resolve::resolve`] reached it: its fd and mount come
    /// together). `None` when `path` has no last name (`/`, or a path ending
    /// in `..`).
    pub fn new(parent: &'a Resolved<F>, path: &'a Path) -> Option<Self> {
        let name = path.file_name()?;
        crate::host::is_single_name(name).then_some(Self {
            parent: &parent.fd,
            mount_id: parent.mount.id,
            fs_type: &parent.mount.fs_type,
            path,
            name,
        })
    }

    pub fn path(&self) -> &Path {
        self.path
    }

    pub fn name(&self) -> &OsStr {
        self.name
    }

    pub fn mount_id(&self) -> u64 {
        self.mount_id
    }

    /// The filesystem type of the mount the candidate is on (the walk never
    /// leaves it).
    pub fn fs_type(&self) -> &str {
        self.fs_type
    }

    /// The parent directory's fd (for opening the candidate below it).
    pub fn parent(&self) -> &F {
        self.parent
    }
}

/// Filesystem types diskgc never takes candidates from: network
/// filesystems (docs/safety.md, Before the walk: `unsupported_fs`). Their
/// locks are not those of the local kernel (Cargo does not lock there), and
/// a dead server can hang any operation on them. Compared with mountinfo's
/// type in full; a FUSE type also by the name after `fuse.`.
pub const NETWORK_FS_TYPES: &[&str] = &[
    "9p",
    "afs",
    "ceph",
    "cifs",
    "coda",
    "davfs",
    "gcsfuse",
    "glusterfs",
    "lustre",
    "ncpfs",
    "nfs",
    "nfs4",
    "rclone",
    "s3fs",
    "smb3",
    "smbfs",
    "sshfs",
];

/// Whether mountinfo's filesystem type `fs_type` is a network filesystem
/// ([`NETWORK_FS_TYPES`], directly or as `fuse.<type>`).
pub fn is_network_fs(fs_type: &str) -> bool {
    let base = fs_type.strip_prefix("fuse.").unwrap_or(fs_type);
    NETWORK_FS_TYPES.contains(&base)
}

/// What identifies an entry between the walk and its unlink, compared in
/// full before every `unlinkat` and at every reopening of a directory
/// (docs/safety.md, Deletion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub id: DevIno,
    pub uid: u32,
    pub file_type: FileType,
    /// Birth time, where the filesystem reports one: an object removed and
    /// recreated with the same inode number, owner and type differs here.
    pub btime: Option<Timestamp>,
}

impl Identity {
    /// The identity in `stat`; `None` when the kernel did not report the
    /// inode, owner or type (a check it cannot answer).
    pub fn of(stat: &Stat) -> Option<Self> {
        Some(Self {
            id: stat.dev_ino()?,
            uid: stat.uid?,
            file_type: stat.file_type?,
            btime: stat.btime,
        })
    }
}

/// One entry the walk checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: OsString,
    pub identity: Identity,
    /// Allocated bytes (`stx_blocks` × 512; 0 if not reported). Each name
    /// counts its object's blocks, so hard links are counted per name.
    pub bytes: u64,
    /// For a directory: its listing in [`WalkRecord`] (index into its
    /// directory table). Some exactly when `identity.file_type` is a
    /// directory.
    listing: Option<usize>,
}

/// The kind of an IPC entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcKind {
    Socket,
    Fifo,
}

/// A socket or FIFO the walk recorded (only when [`Rules::block_ipc`] is
/// off): the caller decides whether it is live (docs/safety.md, the
/// eligibility walk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcEntry {
    pub kind: IpcKind,
    pub id: DevIno,
    /// Its path relative to the candidate's (empty: the candidate itself).
    pub rel: PathBuf,
}

/// The objects the eligibility walk checked: what [`delete`] may remove,
/// and what the in-use check must look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkRecord {
    root: Entry,
    /// Every directory's entries, sorted by name; `listings[0]` is the
    /// candidate's when it is a directory.
    listings: Vec<Vec<Entry>>,
    entries: usize,
    bytes: u64,
    newest_change: Option<Timestamp>,
}

impl WalkRecord {
    /// The candidate itself.
    pub fn root(&self) -> &Entry {
        &self.root
    }

    /// How many entries, the candidate included.
    pub fn entries(&self) -> usize {
        self.entries
    }

    /// The allocated bytes of every entry, the candidate included (an
    /// estimate of what deleting frees; the measure is the observed change
    /// in available space).
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The newest mtime or ctime of any entry, for the idle filter; `None`
    /// if some entry's times were not reported (the caller must then treat
    /// the candidate as not idle).
    pub fn newest_change(&self) -> Option<Timestamp> {
        self.newest_change
    }

    /// Every entry, the candidate first.
    pub fn all_entries(&self) -> impl Iterator<Item = &Entry> + '_ {
        std::iter::once(&self.root).chain(self.listings.iter().flatten())
    }

    /// (device, inode) of every entry, the candidate first: what the in-use
    /// check matches processes against (docs/safety.md, In use).
    pub fn objects(&self) -> impl Iterator<Item = DevIno> + '_ {
        std::iter::once(self.root.identity.id)
            .chain(self.listings.iter().flatten().map(|e| e.identity.id))
    }

    /// Every socket and FIFO in the record, with its path below the
    /// candidate. Empty unless the walk ran with [`Rules::block_ipc`] off
    /// (otherwise the first one refused the candidate).
    pub fn ipc_entries(&self) -> Vec<IpcEntry> {
        let mut found = Vec::new();
        let mut todo = vec![(&self.root, PathBuf::new())];
        while let Some((entry, rel)) = todo.pop() {
            let kind = match entry.identity.file_type {
                FileType::Socket => Some(IpcKind::Socket),
                FileType::Fifo => Some(IpcKind::Fifo),
                _ => None,
            };
            if let Some(kind) = kind {
                found.push(IpcEntry {
                    kind,
                    id: entry.identity.id,
                    rel: rel.clone(),
                });
            }
            if let Some(listing) = entry.listing {
                for child in &self.listings[listing] {
                    todo.push((child, rel.join(&child.name)));
                }
            }
        }
        found
    }

    /// The path, relative to the candidate's (empty: the candidate itself),
    /// of the first entry in record order whose (device, inode) is `id`; for
    /// messages about a match.
    pub fn path_of(&self, id: DevIno) -> Option<PathBuf> {
        let mut todo = vec![(&self.root, PathBuf::new())];
        while let Some((entry, rel)) = todo.pop() {
            if entry.identity.id == id {
                return Some(rel);
            }
            if let Some(listing) = entry.listing {
                for child in self.listings[listing].iter().rev() {
                    todo.push((child, rel.join(&child.name)));
                }
            }
        }
        None
    }

    fn listing(&self, index: usize) -> &[Entry] {
        &self.listings[index]
    }

    /// The entry at `rel`: names below the candidate, one per level (empty:
    /// the candidate itself). `None` when the record holds no such entry.
    pub fn entry_at(&self, rel: &Path) -> Option<&Entry> {
        let mut entry = &self.root;
        for name in rel.components() {
            let std::path::Component::Normal(name) = name else {
                return None;
            };
            let listing = self.listings.get(entry.listing?)?;
            entry = listing.iter().find(|e| e.name == name)?;
        }
        Some(entry)
    }

    /// The entries of the directory at `rel` ([`WalkRecord::entry_at`]),
    /// sorted by name; `None` when there is no directory there.
    pub fn listing_at(&self, rel: &Path) -> Option<&[Entry]> {
        Some(self.listing(self.entry_at(rel)?.listing?))
    }

    /// Takes the entry at `rel` (at least one name below the candidate) and
    /// everything the walk recorded below it out of this record, as a
    /// record of its own whose candidate is that entry. Afterwards each
    /// record holds exactly its part of the walked tree, so both can be
    /// checked and deleted on their own, one after the other: the part
    /// first (below its parent directory, reached and matched by the
    /// caller), then the rest, whose delete then finds that directory as
    /// the walk left it minus the part (the Cargo protocol deletes the
    /// per-unit fingerprints first this way). Entry and byte counts move
    /// with the part; the newest change of both stays the one of the whole
    /// walk, an upper bound (the entries' own times are not kept). `None`,
    /// and the record unchanged, when there is no entry at `rel` or `rel`
    /// names the candidate itself.
    pub fn split_off(&mut self, rel: &Path) -> Option<WalkRecord> {
        let parent = rel.parent()?;
        let name = rel.file_name()?;
        let at = self.entry_at(parent)?.listing?;
        let position = self.listings[at].iter().position(|e| e.name == name)?;
        let mut root = self.listings[at].remove(position);
        // The part's listings, moved into a table of its own (renumbered,
        // in the order found); their slots here are left empty, so nothing
        // of the part is left in this record.
        let mut listings: Vec<Vec<Entry>> = Vec::new();
        let mut entries = 1;
        let mut bytes = root.bytes;
        // (index in the part's table, index in this record's table).
        let mut todo: Vec<(usize, usize)> = Vec::new();
        if let Some(old) = root.listing {
            listings.push(Vec::new());
            root.listing = Some(0);
            todo.push((0, old));
        }
        while let Some((new, old)) = todo.pop() {
            let mut moved = std::mem::take(&mut self.listings[old]);
            for child in &mut moved {
                entries += 1;
                bytes = bytes.saturating_add(child.bytes);
                if let Some(old_child) = child.listing {
                    listings.push(Vec::new());
                    let new_child = listings.len() - 1;
                    child.listing = Some(new_child);
                    todo.push((new_child, old_child));
                }
            }
            listings[new] = moved;
        }
        self.entries -= entries;
        self.bytes = self.bytes.saturating_sub(bytes);
        Some(WalkRecord {
            root,
            listings,
            entries,
            bytes,
            newest_change: self.newest_change,
        })
    }
}

/// Why a candidate is not deleted, by its skip reason (docs/reference.md,
/// Output).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The candidate itself is gone.
    Missing,
    /// An entry belongs to another user.
    NotOwner,
    /// A directory cannot be opened or listed, or an entry cannot be
    /// statted, for lack of permission (`EACCES`, `EPERM`; e.g. a mode-000
    /// directory): fail closed, never treated as empty.
    Unreadable,
    /// A `.diskgc-keep` file is inside.
    Kept,
    /// A Cargo lock file name anywhere, a Cargo target inside (the
    /// structural recogniser), or a device node or an object of unknown
    /// type.
    ContainsSpecial,
    /// A socket or FIFO is inside (with [`Rules::block_ipc`]).
    ContainsIpc,
    /// A mount is inside, or the candidate is one.
    ContainsMount,
    /// The kernel does not report a field a check needs.
    UnsupportedFs,
    /// The tree changed while it was being walked.
    Changed,
    /// More entries than [`Rules::max_entries`].
    TooManyEntries,
    /// A visible process uses one of the candidate's objects, names a path
    /// inside it in its initial environment, or could be using it (an
    /// unreadable process, an incomplete process scan): the in-use check
    /// (docs/safety.md, In use; [`crate::inuse`]).
    InUse,
    /// The candidate directory is `flock`ed by someone else.
    Locked,
    /// A path that cannot be reached safely: a symlink or mount crossing
    /// on the way, an odd mount table (docs/safety.md, Path traversal).
    UnsafePath,
    /// A Cargo target diskgc does not know how to reclaim: an unknown
    /// Cargo lock file name, a lock set it has not measured (a half of a
    /// split build directory), or a profile directory in no known layout
    /// (docs/safety.md, `cargo-target`).
    UnsupportedLayout,
    /// A Cargo target whose profile directories hold nothing but lock
    /// files: nothing to reclaim.
    Empty,
    /// Not idle for as long as the term's `idle_for` requires.
    NotIdle,
    /// Its last use lies in the future, so its age says nothing
    /// (docs/reference.md, Predicates).
    FutureTime,
    /// On another filesystem than the one that must have the space:
    /// deleting it would free nothing there (docs/architecture.md,
    /// Filesystem matching).
    OtherFs,
}

impl Reason {
    /// Every reason (keep it next to [`Reason::as_str`], whose match is
    /// exhaustive: a new reason is added to both).
    pub const ALL: [Reason; 18] = [
        Reason::Missing,
        Reason::NotOwner,
        Reason::Unreadable,
        Reason::Kept,
        Reason::ContainsSpecial,
        Reason::ContainsIpc,
        Reason::ContainsMount,
        Reason::UnsupportedFs,
        Reason::Changed,
        Reason::TooManyEntries,
        Reason::InUse,
        Reason::Locked,
        Reason::UnsafePath,
        Reason::UnsupportedLayout,
        Reason::Empty,
        Reason::NotIdle,
        Reason::FutureTime,
        Reason::OtherFs,
    ];

    /// The skip reason's name in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::NotOwner => "not_owner",
            Self::Unreadable => "unreadable",
            Self::Kept => "kept",
            Self::ContainsSpecial => "contains_special",
            Self::ContainsIpc => "contains_ipc",
            Self::ContainsMount => "contains_mount",
            Self::UnsupportedFs => "unsupported_fs",
            Self::Changed => "changed",
            Self::TooManyEntries => "too_many_entries",
            Self::InUse => "in_use",
            Self::Locked => "locked",
            Self::UnsafePath => "unsafe_path",
            Self::UnsupportedLayout => "unsupported_layout",
            Self::Empty => "empty",
            Self::NotIdle => "not_idle",
            Self::FutureTime => "future_time",
            Self::OtherFs => "other_fs",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A refused candidate: the reason, the entry that decided it, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub reason: Reason,
    pub path: PathBuf,
    pub detail: String,
}

impl Skip {
    pub(crate) fn new(reason: Reason, path: PathBuf, detail: impl Into<String>) -> Self {
        Self {
            reason,
            path,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}: {}",
            self.reason,
            self.path.display(),
            self.detail
        )
    }
}

/// Whether `deadline` has passed. Asks the clock only when there is one, so
/// a run without a deadline makes no clock calls.
fn deadline_passed<H: Host>(host: &H, deadline: Option<MonoTime>) -> bool {
    deadline.is_some_and(|d| host.now() >= d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Dev;

    fn stat() -> Stat {
        Stat {
            dev: Dev {
                major: 0,
                minor: 42,
            },
            file_type: Some(FileType::Regular),
            perm: Some(0o644),
            nlink: Some(1),
            uid: Some(1000),
            gid: Some(1000),
            ino: Some(7),
            size: Some(0),
            blocks: Some(0),
            mtime: None,
            ctime: None,
            btime: Some(Timestamp { secs: 10, nanos: 5 }),
            mnt_id: Some(30),
            mount_root: Some(false),
        }
    }

    #[test]
    fn network_filesystems_are_known_by_type_also_through_fuse() {
        for net in [
            "nfs",
            "nfs4",
            "cifs",
            "smb3",
            "fuse.sshfs",
            "sshfs",
            "fuse.rclone",
            "9p",
        ] {
            assert!(is_network_fs(net), "{net}");
        }
        for local in [
            "ext4",
            "tmpfs",
            "btrfs",
            "xfs",
            "overlay",
            "fuse",
            "fuse.portal",
            "nfsd",
        ] {
            assert!(!is_network_fs(local), "{local}");
        }
    }

    #[test]
    fn every_reason_has_a_distinct_report_name() {
        let all = Reason::ALL;
        let mut names: Vec<&str> = all.iter().map(|r| r.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len());
    }

    /// Every part of the identity counts: an object with the same inode
    /// number but another owner, type or birth time (removed and recreated,
    /// the number reused) is not the recorded one. ModelHost never reuses
    /// inode numbers, so the birth time is pinned here.
    #[test]
    fn identity_is_device_inode_owner_type_and_birth_time() {
        let recorded = Identity::of(&stat()).expect("answered");
        let same = |s: Stat| Identity::of(&s) == Some(recorded);
        assert!(same(stat()));
        assert!(!same(Stat {
            ino: Some(8),
            ..stat()
        }));
        assert!(!same(Stat {
            dev: Dev {
                major: 0,
                minor: 43
            },
            ..stat()
        }));
        assert!(!same(Stat {
            uid: Some(1001),
            ..stat()
        }));
        assert!(!same(Stat {
            file_type: Some(FileType::Directory),
            ..stat()
        }));
        assert!(!same(Stat {
            btime: Some(Timestamp { secs: 10, nanos: 6 }),
            ..stat()
        }));
        assert!(
            !same(Stat {
                btime: None,
                ..stat()
            }),
            "a birth time no longer reported"
        );
        // Unanswered inode, owner or type: no identity at all.
        for s in [
            Stat {
                ino: None,
                ..stat()
            },
            Stat {
                uid: None,
                ..stat()
            },
            Stat {
                file_type: None,
                ..stat()
            },
        ] {
            assert_eq!(Identity::of(&s), None);
        }
        // Without a birth time on either side, the rest decides.
        let no_btime = Identity::of(&Stat {
            btime: None,
            ..stat()
        })
        .expect("answered");
        assert_eq!(
            Identity::of(&Stat {
                btime: None,
                ..stat()
            }),
            Some(no_btime)
        );
    }
}
