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
//!    allocated size. Whether a VCS checkout inside refuses the candidate
//!    is the source profile's setting, in [`Rules::checkouts`] (refused
//!    unless the profile says `checkouts: allow`).
//! 2. [`delete_unchecked`]: deletes exactly the entries of the record, depth first.
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

pub(crate) mod container_storage;
mod delete;
mod frames;
mod overlay;
mod walk;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::host::{DevIno, FileType, Host, MonoTime, Stat, Timestamp};
use crate::resolve::Resolved;

pub use delete::{DeleteOptions, Deletion, End, delete_unchecked};
pub(crate) use walk::host_failure;
pub use walk::{
    CACHEDIR_TAG, CARGO_LOCK_NAMES, CARGO_UNIT_LOCK_NAME, CHECKOUT_MARKERS, CheckoutMarker,
    Checkouts, KEEP_MARKER, PRIVATE_TMP_PREFIX, Rules, WalkStop, checkout_metadata_below,
    checkout_metadata_down_to, is_cargo_lock_name, is_cargo_unit_lock_path, is_checkout_marker,
    is_private_tmp, path_in_private_tmp, tolerated_marker, walk,
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

pub use crate::places::space::{AUTOFS, NETWORK_FS_TYPES, may_be_networked, networked_why};

/// Where a directory lies among subvolumes: its `statx` device and, where
/// the kernel reports it, its `stx_subvol` (TASK-286, TASK-307).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume {
    pub dev: crate::host::Dev,
    pub subvol: Option<u64>,
}

impl Volume {
    pub fn of(stat: &crate::host::Stat) -> Self {
        Self {
            dev: stat.dev,
            subvol: stat.subvol,
        }
    }
}

/// Filesystem types whose subvolumes keep the filesystem's own `st_dev`,
/// so that only `stx_subvol` (Linux 6.10 on) tells them apart (TASK-307:
/// measured on bcachefs 1.39.6, the out-of-tree module on Linux 6.18;
/// btrfs gives each subvolume an anonymous device and reports
/// `stx_subvol` too).
pub const SUBVOLUMES_SHARE_DEV: &[&str] = &["bcachefs"];

/// Filesystem types with subvolumes, whose `stx_subvol` is compared. Not
/// for any other: overlayfs passes a layer's `stx_subvol` through (its
/// `getattr` copies the real inode's), so a merged directory and a
/// lower-only one can differ in it on an overlay over btrfs and ext4; an
/// overlay's upper layer is proved in that layer instead (`overlay`, the
/// eligibility walk), as an `rmdir` through the overlay removes an upper
/// entry, a subvolume of the upper layer included.
pub const SUBVOLUME_FILESYSTEMS: &[&str] = &["btrfs", "bcachefs"];

/// Whether the directory at `path` (`inner`), reached from the directory
/// `outer` with no mount between, lies in `outer`'s subvolume
/// (docs/architecture.md, Filesystem matching: Subvolume boundaries). It
/// does not when its device differs (a nested btrfs subvolume or
/// snapshot, TASK-286, or a directory of another filesystem shown without
/// a mount of its own) or its `stx_subvol` differs (a nested bcachefs one,
/// which keeps the device, TASK-307): `contains_mount`, as `rmdir` deletes
/// an empty subvolume's top (Linux 4.18, unprivileged). One reported and
/// the other not is no proof of the same: `contains_mount` too. The
/// `stx_subvol` rules apply on a filesystem of [`SUBVOLUME_FILESYSTEMS`],
/// or when `fs_type` is not known (the safe side). Neither
/// reported on a filesystem of [`SUBVOLUMES_SHARE_DEV`] (`fs_type`, the
/// mount's; `None` where the caller has none, the walk then deciding) is
/// `unsupported_fs`: a nested subvolume cannot be told from a directory
/// there, so nothing is taken (fail closed). Directories only: overlayfs
/// without `xino` shows a file of a lower layer under that layer's device.
/// The discovery of a candidate, the eligibility walk and the Cargo
/// inspection and discovery all ask it, as the tally does not enter such a
/// directory.
pub fn same_subvolume(
    path: &Path,
    inner: Volume,
    outer: Volume,
    fs_type: Option<&str>,
) -> Result<(), Skip> {
    let nested = |what: String| {
        Err(Skip::new(
            Reason::ContainsMount,
            path.to_owned(),
            format!(
                "{} is a directory {what} without a mount between (a nested subvolume or \
                 snapshot): not touched",
                path.display()
            ),
        ))
    };
    if inner.dev != outer.dev {
        return nested(format!(
            "on device {} inside one on device {}",
            inner.dev, outer.dev
        ));
    }
    if fs_type.is_some_and(|t| !SUBVOLUME_FILESYSTEMS.contains(&t)) {
        return Ok(());
    }
    match (inner.subvol, outer.subvol) {
        (Some(a), Some(b)) if a != b => {
            nested(format!("in subvolume {a} inside one in subvolume {b}"))
        }
        (Some(_), Some(_)) => Ok(()),
        (Some(a), None) => nested(format!(
            "in subvolume {a} inside one whose subvolume the kernel did not report"
        )),
        (None, Some(b)) => nested(format!(
            "whose subvolume the kernel did not report inside one in subvolume {b}"
        )),
        (None, None) => match fs_type.filter(|t| SUBVOLUMES_SHARE_DEV.contains(t)) {
            Some(fs_type) => Err(Skip::new(
                Reason::UnsupportedFs,
                path.to_owned(),
                format!(
                    "{} is on {fs_type}, whose subvolumes keep the filesystem's device, and \
                     the kernel did not report its subvolume (STATX_SUBVOL, Linux 6.10 on): a \
                     nested subvolume or snapshot cannot be told from a directory there, so \
                     no directory on it is taken",
                    path.display()
                ),
            )),
            None => Ok(()),
        },
    }
}

/// Work a resumable discovery walk reports to its pause callback
/// (`fs_kind::DiscoverCursor`, `cargo_target::DiscoverCursor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    /// About to do this many operations of progress (a listing, a
    /// `statx`, a candidate's checks): the callback may pause the walk
    /// before them (`true`).
    Next(usize),
    /// Reopened one directory on the way down: charged, never paused.
    Reopen,
    /// Listed this many names: charged, never paused.
    Listed(usize),
}

/// The index's view of a candidate's discovery `statx`: what its identity
/// falls back to without a birth time, and its provisional age
/// (docs/architecture.md, The index: Identity; The tally).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    /// The inode of the directory the candidate was found in, if reported.
    pub parent_ino: Option<u64>,
    /// `max(mtime, ctime)` of the candidate itself, if both were reported.
    pub changed: Option<Timestamp>,
}

impl Seen {
    /// Of `stat`, found in the directory whose stat is `parent`.
    pub fn of(stat: &Stat, parent: &Stat) -> Self {
        Self {
            parent_ino: parent.ino,
            changed: stat.mtime.zip(stat.ctime).map(|(m, c)| m.max(c)),
        }
    }
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

    /// `identity` for a message; `None` is what [`Identity::of`] gives when
    /// the kernel did not report one.
    pub fn shown(identity: Option<Self>) -> String {
        identity.map_or_else(
            || "an entry whose inode, owner or type was not reported".to_owned(),
            |i| i.to_string(),
        )
    }
}

/// The `not_owner` detail: `owned by uid 1000, not by uid 1001` (or `not
/// by any of uids 1001, 1002`, the `owners:` list), never Debug text.
pub(crate) fn not_owner_detail(uid: u32, owners: &[u32]) -> String {
    let allowed: Vec<String> = owners.iter().map(u32::to_string).collect();
    match allowed.as_slice() {
        [one] => format!("owned by uid {uid}, not by uid {one}"),
        many => format!("owned by uid {uid}, not by any of uids {}", many.join(", ")),
    }
}

/// E.g. `directory 0:32:153 of uid 1000 born 1729850000.5`: every field
/// the identity compares, so two that differ never read the same.
impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} of uid {}", self.file_type, self.id, self.uid)?;
        match self.btime {
            Some(t) => write!(f, " born {}.{:09}", t.secs, t.nanos),
            None => Ok(()),
        }
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
    /// On an overlay with an upper layer: its name there ([`UpperName`]).
    pub upper: UpperName,
}

/// An entry's name in the upper layer of the overlay the walk met it on
/// (docs/safety.md, In use: Upper-layer names; TASK-305): another name of
/// the same object under another identity, which a deletion through the
/// overlay removes, so the in-use check matches it as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpperName {
    /// Not on an overlay with an upper layer, a symlink, or no upper entry
    /// (lower-only: a deletion through the overlay leaves the lower name).
    None,
    /// The upper entry's (device, inode), as `statx` of its name in the
    /// upper layer shows it.
    Known(DevIno),
    /// Whether it has an upper entry, and which, cannot be told.
    Unknown,
}

/// The upper layer of an overlay a walked candidate lies on, for the
/// in-use check's messages and doubt ([`UpperName`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpperLayer {
    /// Reached by the overlay's `upperdir=`.
    Reached {
        /// `upperdir=` joined with the overlay mount's root field: the
        /// upper directory of the overlay's mount point.
        at: PathBuf,
        /// The overlay's mount point.
        mount_point: PathBuf,
        /// Why some regular file's upper name is [`UpperName::Unknown`]
        /// (the first one met).
        unread: Option<String>,
    },
    /// Not reached (why): every regular file's upper name is
    /// [`UpperName::Unknown`].
    Unreached(String),
}

impl UpperLayer {
    /// The upper name of what the overlay shows at `path`, where the layer
    /// was reached and `path` is below its mount point.
    pub fn name_of(&self, path: &Path) -> Option<PathBuf> {
        match self {
            Self::Reached {
                at, mount_point, ..
            } => upper_path(at, mount_point, path),
            Self::Unreached(_) => None,
        }
    }

    /// Why some regular file's upper name cannot be told, if one cannot.
    pub fn unread(&self) -> Option<&str> {
        match self {
            Self::Reached { unread, .. } => unread.as_deref(),
            Self::Unreached(why) => Some(why),
        }
    }
}

/// The name in an upper layer reached at `at` of what the overlay mounted
/// at `mount_point` shows at `path` (the layer mirrors the overlay's names
/// for every entry it holds); `None` when `path` is not below the mount
/// point.
fn upper_path(at: &Path, mount_point: &Path, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(mount_point).ok()?;
    Some(if rel.as_os_str().is_empty() {
        at.to_owned()
    } else {
        at.join(rel)
    })
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

/// The objects the eligibility walk checked: what [`delete_unchecked`] may remove,
/// and what the in-use check must look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkRecord {
    root: Entry,
    /// The name and mount under which `walk` checked the candidate. This
    /// distinguishes hard-linked names with the same inode identity.
    origin: PathBuf,
    mount_id: u64,
    /// Every directory's entries, sorted by name; `listings[0]` is the
    /// candidate's when it is a directory.
    listings: Vec<Vec<Entry>>,
    /// Every checkout marker the walk admitted, in the order met.
    markers: Vec<CheckoutMarker>,
    entries: usize,
    bytes: u64,
    newest_change: Option<Timestamp>,
    /// On an overlay with an upper layer: that layer (each regular file's
    /// [`Entry::upper`]).
    upper: Option<UpperLayer>,
}

impl WalkRecord {
    /// Whether this record was walked for this candidate name on this
    /// mount. The deleting check also borrows this exact record, so a later
    /// re-walk cannot be substituted after it returns a `Clear`.
    pub(crate) fn belongs_to<F>(&self, candidate: &Candidate<'_, F>) -> bool {
        self.origin == candidate.path && self.mount_id == candidate.mount_id
    }

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

    /// Whether the checked tree holds a version-control checkout marker
    /// that a source with these `empty_checkout_markers` does not tolerate
    /// ([`tolerated_marker`]): the walk's own refusal, judged again per
    /// source when a union walked with checkouts allowed and combines the
    /// policies of the terms that admitted the candidate.
    pub fn contains_checkout(&self, empty_checkout_markers: &[crate::pattern::Pattern]) -> bool {
        self.markers
            .iter()
            .any(|m| !tolerated_marker(empty_checkout_markers, &m.below, m.empty_file))
    }

    /// (device, inode) of every entry, the candidate first: what the in-use
    /// check matches processes against (docs/safety.md, In use).
    pub fn objects(&self) -> impl Iterator<Item = DevIno> + '_ {
        std::iter::once(self.root.identity.id)
            .chain(self.listings.iter().flatten().map(|e| e.identity.id))
    }

    /// The upper layer of the overlay the candidate lies on, if it is on
    /// one with an upper layer ([`UpperName`]).
    pub fn upper_layer(&self) -> Option<&UpperLayer> {
        self.upper.as_ref()
    }

    /// The identities the in-use check matches, each with the one shown
    /// through the walk's mount (for messages): per entry, the candidate
    /// first, its upper name's where known ([`UpperName::Known`]), then its
    /// own.
    pub fn matched(&self) -> impl Iterator<Item = (DevIno, DevIno)> + '_ {
        self.all_entries().flat_map(|entry| {
            let shown = entry.identity.id;
            let upper = match entry.upper {
                UpperName::Known(upper) if upper != shown => Some((upper, shown)),
                _ => None,
            };
            upper.into_iter().chain(std::iter::once((shown, shown)))
        })
    }

    /// The first regular file, in record order, whose upper name cannot be
    /// told ([`UpperName::Unknown`]): its (device, inode) through the
    /// overlay. Only a regular file backs a loop device.
    pub fn upper_unknown(&self) -> Option<DevIno> {
        self.all_entries()
            .find(|entry| {
                entry.upper == UpperName::Unknown && entry.identity.file_type == FileType::Regular
            })
            .map(|entry| entry.identity.id)
    }

    /// Whether the record holds a regular file.
    pub fn holds_a_file(&self) -> bool {
        self.all_entries()
            .any(|entry| entry.identity.file_type == FileType::Regular)
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
        // Markers move with their part, re-based to its root. A source's
        // `empty_checkout_markers` are relative to its candidate, so a part
        // must not be judged with them; only the Cargo kind splits, and it
        // tolerates no marker (the loader refuses the field there).
        let (inside, outside): (Vec<CheckoutMarker>, Vec<CheckoutMarker>) =
            std::mem::take(&mut self.markers)
                .into_iter()
                .partition(|m| m.below.starts_with(rel));
        self.markers = outside;
        let markers = inside
            .into_iter()
            .map(|m| CheckoutMarker {
                below: m
                    .below
                    .strip_prefix(rel)
                    .map_or(m.below.clone(), Path::to_path_buf),
                empty_file: m.empty_file,
            })
            .collect();
        Some(WalkRecord {
            root,
            origin: self.origin.join(rel),
            mount_id: self.mount_id,
            listings,
            markers,
            entries,
            bytes,
            newest_change: self.newest_change,
            upper: self.upper.clone(),
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
    /// A `.diskgc-keep` file is inside the candidate or between its root
    /// and it (`keep_marker`; named `kept` before 0.1, TASK-192).
    KeepMarker,
    /// A PrivateTmp entry, a Cargo lock file name (`.cargo-*lock*`, known
    /// or not) anywhere, or a device node or an object of unknown type; or
    /// container storage ([`container_storage`]) in the candidate, the
    /// candidate itself, or the root or a directory between it and the
    /// candidate (TASK-242).
    ContainsSpecial,
    /// A version-control checkout is inside ([`CHECKOUT_MARKERS`]
    /// anywhere, the candidate itself included), or the candidate lies
    /// inside a checkout's metadata, found by its path below the root
    /// ([`checkout_metadata_below`]). Unless the source profile says
    /// `checkouts: allow` ([`Checkouts`]).
    ContainsCheckout,
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
    /// No in-use verdict could be had: every in-use scan the candidate was
    /// given finished older than the freshness bound (a host too busy, or
    /// a suspend), or later candidates of a tier after several in a row
    /// did (docs/safety.md, In use: Freshness). Never deleted on a stale
    /// scan; a later run may reclaim it.
    InUseUnknown,
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
    /// Older than the exclusive upper age bound of a negated idle filter.
    TooIdle,
    /// A selection entry filter did not match (or its complement matched).
    Filtered,
    /// A candidate overlaps a raw match of a negated source profile.
    Excluded,
    /// Its last use lies in the future, so its age says nothing
    /// (docs/reference.md, Predicates).
    FutureTime,
    /// On another filesystem than the one that must have the space:
    /// deleting it would free nothing there (docs/architecture.md,
    /// Filesystem matching).
    OtherFs,
    /// A version kept by the source's newest-version rule.
    NewestVersion,
    /// A version named by a configured current symlink.
    LinkedCurrent,
    /// Listed as installed in Cursor's extension registry, or the registry cannot be trusted.
    InstalledExtension,
    /// A toolchain its manager could select: the default, an override or a
    /// pin file names it, or the pin state cannot be read completely
    /// (docs/safety.md, Toolchain pins).
    PinnedToolchain,
    /// The candidate overlaps a configured protected source match.
    Protected,
    /// A recognised instance of a source's liveness contract whose
    /// producer state cannot be determined (docs/safety.md, Producer
    /// liveness): a precondition failed, a process stayed unreadable, a
    /// marker is malformed. Retained, never eligible.
    ProducerUnknown,
    /// A backend tier's client selects a remote endpoint or store
    /// (docs/reference.md, Backend skips and errors).
    RemoteBackend,
    /// A backend tier's backend is one diskgc has not verified
    /// (docs/reference.md, Backend skips and errors).
    UnsupportedBackend,
    /// A backend tier's backend is installed and selected but not serving
    /// diskgc's user: its socket absent, the connection refused, or the
    /// daemon not answering or refusing the user (docs/reference.md,
    /// Backend skips and errors).
    BackendUnavailable,
    /// An index unit that is not what discovery would find now: gone,
    /// replaced by another object under its name, or no longer found by
    /// its source under the tier's root (docs/architecture.md, The reclaim
    /// walk). Never walked; discovery judges whatever is there now.
    Stale,
}

impl Reason {
    /// Every reason (keep it next to [`Reason::as_str`], whose match is
    /// exhaustive: a new reason is added to both).
    pub const ALL: [Reason; 33] = [
        Reason::Missing,
        Reason::NotOwner,
        Reason::Unreadable,
        Reason::KeepMarker,
        Reason::ContainsSpecial,
        Reason::ContainsCheckout,
        Reason::ContainsIpc,
        Reason::ContainsMount,
        Reason::UnsupportedFs,
        Reason::Changed,
        Reason::TooManyEntries,
        Reason::InUse,
        Reason::InUseUnknown,
        Reason::Locked,
        Reason::UnsafePath,
        Reason::UnsupportedLayout,
        Reason::Empty,
        Reason::NotIdle,
        Reason::TooIdle,
        Reason::Filtered,
        Reason::Excluded,
        Reason::FutureTime,
        Reason::OtherFs,
        Reason::NewestVersion,
        Reason::LinkedCurrent,
        Reason::InstalledExtension,
        Reason::PinnedToolchain,
        Reason::Protected,
        Reason::ProducerUnknown,
        Reason::RemoteBackend,
        Reason::UnsupportedBackend,
        Reason::BackendUnavailable,
        Reason::Stale,
    ];

    /// The skip reason's name in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::NotOwner => "not_owner",
            Self::Unreadable => "unreadable",
            Self::KeepMarker => "keep_marker",
            Self::ContainsSpecial => "contains_special",
            Self::ContainsCheckout => "contains_checkout",
            Self::ContainsIpc => "contains_ipc",
            Self::ContainsMount => "contains_mount",
            Self::UnsupportedFs => "unsupported_fs",
            Self::Changed => "changed",
            Self::TooManyEntries => "too_many_entries",
            Self::InUse => "in_use",
            Self::InUseUnknown => "in_use_unknown",
            Self::Locked => "locked",
            Self::UnsafePath => "unsafe_path",
            Self::UnsupportedLayout => "unsupported_layout",
            Self::Empty => "empty",
            Self::NotIdle => "not_idle",
            Self::TooIdle => "too_idle",
            Self::Filtered => "filtered",
            Self::Excluded => "excluded",
            Self::FutureTime => "future_time",
            Self::OtherFs => "other_fs",
            Self::NewestVersion => "newest_version",
            Self::LinkedCurrent => "linked_current",
            Self::InstalledExtension => "installed_extension",
            Self::PinnedToolchain => "pinned_toolchain",
            Self::Protected => "protected",
            Self::ProducerUnknown => "producer_unknown",
            Self::RemoteBackend => "remote_backend",
            Self::UnsupportedBackend => "unsupported_backend",
            Self::BackendUnavailable => "backend_unavailable",
            Self::Stale => "stale",
        }
    }
}

/// Skip reasons the JSON contract documents (docs/reference.md, Output)
/// that no code produces yet, each with the task that brings its producer.
/// They are reserved: a consumer must accept them, and no other value can
/// appear, because the report's reason is a [`Reason`]. The task that adds
/// a producer adds the variant to [`Reason`] and removes the entry here
/// (`every_skip_reason_has_a_vector` fails until it does).
pub const RESERVED_REASONS: [(&str, &str); 0] = [];

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
    /// Scan-time process evidence, when this refusal names a known process.
    pub process_blockers: Option<Vec<ProcessBlocker>>,
    /// Why whether it is in use cannot be told: set on every
    /// `in_use_unknown`, `None` on every other reason (docs/reference.md,
    /// Output: `unknown_cause`).
    pub unknown_cause: Option<UnknownCause>,
    /// The candidate's allocated bytes, when its eligibility walk was
    /// complete before this refusal (an idle requirement, the in-use check,
    /// a liveness contract): what it would be sized at had it been
    /// eligible. `None` for a refusal before or during the walk, where a
    /// size would be a partial count (docs/reference.md, Output: `summary`).
    pub measured_bytes: Option<u64>,
}

/// Why an `in_use_unknown` candidate could not be judged: a stable report
/// enum (JSON `unknown_cause`), so programs and the human summary branch
/// on it, not on the detail text (TASK-263 review, mped S2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownCause {
    /// No in-use scan stayed fresh or settled for the candidate, or a
    /// process's own reads did not settle (it was exiting or changing,
    /// TASK-348): a busy host, and a later run may judge it. Three in a row
    /// make the tier back off, never skip candidates unchecked (TASK-340).
    Busy,
    /// A process of the user could not be inspected, and can stay so while
    /// it runs: the kernel refused it for no privilege boundary its status
    /// shows (a `newgrp` shell), a read of it failed otherwise, or a live
    /// task of it showed nothing (TASK-190; its `process_blockers` name
    /// it). One whose reads only did not settle is [`Self::Busy`], whatever
    /// its status read showed.
    UnreadableProcess,
    /// The held `/proc` cannot list every process (`hidepid`, a PID
    /// namespace other than the initial one, facts unread or changed):
    /// it stays so until the environment changes (TASK-263).
    IncompleteProcessList,
    /// A loop device's backing file could be in the candidate and cannot
    /// be told apart from it: it stays so until the device is detached
    /// (TASK-169).
    LoopDevice,
}

impl UnknownCause {
    pub const ALL: [UnknownCause; 4] = [
        UnknownCause::Busy,
        UnknownCause::UnreadableProcess,
        UnknownCause::IncompleteProcessList,
        UnknownCause::LoopDevice,
    ];

    /// The cause's name in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::UnreadableProcess => "unreadable_process",
            Self::IncompleteProcessList => "incomplete_process_list",
            Self::LoopDevice => "loop_device",
        }
    }

    /// Whether waiting does not end the cause: it lasts until someone acts
    /// (ends the process, detaches the device, changes the environment).
    /// Only a busy host passes by itself (TASK-348). The one rule for both
    /// what a tier does after three in a row (it backs off only for a cause
    /// that does not stay, docs/safety.md, In use: Freshness) and what the
    /// summary tells the user to do.
    pub fn stays(self) -> bool {
        match self {
            Self::Busy => false,
            Self::UnreadableProcess | Self::IncompleteProcessList | Self::LoopDevice => true,
        }
    }
}

/// A process observed by the in-use scan. The evidence value is a stable
/// report enum; the human detail remains separate and may change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessBlocker {
    pub pid: u32,
    pub command: Option<String>,
    pub evidence: ProcessEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessEvidence {
    Fd,
    Cwd,
    Root,
    Exe,
    Map,
    InodeMap,
    Environment,
    Unreadable,
    TargetCwd,
    TargetExe,
    ProjectCargo,
    DetachedCargo,
    CargoManifestDir,
    /// A listed process carries the number a liveness contract recorded
    /// as its producer's pid (a number match, not proof of identity).
    OwnerPid,
    /// An element of the process's command line names the candidate (a
    /// path string, or a `--basetemp` value resolved into it): the pytest
    /// contract.
    CommandLine,
    /// The process could use the pytest session's temp root (a
    /// temp-directory variable resolving to it, Python's fallback roots,
    /// its working directory).
    TempRoot,
    /// A path the pytest contract needs for this process did not resolve
    /// to an answer: a relative value of a variable or option pytest
    /// resolves, an absolute one whose lookup in the process's root
    /// failed, or a failed lookup of one of Python's fallback roots. The
    /// contract cannot decide.
    UnresolvedPath,
}

impl ProcessEvidence {
    pub const ALL: [Self; 17] = [
        Self::Fd,
        Self::Cwd,
        Self::Root,
        Self::Exe,
        Self::Map,
        Self::InodeMap,
        Self::Environment,
        Self::Unreadable,
        Self::TargetCwd,
        Self::TargetExe,
        Self::ProjectCargo,
        Self::DetachedCargo,
        Self::CargoManifestDir,
        Self::OwnerPid,
        Self::CommandLine,
        Self::TempRoot,
        Self::UnresolvedPath,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fd => "fd",
            Self::Cwd => "cwd",
            Self::Root => "root",
            Self::Exe => "exe",
            Self::Map => "map",
            Self::InodeMap => "inode_map",
            Self::Environment => "environment",
            Self::Unreadable => "unreadable",
            Self::TargetCwd => "target_cwd",
            Self::TargetExe => "target_exe",
            Self::ProjectCargo => "project_cargo",
            Self::DetachedCargo => "detached_cargo",
            Self::CargoManifestDir => "cargo_manifest_dir",
            Self::OwnerPid => "owner_pid",
            Self::CommandLine => "command_line",
            Self::TempRoot => "temp_root",
            Self::UnresolvedPath => "unresolved_path",
        }
    }
}

impl Skip {
    pub(crate) fn new(reason: Reason, path: PathBuf, detail: impl Into<String>) -> Self {
        Self {
            reason,
            path,
            detail: detail.into(),
            process_blockers: None,
            unknown_cause: None,
            measured_bytes: None,
        }
    }

    /// This refusal came after a complete walk that measured `bytes`
    /// ([`Skip::measured_bytes`]).
    pub(crate) fn measured(mut self, bytes: u64) -> Self {
        self.measured_bytes = Some(bytes);
        self
    }

    /// An `in_use_unknown` skip and its cause.
    pub(crate) fn in_use_unknown(
        path: PathBuf,
        cause: UnknownCause,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            unknown_cause: Some(cause),
            ..Self::new(Reason::InUseUnknown, path, detail)
        }
    }

    pub(crate) fn with_process_blockers(mut self, blockers: Vec<ProcessBlocker>) -> Self {
        if !blockers.is_empty() {
            self.process_blockers = Some(blockers);
        }
        self
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

    /// TASK-307: the subvolume boundary by device and `stx_subvol`; a side
    /// that does not report `stx_subvol` proves nothing, and bcachefs
    /// without it fails closed while other filesystems keep the device
    /// rule.
    #[test]
    fn the_subvolume_boundary_fails_closed() {
        let dev = Dev {
            major: 0,
            minor: 40,
        };
        let at = |subvol| Volume { dev, subvol };
        let path = Path::new("/w/c/d");
        let reason = |inner, outer, fs: &str| {
            same_subvolume(path, inner, outer, Some(fs)).map_err(|skip| skip.reason)
        };
        assert_eq!(reason(at(Some(1)), at(Some(1)), "bcachefs"), Ok(()));
        assert_eq!(
            reason(at(Some(2)), at(Some(1)), "bcachefs"),
            Err(Reason::ContainsMount)
        );
        assert_eq!(
            reason(at(Some(2)), at(None), "btrfs"),
            Err(Reason::ContainsMount)
        );
        assert_eq!(
            reason(at(None), at(Some(2)), "btrfs"),
            Err(Reason::ContainsMount)
        );
        assert_eq!(
            reason(at(None), at(None), "bcachefs"),
            Err(Reason::UnsupportedFs)
        );
        assert_eq!(reason(at(None), at(None), "btrfs"), Ok(()));
        assert_eq!(reason(at(None), at(None), "ext4"), Ok(()));
        // An overlay passes a layer's stx_subvol through: not compared.
        assert_eq!(reason(at(Some(2)), at(None), "overlay"), Ok(()));
        assert_eq!(reason(at(Some(2)), at(Some(1)), "overlay"), Ok(()));
        let unknown = same_subvolume(path, at(Some(2)), at(None), None).map_err(|s| s.reason);
        assert_eq!(unknown, Err(Reason::ContainsMount));
        let other = Volume {
            dev: Dev {
                major: 0,
                minor: 41,
            },
            subvol: Some(1),
        };
        assert_eq!(
            reason(other, at(Some(1)), "btrfs"),
            Err(Reason::ContainsMount)
        );
        assert_eq!(same_subvolume(path, at(None), at(None), None), Ok(()));
    }

    /// `changed` details name identities as a user reads them, never as
    /// Debug text (TASK-140), and two identities that differ in any
    /// compared field read differently.
    #[test]
    fn identities_read_as_text_in_messages() {
        let mut identity = Identity::of(&stat()).expect("complete");
        assert_eq!(
            identity.to_string(),
            "regular file 0:42:7 of uid 1000 born 10.000000005"
        );
        let before = identity.to_string();
        identity.btime = None;
        assert_eq!(identity.to_string(), "regular file 0:42:7 of uid 1000");
        assert_ne!(identity.to_string(), before);
        assert_eq!(
            Identity::shown(None),
            "an entry whose inode, owner or type was not reported"
        );
        assert_eq!(
            not_owner_detail(1000, &[1001]),
            "owned by uid 1000, not by uid 1001"
        );
        assert_eq!(
            not_owner_detail(1000, &[1001, 1002]),
            "owned by uid 1000, not by any of uids 1001, 1002"
        );
    }

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
            atime: None,
            mnt_id: Some(30),
            mount_root: Some(false),
            subvol: None,
        }
    }

    /// Fail closed on FUSE (task-18 round 2, Codex B1): every FUSE type is
    /// possibly networked unless allowlisted as local, so a network FUSE
    /// client missing from the list (`ceph-fuse` once was) is refused too.
    #[test]
    fn network_and_unknown_fuse_filesystems_may_be_networked() {
        for net in [
            "nfs",
            "nfs4",
            "cifs",
            "smb3",
            "9p",
            "virtiofs",
            "ocfs2",
            "fuse.sshfs",
            "fuse.rclone",
            "fuse.ceph-fuse",
            "fuse.mfs",
            "fuse.s3fs",
            "fuse",
            "fuseblk",
            "fuseblk.ntfs-3g",
            "fuse.portal",
            "fuse.never-heard-of-it",
            "fuse.fuse-overlayfs",
            "fuse.",
        ] {
            assert!(may_be_networked(net), "{net}");
        }
        for local in ["ext4", "tmpfs", "btrfs", "xfs", "overlay", "nfsd"] {
            assert!(!may_be_networked(local), "{local}");
        }
        assert!(
            may_be_networked("autofs"),
            "an untriggered automount (TASK-180)"
        );
        assert!(networked_why("autofs").starts_with("an untriggered automount"));
        assert!(networked_why("fuse.ceph-fuse").starts_with("a network"));
        assert!(networked_why("fuse.never-heard-of-it").starts_with("a FUSE"));
    }

    /// The path rule looks at the names strictly between the root and the
    /// candidate: not the root's own path, not the candidate's own name
    /// (the walk refuses that), only exact marker names. Under `checkouts:
    /// allow` it refuses nothing below the root. Whatever the setting, a
    /// candidate not below its root (`/rx` is not below `/r`) is refused
    /// as `unsafe_path` (fail closed).
    #[test]
    fn checkout_metadata_is_found_between_the_root_and_the_candidate() {
        let judged = |checkouts, root: &str, at: &str| {
            checkout_metadata_below(checkouts, Path::new(root), Path::new(at))
                .map(|skip| (skip.reason, skip.path.display().to_string()))
        };
        let below = |root: &str, at: &str| {
            judged(Checkouts::Refuse, root, at).map(|(reason, path)| {
                assert_eq!(reason, Reason::ContainsCheckout, "{root} {at}");
                path
            })
        };
        assert_eq!(
            below("/r", "/r/proj/.git/objects").as_deref(),
            Some("/r/proj/.git")
        );
        assert_eq!(
            below("/r", "/r/a/.hg/store/data").as_deref(),
            Some("/r/a/.hg")
        );
        assert_eq!(below("/r", "/r/.svn/x").as_deref(), Some("/r/.svn"));
        assert_eq!(below("/r", "/r/.git/a/.hg/b").as_deref(), Some("/r/.git"));
        for (root, at) in [
            ("/r", "/r/proj/.git"),
            ("/r/.git", "/r/.git/lfs/tmp"),
            ("/r", "/r/proj/.github/workflows"),
            ("/r", "/r/proj/build/out"),
            ("/r", "/r"),
        ] {
            assert_eq!(below(root, at), None, "{root} {at}");
        }
        assert_eq!(judged(Checkouts::Allow, "/r", "/r/proj/.git/objects"), None);
        for checkouts in [Checkouts::Refuse, Checkouts::Allow] {
            for (root, at) in [
                ("/r", "/other/.git/x"),
                ("/r", "/rx/.git/y"),
                ("/r", "/rx/y"),
            ] {
                assert_eq!(
                    judged(checkouts, root, at),
                    Some((Reason::UnsafePath, at.to_owned())),
                    "{checkouts:?} {root} {at}"
                );
            }
        }
    }

    /// The rule for a directory's candidates (`checkout_metadata_down_to`)
    /// looks at the directory's own name too, so it judges for any entry
    /// of the directory what `checkout_metadata_below` judges for that
    /// entry, whatever the directory holds; the same fail-closed
    /// `unsafe_path` whatever the setting.
    #[test]
    fn checkout_metadata_down_to_a_directory_includes_its_own_name() {
        let judged = |checkouts, root: &str, dir: &str| {
            checkout_metadata_down_to(checkouts, Path::new(root), Path::new(dir))
                .map(|skip| (skip.reason, skip.path.display().to_string()))
        };
        let refused = |path: &str| Some((Reason::ContainsCheckout, path.to_owned()));
        for (root, dir, expected) in [
            ("/r", "/r/x/.git", refused("/r/x/.git")),
            ("/r", "/r/x/target/.hg/debug", refused("/r/x/target/.hg")),
            ("/r", "/r/.svn", refused("/r/.svn")),
            ("/r", "/r/x/target/debug", None),
            ("/r", "/r/x/.github", None),
            ("/r", "/r", None),
            ("/r/.git", "/r/.git/target/debug", None),
        ] {
            assert_eq!(
                judged(Checkouts::Refuse, root, dir),
                expected,
                "{root} {dir}"
            );
            let entry = format!("{dir}/entry");
            assert_eq!(
                checkout_metadata_below(Checkouts::Refuse, Path::new(root), Path::new(&entry))
                    .map(|skip| (skip.reason, skip.path.display().to_string())),
                expected,
                "{root} {entry}: the same verdict as for an entry of {dir}"
            );
        }
        assert_eq!(judged(Checkouts::Allow, "/r", "/r/x/.git"), None);
        for checkouts in [Checkouts::Refuse, Checkouts::Allow] {
            for dir in ["/other/.git", "/rx/.git", "/rx/y"] {
                assert_eq!(
                    judged(checkouts, "/r", dir),
                    Some((Reason::UnsafePath, dir.to_owned())),
                    "{checkouts:?} {dir}"
                );
            }
        }
    }

    /// Fail closed by default: rules that do not state the checkout
    /// setting ([`Rules::new`], or anything built from it) refuse
    /// checkouts, and the setting's own default is `refuse`.
    #[test]
    fn rules_that_do_not_state_the_checkout_setting_refuse_checkouts() {
        assert_eq!(Checkouts::default(), Checkouts::Refuse);
        assert_eq!(Rules::new(&[1000]).checkouts, Checkouts::Refuse);
        let narrower = Rules {
            max_entries: 3,
            ..Rules::new(&[1000])
        };
        assert_eq!(narrower.checkouts, Checkouts::Refuse);
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
    /// the number reused) is not the recorded one. ModelHost also exercises
    /// inode number reuse at both deletion check sites.
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
