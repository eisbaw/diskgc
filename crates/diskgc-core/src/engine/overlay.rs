//! An overlay's upper layer, which a deletion through the overlay changes:
//! subvolume boundaries in it (TASK-307 review, Codex blocker;
//! docs/architecture.md, Filesystem matching: Subvolume boundaries), and the
//! upper names of regular files (TASK-305; docs/safety.md, In use: Loop
//! devices).
//!
//! overlayfs shows every directory under its own device, so the device rule
//! never fires through it, and it passes the real entry's `stx_subvol`
//! through (`ovl_getattr` fills the stat from the upper entry, else the
//! lower one, and overrides only the device and inode), so a difference
//! between two directories may be a layer difference, harmless, or a
//! subvolume of the upper layer, which an `rmdir` through the overlay
//! removes: a pure upper directory by `ovl_remove_upper` -> `ovl_do_rmdir`
//! -> `vfs_rmdir`, a merged one by `ovl_remove_and_whiteout`, which
//! exchanges it into the work directory and removes it there
//! (`ovl_cleanup_and_whiteout` -> `ovl_cleanup`), and `btrfs_rmdir` deletes
//! an empty subvolume's top. A lower-only directory has no upper entry:
//! its removal renames a whiteout into place and leaves the lower layer
//! alone (Linux 6.18 `fs/overlayfs/dir.c`).
//!
//! So the boundary is proved in the upper layer itself, reached by the
//! overlay's `upperdir=` ([`MountEntry::upper_dir`]): a directory with an
//! upper entry must be in its candidate's upper subvolume, by device and
//! `stx_subvol` ([`super::same_subvolume`] on the upper filesystem's type);
//! one without is lower-only. An upper filesystem that has no subvolumes
//! (not btrfs or bcachefs) needs no proof. Where the upper layer cannot be
//! reached by that name, a directory whose `stx_subvol` through the overlay
//! differs from its candidate's is refused: it cannot be proved a lower
//! layer's (fail closed).
//!
//! A regular file the overlay shows from its upper layer is shown under the
//! overlay's device (all layers on one filesystem, or `xino`) or the upper
//! layer's anonymous one, never the upper filesystem's (`ovl_map_dev_ino`),
//! and an unlink through the overlay removes its upper name: by
//! `ovl_remove_upper` -> `ovl_do_unlink` for a pure upper file, by renaming
//! a whiteout over it (`ovl_cleanup_and_whiteout`) for a copied-up one. A
//! lower-only file has no upper name, and its removal leaves the lower layer
//! alone (Linux 6.18 `fs/overlayfs/dir.c`, `ovl_do_remove`). So the walk
//! reads each regular file's upper entry by the same name below the upper
//! layer ([`upper_name`]): its identity is what a loop device opened by a
//! plain name of that file reports.
//!
//! Limits: `upperdir=` is the mounter's string at mount time, in its
//! namespace: a name that now reaches another directory (the upper layer
//! moved, or a mount made over it since) is not detected; and a kernel
//! before Linux 6.10 reports no `stx_subvol`, so with an unreachable upper
//! layer a nested btrfs subvolume there goes unseen.

use std::path::{Path, PathBuf};

use super::{Reason, SUBVOLUME_FILESYSTEMS, Skip, UpperLayer, UpperName, Volume, same_subvolume};
use crate::host::{Access, Base, Errno, Host, HostError, MountEntry, Stat};
use crate::resolve::{Want, beneath_how, resolve_in};

/// An overlay's upper layer, reached by its `upperdir=`.
pub(super) struct Layer<F> {
    /// The directory showing the overlay mount's root.
    pub root: F,
    /// The upper layer's filesystem type.
    pub fs_type: String,
    /// Where it was reached.
    pub at: PathBuf,
    /// The overlay's mount point.
    pub mount_point: PathBuf,
}

/// The upper layer of a candidate's mount.
pub(super) enum Upper<F> {
    /// Not on an overlay, or an overlay without an upper layer (read-only:
    /// nothing is removed).
    None,
    /// The upper layer reached.
    Reached(Layer<F>),
    /// The upper layer cannot be reached by its `upperdir=`: why.
    Unreached(String),
}

/// What the walk checks for each directory of a candidate on an overlay.
pub(super) enum State {
    /// Nothing: not an overlay, no upper layer or none with subvolumes, a
    /// candidate that is not a directory, or a lower-only one (whose
    /// directories have no upper entries either).
    Off,
    /// Each directory with an upper entry must be in `candidate`, the
    /// candidate's upper entry's volume, in the reached [`Layer`].
    Upper { candidate: Volume },
    /// The upper layer cannot be reached (`why`): each directory's
    /// `stx_subvol` through the overlay must be the candidate's.
    Unreached { why: String, candidate: Option<u64> },
}

/// The upper layer of `mount`, the candidate's mount.
pub(super) fn upper_of<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    mount: &MountEntry,
) -> Upper<H::Fd> {
    if mount.fs_type != "overlay" {
        return Upper::None;
    }
    let Some(dir) = &mount.upper_dir else {
        return Upper::None;
    };
    if !dir.is_absolute() {
        return Upper::Unreached(format!(
            "its upperdir= {} is not an absolute path",
            dir.display()
        ));
    }
    let at = match mount.fs_root.strip_prefix("/") {
        Ok(rel) => dir.join(rel),
        Err(_) => {
            return Upper::Unreached(format!(
                "its root field {} is not an absolute path",
                mount.fs_root.display()
            ));
        }
    };
    match resolve_in(host, mounts, &at, Want::Directory) {
        Ok(resolved) => Upper::Reached(Layer {
            root: resolved.fd,
            fs_type: resolved.mount.fs_type,
            at,
            mount_point: mount.mount_point.clone(),
        }),
        Err(e) => Upper::Unreached(format!(
            "its upperdir= {} cannot be reached: {e}",
            at.display()
        )),
    }
}

impl<F> Layer<F> {
    /// Whether directories of this layer need the subvolume proof.
    pub(super) fn has_subvolumes(&self) -> bool {
        SUBVOLUME_FILESYSTEMS.contains(&self.fs_type.as_str())
    }

    /// The upper name of what the overlay shows at `path`: the layer's
    /// directory plus `path` below the overlay's mount point.
    pub(super) fn name_of(&self, path: &Path) -> PathBuf {
        super::upper_path(&self.at, &self.mount_point, path)
    }

    /// The layer as a walk record keeps it, with the first reason a
    /// regular file's upper name could not be read.
    pub(super) fn shown(&self, unread: Option<String>) -> UpperLayer {
        UpperLayer::Reached {
            at: self.at.clone(),
            mount_point: self.mount_point.clone(),
            unread,
        }
    }
}

/// The upper entry of what the overlay shows at `path` (below
/// `mount_point`), reached from the layer's root beneath it without a
/// symlink or a mount crossing: `None` when it has none (lower-only).
fn upper_stat<H: Host>(
    host: &H,
    layer: &Layer<H::Fd>,
    path: &Path,
    want: Want,
) -> Result<Option<Stat>, HostError> {
    let rel = path.strip_prefix(&layer.mount_point).map_err(|_| {
        HostError::invalid(
            "find the upper entry",
            path.display(),
            format!("not below its overlay at {}", layer.mount_point.display()),
        )
    })?;
    if rel.as_os_str().is_empty() {
        return host.statx(Base::Dir(&layer.root), Path::new("")).map(Some);
    }
    match host.openat2(Base::Dir(&layer.root), rel, beneath_how(Access::Path, want)) {
        Ok(fd) => host.statx(Base::Dir(&fd), Path::new("")).map(Some),
        Err(e) if e.is(Errno::ENOENT) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The upper layer's volume of the directory the overlay shows at `path`:
/// `None` when it has no upper entry (lower-only), `Err` when that cannot
/// be told.
pub(super) fn upper_volume<H: Host>(
    host: &H,
    layer: &Layer<H::Fd>,
    path: &Path,
) -> Result<Option<Volume>, String> {
    upper_stat(host, layer, path, Want::Directory)
        .map(|stat| stat.as_ref().map(Volume::of))
        .map_err(|e| e.to_string())
}

/// The upper name of the regular file the overlay shows at `path`
/// (TASK-305): [`UpperName::None`] when it has none (lower-only: a deletion
/// through the overlay writes a whiteout and leaves the lower name),
/// [`UpperName::Unknown`] when it cannot be read (`why`); diskgc out of
/// file descriptors is an error of the run (`Err`), as everywhere in the
/// walk.
pub(super) fn upper_name<H: Host>(
    host: &H,
    layer: &Layer<H::Fd>,
    path: &Path,
) -> Result<(UpperName, Option<String>), HostError> {
    match upper_stat(host, layer, path, Want::Any) {
        Ok(None) => Ok((UpperName::None, None)),
        Ok(Some(stat)) => match stat.dev_ino() {
            Some(id) => Ok((UpperName::Known(id), None)),
            None => Ok((
                UpperName::Unknown,
                Some(format!(
                    "the kernel did not report the inode of its upper entry {}",
                    layer.name_of(path).display()
                )),
            )),
        },
        Err(e) if e.is_out_of_descriptors() => Err(e),
        Err(e) => Ok((
            UpperName::Unknown,
            Some(format!(
                "its upper entry {} cannot be read: {e}",
                layer.name_of(path).display()
            )),
        )),
    }
}

/// The boundary between the upper entries `inner` (at `path`) and `outer`,
/// on an upper filesystem of `fs_type` reached at `at`, said as such.
pub(super) fn upper_boundary(
    path: &Path,
    inner: Volume,
    outer: Volume,
    fs_type: &str,
    at: &Path,
) -> Result<(), Skip> {
    same_subvolume(path, inner, outer, Some(fs_type)).map_err(|skip| {
        Skip::new(
            skip.reason,
            skip.path,
            format!(
                "{} (in the overlay's upper layer at {}, which an rmdir through the overlay \
                 removes)",
                skip.detail,
                at.display()
            ),
        )
    })
}

/// A directory at `path` that cannot be proved outside an upper-layer
/// subvolume: `contains_mount`, saying why.
pub(super) fn unproved(path: &Path, why: &str) -> Skip {
    Skip::new(
        Reason::ContainsMount,
        path.to_owned(),
        format!(
            "{} is a directory on an overlay whose upper layer cannot be reached or read to \
             check it for subvolumes ({why}): not touched",
            path.display()
        ),
    )
}

/// Where the unreached upper layer leaves the walk: a directory whose
/// `stx_subvol` through the overlay differs from its candidate's.
pub(super) fn unreached(
    path: &Path,
    inner: Option<u64>,
    outer: Option<u64>,
    why: &str,
) -> Result<(), Skip> {
    if inner == outer {
        return Ok(());
    }
    Err(unproved(
        path,
        &format!(
            "{why}; its subvolume {} differs from its candidate's {}, so it may be one of the \
             upper layer's",
            shown(inner),
            shown(outer)
        ),
    ))
}

fn shown(subvol: Option<u64>) -> String {
    subvol.map_or_else(|| "(not reported)".to_owned(), |id| id.to_string())
}
