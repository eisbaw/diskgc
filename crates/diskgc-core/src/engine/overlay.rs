//! Subvolume boundaries in an overlay's upper layer (TASK-307 review,
//! Codex blocker; docs/architecture.md, Filesystem matching: Subvolume
//! boundaries).
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
//! Limits: `upperdir=` is the mounter's string at mount time, in its
//! namespace: a name that now reaches another directory (the upper layer
//! moved, or a mount made over it since) is not detected; and a kernel
//! before Linux 6.10 reports no `stx_subvol`, so with an unreachable upper
//! layer a nested btrfs subvolume there goes unseen.

use std::path::{Path, PathBuf};

use super::{Reason, SUBVOLUME_FILESYSTEMS, Skip, Volume, same_subvolume};
use crate::host::{Access, Base, Errno, Host, MountEntry};
use crate::resolve::{Want, beneath_how, resolve_in};

/// What a candidate on an overlay needs proved about its directories.
pub(super) enum Upper<F> {
    /// Not on an overlay, an overlay without an upper layer (read-only:
    /// nothing is removed), or one whose upper filesystem has no
    /// subvolumes.
    None,
    /// The upper layer reached: the directory showing the overlay mount's
    /// root, its filesystem type, where it was reached and the overlay's
    /// mount point.
    Reached {
        root: F,
        fs_type: String,
        at: PathBuf,
        mount_point: PathBuf,
    },
    /// The upper layer cannot be reached by its `upperdir=`: why.
    Unreached(String),
}

/// What the walk checks for each directory of a candidate on an overlay.
pub(super) enum State<F> {
    /// Nothing: not an overlay, no upper layer or none with subvolumes, a
    /// candidate that is not a directory, or a lower-only one (whose
    /// directories have no upper entries either).
    Off,
    /// Each directory with an upper entry must be in `candidate`, the
    /// candidate's upper entry's volume.
    Upper {
        root: F,
        fs_type: String,
        at: PathBuf,
        mount_point: PathBuf,
        candidate: Volume,
    },
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
        Ok(resolved) if !SUBVOLUME_FILESYSTEMS.contains(&resolved.mount.fs_type.as_str()) => {
            Upper::None
        }
        Ok(resolved) => Upper::Reached {
            root: resolved.fd,
            fs_type: resolved.mount.fs_type,
            at,
            mount_point: mount.mount_point.clone(),
        },
        Err(e) => Upper::Unreached(format!(
            "its upperdir= {} cannot be reached: {e}",
            at.display()
        )),
    }
}

/// The upper layer's volume of the directory the overlay shows at `path`
/// (below `mount_point`), from `root`: `None` when it has no upper entry
/// (lower-only), `Err` when that cannot be told. Reached beneath `root`
/// without a symlink or a mount crossing.
pub(super) fn upper_volume<H: Host>(
    host: &H,
    root: &H::Fd,
    mount_point: &Path,
    path: &Path,
) -> Result<Option<Volume>, String> {
    let rel = path.strip_prefix(mount_point).map_err(|_| {
        format!(
            "{} is not below its overlay at {}",
            path.display(),
            mount_point.display()
        )
    })?;
    let stat = if rel.as_os_str().is_empty() {
        host.statx(Base::Dir(root), Path::new(""))
    } else {
        match host.openat2(
            Base::Dir(root),
            rel,
            beneath_how(Access::Path, Want::Directory),
        ) {
            Ok(fd) => host.statx(Base::Dir(&fd), Path::new("")),
            Err(e) if e.is(Errno::ENOENT) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        }
    };
    stat.map(|stat| Some(Volume::of(&stat)))
        .map_err(|e| e.to_string())
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
