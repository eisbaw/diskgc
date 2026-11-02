//! Loop devices in the in-use scan (docs/safety.md, In use: Loop devices).
//!
//! A configured loop device holds its backing file open in the kernel, where
//! no process's fds, maps or directories show it: an image set up with
//! `udisksctl loop-setup` or `losetup` and the file deleted under it loses
//! its data the moment the device detaches (TASK-169). So every scan lists
//! the configured loop devices ([`Host::loop_backing_files`]) and turns
//! each into what candidates are matched by, the backing file's identity
//! (device, inode), or else a refusal of the candidates it could be in.
//!
//! In order, for each device (TASK-263 review, Codex 2 and 3):
//!
//! 1. **The device's own word.** `LOOP_GET_STATUS64` on the opened device
//!    gives the held file's (device, inode), deleted or not, whatever its
//!    name ([`Host::loop_backing_identity`]): a use, matched like an open
//!    fd. Only a reader that can open the block device gets it (root, the
//!    `disk` group). The kernel takes it from the path the file was opened
//!    by (`vfs_getattr`), so a file opened through a filesystem stored in
//!    another's directories (overlayfs: its own or a layer's anonymous
//!    device, perhaps the copy-up origin's inode) shows an identity no
//!    other name of it has. Candidates on that filesystem's own mounts see
//!    the same identity; every other candidate is `in_use_unknown` while
//!    such a device is configured (TASK-299, [`stacked_alias`]).
//! 2. Otherwise only the name the kernel prints (`d_path`) is left, which
//!    proves nothing about identity: a mount made over the file's directory
//!    after the device opened it makes a fresh lookup reach another file (a
//!    decoy), and a name diskgc cannot look up may still have another link
//!    inside a candidate.
//!    - The name resolves (by the safe lookup every root takes, no symlink
//!      followed, network and FUSE mounts never touched) to a file none of
//!      the run's owners owns: a candidate holds only its owners' entries
//!      (the eligibility walk refuses any other), so it cannot be in one
//!      on a mount that shows owners as stored. Noted; candidates on a
//!      mount of the same filesystem that may show other owners (idmapped:
//!      [`crate::places::may_show_other_owners`]) are `in_use_unknown`
//!      (TASK-299: an idmapped bind shows root's image as the user's). An
//!      owner read through such a mount proves nothing: as below, every
//!      candidate. Limits (docs/safety.md, Limits of the loop rules): a
//!      decoy owned by another user over a real file of an owner's, and
//!      what the mount table does not show (an overlay's idmapped private
//!      layers, another namespace's mounts).
//!    - The text is not an absolute path: an anonymous object (`d_dname`),
//!      which no directory can name. Noted, not blocking.
//!    - The text ends in ` (deleted)`: the name the device opened is gone;
//!      another link of the file can only be on the same filesystem, so
//!      the candidates on the filesystem of the name's surviving parent
//!      directory are `in_use_unknown`; with no such parent, or a parent on
//!      a filesystem stored in another's directories (an overlay, whose
//!      other links lie on its layers' filesystems under their devices:
//!      TASK-299), every candidate. Limit: a mount made over that parent
//!      since.
//!    - Anything else: the name resolves to an owner's file (perhaps a
//!      decoy), or does not resolve: every candidate is `in_use_unknown`
//!      until the device is detached. A name that does not resolve is read
//!      again first (`d_path` follows renames, so a rename racing the
//!      lookup shows the new name), up to [`LOOP_REREADS`] times.
//!
//! Names of a backing file the walk cannot see by identity, an overlay's
//! upper names and eCryptfs's lower names, are judged with the walk record
//! ([`LoopBackings::identified`], [`LoopBackings::ecryptfs_alias`];
//! docs/safety.md, In use: Upper-layer names, eCryptfs names).
//!
//! Each device is kept with its own [`Scope`] and cause; a candidate is
//! `in_use_unknown` for the first one whose scope covers it
//! ([`LoopBackings::doubt`]).
//!
//! diskgc out of file descriptors, a listing that fails or a backing file
//! or device that cannot be read for another reason is an error of the
//! run (as the process list's is, TASK-166), never a shorter list.

use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::ScanError;
use super::scan::with_descriptor_advice;
use crate::engine::SUBVOLUME_FILESYSTEMS;
use crate::host::{Dev, DevIno, Host, HostError, LoopIdentity, MountEntry};
use crate::places::{may_show_other_owners, mount_for, stored_elsewhere};
use crate::resolve::{Want, resolve_in};

/// How often a backing file's text is read again when it names nothing
/// here and changed since the last read (a rename racing the lookup).
pub const LOOP_REREADS: usize = 3;

/// Which candidates a configured loop device's backing file could lie in,
/// where its identity alone does not tell (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// In none: no candidate can hold the file, or its identity is exact.
    Nowhere,
    /// In a candidate holding an object `statx` shows on this device (the
    /// filesystem of a deleted name's surviving directory).
    Filesystem(Dev),
    /// In a candidate on one of these mounts (ids of the mount table the
    /// scan read).
    Mounts(BTreeSet<u64>),
    /// In any candidate but those on these mounts (ids of the scan's
    /// table), which show the file under the identity the device gave.
    AllBut(BTreeSet<u64>),
    /// In any candidate.
    Everywhere,
}

impl Scope {
    /// Whether a candidate on mount `mount_id`, whose objects lie on the
    /// devices `devices` (`statx`), may hold the file.
    fn covers(&self, mount_id: u64, devices: &BTreeSet<Dev>) -> bool {
        match self {
            Self::Nowhere => false,
            Self::Filesystem(dev) => devices.contains(dev),
            Self::Mounts(ids) => ids.contains(&mount_id),
            Self::AllBut(ids) => !ids.contains(&mount_id),
            Self::Everywhere => true,
        }
    }
}

/// One configured loop device as the in-use check judges it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopBacking {
    pub device: Dev,
    /// Its `backing_file` text, as last read.
    pub text: PathBuf,
    /// The held file's identity (`LOOP_GET_STATUS64`), where diskgc can ask
    /// the device: a use, matched like an open fd.
    pub identity: Option<DevIno>,
    /// The candidates it may lie in beyond its identity.
    pub scope: Scope,
    /// What the report says of it: the note (scope [`Scope::Nowhere`]
    /// without an identity) or the `in_use_unknown` detail (any other).
    pub why: String,
    /// Where the table lists a mount of the identity's device: its type
    /// (`None`: an anonymous device no mount lists, as a btrfs subvolume's
    /// or an overlay layer's).
    pub identity_fs: Option<String>,
    /// For a known identity, where an eCryptfs mount is listed: the owner
    /// of the file its name reaches, when that file is the device's (the
    /// same identity) on a mount that shows owners as stored; `None`:
    /// unknown.
    pub owner: Option<u32>,
}

/// Filesystem types with no write path: no upper layer or lower directory
/// that diskgc deletes through can lie on one, and a hard link never
/// crosses filesystems, so a device backed by one of their files holds no
/// other name of it there.
pub const NO_WRITE_TYPES: &[&str] = &["iso9660", "squashfs", "erofs", "cramfs"];

impl LoopBacking {
    /// Whether its identity lies on a filesystem with no write path
    /// ([`NO_WRITE_TYPES`]).
    fn read_only_fs(&self) -> bool {
        self.identity_fs
            .as_deref()
            .is_some_and(|t| NO_WRITE_TYPES.contains(&t))
    }

    /// Whether its identity's device is a listed mount's of a filesystem
    /// that keeps its own files (not an overlay's or eCryptfs's own view).
    fn plain_identity(&self) -> bool {
        self.identity_fs
            .as_deref()
            .is_some_and(|t| !stored_elsewhere(t))
    }
}

/// The lower directory of an eCryptfs mount, from mountinfo's source
/// field: the device `statx` shows for it and its filesystem's type, or
/// why it cannot be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcryptfsLower {
    pub mount_id: u64,
    pub lower: Result<(Dev, String), String>,
}

/// The configured loop devices as the in-use check uses them, every one
/// with its own cause.
#[derive(Debug, Clone, Default)]
pub struct LoopBackings {
    pub backings: Vec<LoopBacking>,
    /// Every eCryptfs mount's lower directory, read when a device is
    /// configured (docs/safety.md, In use: eCryptfs names).
    pub ecryptfs: Vec<EcryptfsLower>,
    /// The run's owners.
    pub owners: Vec<u32>,
}

impl LoopBackings {
    /// The first device whose file could be in any candidate: the scan
    /// clears none.
    pub fn everywhere(&self) -> Option<&LoopBacking> {
        self.backings
            .iter()
            .find(|backing| backing.scope == Scope::Everywhere)
    }

    /// The first device whose file a candidate on mount `mount_id` holding
    /// `objects` may hold ([`Scope`]): it is `in_use_unknown`.
    pub fn doubt(
        &self,
        mount_id: u64,
        objects: impl IntoIterator<Item = DevIno>,
    ) -> Option<&LoopBacking> {
        let by_device = self
            .backings
            .iter()
            .any(|backing| matches!(backing.scope, Scope::Filesystem(_)));
        // Only read the objects when some scope is a filesystem's.
        let devices: BTreeSet<Dev> = if by_device {
            objects.into_iter().map(|id| id.dev).collect()
        } else {
            BTreeSet::new()
        };
        self.backings
            .iter()
            .find(|backing| backing.scope.covers(mount_id, &devices))
    }

    /// The first device whose file may be the unread upper-layer name of a
    /// regular file of a candidate on the overlay mount `mount_id`
    /// (docs/safety.md, In use: Upper-layer names): one whose identity is
    /// known, or whose doubt is a filesystem's (a deleted name's). Not one
    /// on a filesystem with no write path ([`NO_WRITE_TYPES`]: a live
    /// medium), nor one opened through that overlay itself, unless its
    /// identity is a plain filesystem's: its identity is then the
    /// overlay's view of the file, which every name there shows, so it is
    /// matched exactly. The other devices already doubt the candidate by
    /// its mount or everywhere, or hold a file no candidate can.
    pub fn identified(&self, mount_id: u64) -> Option<&LoopBacking> {
        self.backings.iter().find(|backing| {
            if backing.read_only_fs() {
                return false;
            }
            match &backing.scope {
                Scope::Filesystem(_) => true,
                Scope::AllBut(own) if own.contains(&mount_id) && !backing.plain_identity() => false,
                _ => backing.identity.is_some(),
            }
        })
    }

    /// The first device whose file may be another name, on the lower
    /// directory's filesystem, of a file of a candidate on the eCryptfs
    /// mount `mount_id` (docs/safety.md, In use: eCryptfs names), with
    /// why: a known identity on that filesystem whose file is an owner's
    /// or whose owner cannot be told, or a deleted name on it. A file of
    /// another user (root's snaps on the same filesystem) is not: the walk
    /// admits only owners' entries, and eCryptfs shows its lower owner.
    pub fn ecryptfs_alias(&self, mount_id: u64) -> Option<(&LoopBacking, String)> {
        let lower = &self.ecryptfs.iter().find(|e| e.mount_id == mount_id)?.lower;
        let on_lower = |dev: Dev, anonymous: bool| match lower {
            Ok((lower_dev, fs_type)) => {
                dev == *lower_dev
                    || (anonymous && SUBVOLUME_FILESYSTEMS.contains(&fs_type.as_str()))
            }
            Err(_) => true,
        };
        let shown_lower = || match lower {
            Ok((dev, fs_type)) => format!("its lower directory is on {fs_type} device {dev}"),
            Err(why) => format!("its lower directory cannot be told: {why}"),
        };
        self.backings.iter().find_map(|backing| {
            let anonymous = backing.identity_fs.is_none();
            match (&backing.scope, backing.identity) {
                (Scope::Filesystem(dev), _) if on_lower(*dev, false) => Some((
                    backing,
                    format!("its name was deleted there; {}", shown_lower()),
                )),
                (_, Some(id))
                    if !backing.read_only_fs()
                        && backing.owner.is_none_or(|uid| self.owners.contains(&uid))
                        && on_lower(id.dev, anonymous) =>
                {
                    Some((
                        backing,
                        format!(
                            "its identity {id} is {}; {}",
                            backing.owner.map_or_else(
                                || "a file whose owner cannot be told".to_owned(),
                                |uid| format!("a file of uid {uid}, an owner of this run")
                            ),
                            shown_lower()
                        ),
                    ))
                }
                _ => None,
            }
        })
    }

    /// The loop device holding the file `id`, if the device said so.
    pub fn held_by(&self, id: DevIno) -> Option<Dev> {
        self.backings
            .iter()
            .find(|backing| backing.identity == Some(id))
            .map(|backing| backing.device)
    }

    /// The notes of the devices no candidate can hold.
    pub fn notes(&self) -> impl Iterator<Item = &str> {
        self.backings
            .iter()
            .filter(|backing| backing.identity.is_none() && backing.scope == Scope::Nowhere)
            .map(|backing| backing.why.as_str())
    }
}

/// Lists the configured loop devices and judges each backing file (module
/// docs). `owners` are the users whose files candidates may hold (the
/// walk's owners).
pub fn read<H: Host>(host: &H, owners: &[u32]) -> Result<LoopBackings, ScanError> {
    let listed = host.loop_backing_files().map_err(run_error)?;
    let mut backings = LoopBackings::default();
    if listed.is_empty() {
        return Ok(backings);
    }
    let mounts = host.mountinfo().map_err(run_error)?;
    backings.owners = owners.to_vec();
    for (device, text) in listed {
        if let Some(backing) = judge(host, &mounts, owners, device, text)? {
            backings.backings.push(backing);
        }
    }
    backings.ecryptfs = ecryptfs_lowers(host, &mounts)?;
    Ok(backings)
}

/// Every eCryptfs mount's lower directory: mountinfo's source field (the
/// path the mount was given, `ecryptfs_mount`'s `kern_path`) reached by
/// the safe lookup, its `statx` device and filesystem type.
fn ecryptfs_lowers<H: Host>(
    host: &H,
    mounts: &[MountEntry],
) -> Result<Vec<EcryptfsLower>, ScanError> {
    let mut lowers = Vec::new();
    for mount in mounts.iter().filter(|m| m.fs_type == "ecryptfs") {
        let source = Path::new(&mount.source);
        let lower = if !source.is_absolute() {
            Err(format!(
                "its source {} is not an absolute path",
                mount.source
            ))
        } else {
            match resolve_in(host, mounts, source, Want::Directory) {
                Ok(dir) => Ok((dir.stat.dev, dir.mount.fs_type)),
                Err(crate::resolve::ResolveError::Host(e)) if e.is_out_of_descriptors() => {
                    return Err(run_error(e));
                }
                Err(e) => Err(format!("{} cannot be reached ({e})", mount.source)),
            }
        };
        lowers.push(EcryptfsLower {
            mount_id: mount.id,
            lower,
        });
    }
    Ok(lowers)
}

/// The type of a listed mount of `dev` (mountinfo's device), if one is.
fn listed_type(mounts: &[MountEntry], dev: Dev) -> Option<String> {
    mounts
        .iter()
        .find(|mount| mount.dev == dev)
        .map(|mount| mount.fs_type.clone())
}

/// The owner of the file the device's printed name reaches, when it is
/// the device's own file (identity `id`) on a mount that shows owners as
/// stored: `None` otherwise. Only read where an eCryptfs mount is listed.
fn owner_of<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    text: &Path,
    id: DevIno,
) -> Result<Option<u32>, ScanError> {
    if !mounts.iter().any(|m| m.fs_type == "ecryptfs") || !text.is_absolute() {
        return Ok(None);
    }
    match resolve_in(host, mounts, text, Want::Any) {
        Ok(resolved)
            if resolved.stat.dev_ino() == Some(id) && !may_show_other_owners(&resolved.mount) =>
        {
            Ok(resolved.stat.uid)
        }
        Ok(_) => Ok(None),
        Err(crate::resolve::ResolveError::Host(e)) if e.is_out_of_descriptors() => {
            Err(run_error(e))
        }
        Err(_) => Ok(None),
    }
}

fn run_error(e: HostError) -> ScanError {
    if e.is_out_of_descriptors() {
        ScanError::Failed(with_descriptor_advice(e))
    } else {
        ScanError::Failed(e)
    }
}

/// Where a backing file may lie and why (step 2 of the module docs, and the
/// alias check of step 1).
struct Judgment {
    scope: Scope,
    why: String,
    /// The text may have changed (a lookup that failed): read it again.
    reread: bool,
}

impl Judgment {
    fn new(scope: Scope, why: impl Into<String>) -> Self {
        Self {
            scope,
            why: why.into(),
            reread: false,
        }
    }
}

/// Judges one device; `None`: it was detached meanwhile.
fn judge<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    owners: &[u32],
    device: Dev,
    first: PathBuf,
) -> Result<Option<LoopBacking>, ScanError> {
    let unavailable = match host.loop_backing_identity(device).map_err(run_error)? {
        LoopIdentity::Known(id) => {
            let alias = stacked_alias(mounts, id, &first);
            let why = match alias.scope {
                Scope::Nowhere => format!("its identity {id} is exact"),
                _ => format!(
                    "loop device {device} is backed by {} and its file cannot be told apart \
                     from a candidate: {}. Detach it (`losetup -d`, `udisksctl loop-delete`)",
                    first.display(),
                    alias.why
                ),
            };
            let owner = owner_of(host, mounts, &first, id)?;
            return Ok(Some(LoopBacking {
                device,
                text: first,
                identity: Some(id),
                scope: alias.scope,
                why,
                identity_fs: listed_type(mounts, id.dev),
                owner,
            }));
        }
        LoopIdentity::Detached => return Ok(None),
        LoopIdentity::Unavailable(why) => why,
    };
    let backing = |text: PathBuf, judged: Judgment| {
        let why = match judged.scope {
            Scope::Nowhere => format!(
                "loop device {device}: its backing file {} is in no candidate ({})",
                text.display(),
                judged.why
            ),
            _ => cause(device, &text, &unavailable, &judged.why),
        };
        LoopBacking {
            device,
            text,
            identity: None,
            scope: judged.scope,
            why,
            identity_fs: None,
            owner: None,
        }
    };
    let mut text = first;
    for _ in 0..=LOOP_REREADS {
        let judged = by_name(host, mounts, owners, &text)?;
        if judged.reread {
            match host.loop_backing_file(device).map_err(run_error)? {
                // Detached meanwhile: it holds nothing now.
                None => return Ok(None),
                Some(again) if again != text => {
                    text = again;
                    continue;
                }
                Some(_) => {}
            }
        }
        return Ok(Some(backing(text, judged)));
    }
    let why = format!("its name kept changing over {} reads", LOOP_REREADS + 1);
    Ok(Some(backing(text, Judgment::new(Scope::Everywhere, why))))
}

/// The `in_use_unknown` detail of a device whose file cannot be told apart
/// from a candidate.
fn cause(device: Dev, text: &Path, unavailable: &str, why: &str) -> String {
    format!(
        "loop device {device} is backed by {} and its file cannot be told apart from a \
         candidate: {why}; the device does not say ({unavailable}). Detach it (`losetup -d`, \
         `udisksctl loop-delete`), or run diskgc where it can open the device (root, the \
         `disk` group)",
        text.display()
    )
}

/// What the backing file's name shows (step 2 of the module docs).
fn by_name<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    owners: &[u32],
    text: &Path,
) -> Result<Judgment, ScanError> {
    if let Some(name) = text.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
        let name = Path::new(std::ffi::OsStr::from_bytes(name));
        let parent = name.parent().filter(|p| p.is_absolute());
        let Some(parent) = parent else {
            return Ok(Judgment::new(
                Scope::Everywhere,
                "the name it was opened by was deleted, and has no parent directory",
            ));
        };
        return Ok(match resolve_in(host, mounts, parent, Want::Directory) {
            // An overlay shows its layers' files under its own device or a
            // layer's anonymous one; another link lies on a layer's
            // filesystem under that one's. Neither is the directory's.
            Ok(dir) if stored_elsewhere(&dir.mount.fs_type) => Judgment::new(
                Scope::Everywhere,
                format!(
                    "the name it was opened by was deleted on {} mount {}, which stores its \
                     files in another filesystem's directories, so another link of the file \
                     need not show its device",
                    dir.mount.fs_type,
                    dir.mount.mount_point.display()
                ),
            ),
            Ok(dir) => Judgment::new(
                Scope::Filesystem(dir.stat.dev),
                format!(
                    "the name it was opened by was deleted, and another link of the file can \
                     be anywhere on the filesystem of {}",
                    parent.display()
                ),
            ),
            Err(crate::resolve::ResolveError::Host(e)) if e.is_out_of_descriptors() => {
                return Err(run_error(e));
            }
            Err(e) => Judgment::new(
                Scope::Everywhere,
                format!(
                    "the name it was opened by was deleted, and its directory cannot be \
                     reached ({e})"
                ),
            ),
        });
    }
    if !text.is_absolute() {
        // `d_path` prints a relative text only for an object in no
        // directory (an anonymous inode).
        return Ok(Judgment::new(Scope::Nowhere, "an object in no directory"));
    }
    Ok(match resolve_in(host, mounts, text, Want::Any) {
        Ok(resolved) if may_show_other_owners(&resolved.mount) => Judgment::new(
            Scope::Everywhere,
            format!(
                "its name reaches the file through {} mount {}, which may show another owner \
                 than the one stored",
                shown_type(&resolved.mount),
                resolved.mount.mount_point.display()
            ),
        ),
        Ok(resolved) => match resolved.stat.uid {
            Some(uid) if !owners.contains(&uid) => foreign_owner(mounts, &resolved.mount, uid),
            Some(_) => Judgment::new(
                Scope::Everywhere,
                "its name reaches a file the run's owners hold, which need not be the device's \
                 own (a mount over its directory hides that)",
            ),
            None => Judgment::new(
                Scope::Everywhere,
                "the kernel did not report the owner of the file its name reaches",
            ),
        },
        Err(crate::resolve::ResolveError::Host(e)) if e.is_out_of_descriptors() => {
            return Err(run_error(e));
        }
        Err(e) => Judgment {
            scope: Scope::Everywhere,
            why: format!("its name reaches nothing here ({e})"),
            reread: true,
        },
    })
}

/// A file of uid `uid`, none of the owners', reached on `on` (a mount that
/// shows owners as stored): the walk admits only the owners' entries, so
/// only a candidate on a mount of the same filesystem that may show
/// another owner (an idmapped one, [`may_show_other_owners`]) can hold it
/// (TASK-299 review, mped S1: a mount of another filesystem shows other
/// files). The same filesystem by the device mountinfo names: one device
/// may stand for more than one filesystem (bcachefs after a member's
/// reuse), which only widens the doubt.
fn foreign_owner(mounts: &[MountEntry], on: &MountEntry, uid: u32) -> Judgment {
    let why =
        format!("its name reaches a file of uid {uid}, which no candidate of this run may hold");
    let showing: Vec<&MountEntry> = mounts
        .iter()
        .filter(|mount| mount.dev == on.dev && may_show_other_owners(mount))
        .collect();
    if showing.is_empty() {
        return Judgment::new(Scope::Nowhere, why);
    }
    let shown: Vec<String> = showing
        .iter()
        .map(|mount| {
            format!(
                "{} mount {}",
                shown_type(mount),
                mount.mount_point.display()
            )
        })
        .collect();
    Judgment::new(
        Scope::Mounts(showing.iter().map(|mount| mount.id).collect()),
        format!(
            "{why} as stored, but {} may show it as an owner's",
            shown.join(", ")
        ),
    )
}

/// How a mount that may show other owners is named in messages: its type,
/// and `idmapped` where it is.
fn shown_type(mount: &MountEntry) -> String {
    if mount.idmapped {
        format!("idmapped {}", mount.fs_type)
    } else {
        mount.fs_type.clone()
    }
}

/// Where a file the device identified as `id` may lie under another
/// identity (TASK-299). The kernel takes the identity from the path the
/// file was opened by, and a filesystem stored in another's directories
/// ([`stored_elsewhere`]: overlayfs, eCryptfs, FUSE) reports its own device
/// there, or for overlayfs a layer's anonymous one and perhaps the copy-up
/// origin's inode, never what a name on the underlying filesystem shows.
/// Its own mounts show the file as the device did (`ovl_getattr` is the
/// same call), so candidates on them are matched exactly and the rest are
/// in doubt ([`Scope::AllBut`]). The aliasing filesystem is told from the
/// table alone: the identity's device being such a mount's, or the name
/// the device prints (its parent, when deleted) lying on one; a name the
/// table cannot place leaves every candidate in doubt. [`Scope::Nowhere`]:
/// an identity every name of the file shares.
fn stacked_alias(mounts: &[MountEntry], id: DevIno, text: &Path) -> Judgment {
    let of_superblock = |dev: Dev| -> BTreeSet<u64> {
        mounts
            .iter()
            .filter(|mount| mount.dev == dev)
            .map(|mount| mount.id)
            .collect()
    };
    let aliasing = |mount: &MountEntry, how: String| {
        Judgment::new(
            Scope::AllBut(of_superblock(mount.dev)),
            format!(
                "{how} {} mount {}, which shows files stored in another filesystem's \
                 directories under identities their names there do not have (candidates on \
                 its mounts are matched by it)",
                mount.fs_type,
                mount.mount_point.display()
            ),
        )
    };
    if let Some(mount) = mounts
        .iter()
        .find(|mount| mount.dev == id.dev && stored_elsewhere(&mount.fs_type))
    {
        return aliasing(mount, format!("its identity {id} is the device of"));
    }
    let bytes = text.as_os_str().as_bytes();
    let name = Path::new(std::ffi::OsStr::from_bytes(
        bytes.strip_suffix(b" (deleted)").unwrap_or(bytes),
    ));
    if !name.is_absolute() {
        return Judgment::new(Scope::Nowhere, "an object in no directory");
    }
    match mount_for(mounts, name) {
        Ok(mount) if stored_elsewhere(&mount.fs_type) => {
            aliasing(mount, "it was opened through".to_owned())
        }
        Ok(_) => Judgment::new(Scope::Nowhere, "an identity every name shares"),
        Err(why) => Judgment::new(
            Scope::Everywhere,
            format!(
                "the mount its name {} is on cannot be told ({why:?})",
                name.display()
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mount(id: u64, dev: (u32, u32), at: &str, fs_type: &str) -> MountEntry {
        MountEntry {
            id,
            parent_id: if id == 1 { 0 } else { 1 },
            dev: Dev {
                major: dev.0,
                minor: dev.1,
            },
            fs_root: PathBuf::from("/"),
            mount_point: PathBuf::from(at),
            fs_type: fs_type.to_owned(),
            source: fs_type.to_owned(),
            read_only: false,
            idmapped: false,
            noatime: false,
            upper_dir: None,
        }
    }

    /// TASK-299: overlayfs without `xino` on layers of several filesystems
    /// shows a layer's file under that layer's anonymous device, which no
    /// mount lists: only the name the device prints shows the overlay. And
    /// an overlay's own device tells it whatever the name says (another
    /// namespace's rendering). Candidates on the overlay's own mounts see
    /// the identity the device gave, so only the others are in doubt.
    /// Plain filesystems, bind mounts and btrfs subvolumes (whose `statx`
    /// device the table does not list either) keep an exact identity.
    #[test]
    fn an_identity_shown_through_a_stacking_filesystem_is_an_alias() {
        let mounts = [
            mount(1, (254, 1), "/", "ext4"),
            mount(60, (0, 60), "/merged", "overlay"),
            mount(63, (0, 60), "/again", "overlay"),
            mount(61, (0, 61), "/home/u", "ecryptfs"),
            mount(62, (0, 62), "/data", "btrfs"),
        ];
        let id = |major, minor| DevIno {
            dev: Dev { major, minor },
            ino: 12,
        };
        let pseudo = id(0, 99);
        let all_but = |ids: &[u64]| Scope::AllBut(ids.iter().copied().collect());
        for (identity, text, scope) in [
            (pseudo, "/merged/img", all_but(&[60, 63])),
            (pseudo, "/merged/img (deleted)", all_but(&[60, 63])),
            (id(0, 60), "/ns/img", all_but(&[60, 63])),
            (id(0, 61), "/home/u/img", all_but(&[61])),
            (id(254, 1), "/srv/img", Scope::Nowhere),
            (id(254, 1), "/srv/img (deleted)", Scope::Nowhere),
            (id(0, 77), "/data/sub/img", Scope::Nowhere),
            (pseudo, "anon_inode:[loop]", Scope::Nowhere),
        ] {
            assert_eq!(
                stacked_alias(&mounts, identity, Path::new(text)).scope,
                scope,
                "{identity} by {text}"
            );
        }
        // A table that cannot place the name: in doubt everywhere.
        assert_eq!(
            stacked_alias(&mounts[1..], id(254, 1), Path::new("/srv/img")).scope,
            Scope::Everywhere
        );
    }

    /// TASK-299 review (mped S2): each scope covers what it says, and
    /// `doubt` names the first device in doubt for a candidate; a scope by
    /// filesystem reads the candidate's objects, the others its mount.
    #[test]
    fn each_scope_covers_its_candidates() {
        let backing = |minor, scope| LoopBacking {
            device: Dev { major: 7, minor },
            text: PathBuf::from("/x"),
            identity: None,
            scope,
            why: format!("device {minor}"),
            identity_fs: None,
            owner: None,
        };
        let object = |minor| DevIno {
            dev: Dev { major: 254, minor },
            ino: 2,
        };
        let backings = LoopBackings {
            backings: vec![
                backing(0, Scope::Nowhere),
                backing(
                    1,
                    Scope::Filesystem(Dev {
                        major: 254,
                        minor: 9,
                    }),
                ),
                backing(2, Scope::Mounts([40].into())),
                backing(3, Scope::AllBut([60, 40].into())),
            ],
            ..LoopBackings::default()
        };
        let doubt = |mount, objects: &[DevIno]| {
            backings
                .doubt(mount, objects.iter().copied())
                .map(|b| b.device.minor)
        };
        assert_eq!(doubt(60, &[object(1)]), None);
        assert_eq!(doubt(60, &[object(1), object(9)]), Some(1));
        assert_eq!(doubt(40, &[object(1)]), Some(2));
        assert_eq!(doubt(30, &[object(1)]), Some(3));
        assert!(backings.everywhere().is_none());
        assert_eq!(backings.notes().collect::<Vec<_>>(), ["device 0"]);
        let all = LoopBackings {
            backings: vec![backing(4, Scope::Everywhere)],
            ..LoopBackings::default()
        };
        assert_eq!(all.everywhere().map(|b| b.device.minor), Some(4));
        assert_eq!(
            all.doubt(60, std::iter::empty()).map(|b| b.device.minor),
            Some(4)
        );
    }
}
