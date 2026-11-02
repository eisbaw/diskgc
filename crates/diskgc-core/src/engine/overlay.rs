//! An overlay's upper layer, which a deletion through the overlay changes
//! (docs/architecture.md, Filesystem matching: Subvolume boundaries, TASK-307;
//! docs/safety.md, In use: Upper-layer names, TASK-305).
//!
//! The walk reaches the layer by the overlay's `upperdir=`
//! ([`MountEntry::upper_dir`]) and reads each entry's upper entry by the
//! same name below it: its volume proves a directory's subvolume boundary
//! where the upper filesystem has subvolumes, and its identity is the
//! entry's upper name, which the in-use check matches against processes
//! and loop devices. A layer that cannot be trusted to be the real one is
//! [`Walk::Unreached`]: every name there is [`UpperName::Unknown`], and a
//! directory whose `stx_subvol` through the overlay differs from its
//! candidate's is refused (fail closed).

use std::path::{Path, PathBuf};

use super::{Reason, SUBVOLUME_FILESYSTEMS, Skip, UpperLayer, UpperName, Volume, same_subvolume};
use crate::host::{Access, Base, Errno, FileType, Host, HostError, MountEntry, Stat};
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

/// What a walk on a candidate's mount knows of its upper layer: one state,
/// so a subvolume proof cannot exist without the layer it was made in.
pub(super) enum Walk<F> {
    /// Not an overlay, or an overlay without an upper layer (read-only:
    /// nothing is removed).
    Off,
    /// The upper layer reached. `subvolume`: the candidate's upper entry's
    /// volume, which each directory with an upper entry must be in, where
    /// the layer has subvolumes and the candidate is a directory with an
    /// upper entry; `None`: no proof needed.
    Reached {
        layer: Layer<F>,
        subvolume: Option<Volume>,
    },
    /// The upper layer cannot be reached, or not trusted (`why`): each
    /// directory's `stx_subvol` through the overlay must be `subvol`, the
    /// candidate's.
    Unreached { why: String, subvol: Option<u64> },
}

/// The upper layer of `mount`, the candidate's mount, as `Walk` before
/// the candidate's own proof ([`Walk::Reached`] with no `subvolume`).
pub(super) fn upper_of<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    mount: &MountEntry,
) -> Walk<H::Fd> {
    if mount.fs_type != "overlay" {
        return Walk::Off;
    }
    let Some(dir) = &mount.upper_dir else {
        return Walk::Off;
    };
    let unreached = |why: String| Walk::Unreached { why, subvol: None };
    if !dir.is_absolute() {
        return unreached(format!(
            "its upperdir= {} is not an absolute path",
            dir.display()
        ));
    }
    let at = match mount.fs_root.strip_prefix("/") {
        Ok(rel) => dir.join(rel),
        Err(_) => {
            return unreached(format!(
                "its root field {} is not an absolute path",
                mount.fs_root.display()
            ));
        }
    };
    if let Some(cover) = covered_since(mounts, mount, &at) {
        return unreached(format!(
            "mount {} at {} was made after the overlay, so its upperdir= {} may name a \
             directory that covers the upper layer",
            cover.id,
            cover.mount_point.display(),
            at.display()
        ));
    }
    match resolve_in(host, mounts, &at, Want::Directory) {
        // An upper layer is never an overlay (the kernel refuses one), so
        // a name reaching one, this overlay included, is not the layer.
        Ok(resolved) if resolved.mount.fs_type == "overlay" || resolved.mount.dev == mount.dev => {
            unreached(format!(
                "its upperdir= {} reaches {} mount {}, not a layer",
                at.display(),
                resolved.mount.fs_type,
                resolved.mount.mount_point.display()
            ))
        }
        Ok(resolved) => Walk::Reached {
            layer: Layer {
                root: resolved.fd,
                fs_type: resolved.mount.fs_type,
                at,
                mount_point: mount.mount_point.clone(),
            },
            subvolume: None,
        },
        Err(e) => unreached(format!(
            "its upperdir= {} cannot be reached: {e}",
            at.display()
        )),
    }
}

/// A mount listed after the overlay `overlay` whose mount point is `at` or
/// one of its ancestors: made after the overlay, it may cover the upper
/// layer `upperdir=` named (a decoy). The table lists a namespace's mounts
/// in the order of their 64-bit unique ids, which are never reused (Linux
/// 6.18 `fs/namespace.c`: `mnt_add_to_ns`, `m_start`); the 32-bit mount id
/// is reused lowest-free and orders nothing.
fn covered_since<'m>(
    mounts: &'m [MountEntry],
    overlay: &MountEntry,
    at: &Path,
) -> Option<&'m MountEntry> {
    let position = mounts.iter().position(|m| m.id == overlay.id)?;
    mounts[position + 1..]
        .iter()
        .find(|m| at.starts_with(&m.mount_point))
}

impl<F> Layer<F> {
    /// Whether directories of this layer need the subvolume proof.
    pub(super) fn has_subvolumes(&self) -> bool {
        SUBVOLUME_FILESYSTEMS.contains(&self.fs_type.as_str())
    }

    /// The upper name of what the overlay shows at `path`; `None` when
    /// `path` is not below the overlay's mount point.
    pub(super) fn name_of(&self, path: &Path) -> Option<PathBuf> {
        super::upper_path(&self.at, &self.mount_point, path)
    }

    /// The layer as a walk record keeps it, with the first reason an
    /// entry's upper name could not be read.
    pub(super) fn shown(&self, unread: Option<String>) -> UpperLayer {
        UpperLayer::Reached {
            at: self.at.clone(),
            mount_point: self.mount_point.clone(),
            unread,
        }
    }
}

/// The upper entry of what the overlay shows at `path`, reached from the
/// layer's root beneath it without a symlink or a mount crossing: `None`
/// when it has none (lower-only).
pub(super) fn upper_stat<H: Host>(
    host: &H,
    layer: &Layer<H::Fd>,
    path: &Path,
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
    match host.openat2(
        Base::Dir(&layer.root),
        rel,
        beneath_how(Access::Path, Want::Any),
    ) {
        Ok(fd) => host.statx(Base::Dir(&fd), Path::new("")).map(Some),
        Err(e) if e.is(Errno::ENOENT) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The upper name of the entry the overlay shows at `path` from its upper
/// stat `found` ([`upper_stat`]), with why it is [`UpperName::Unknown`]
/// where it is.
pub(super) fn upper_name<F>(
    layer: &Layer<F>,
    path: &Path,
    found: &Result<Option<Stat>, HostError>,
) -> (UpperName, Option<String>) {
    let shown = || {
        layer
            .name_of(path)
            .unwrap_or_else(|| path.to_owned())
            .display()
            .to_string()
    };
    match found {
        Ok(None) => (UpperName::None, None),
        Ok(Some(stat)) => match stat.dev_ino() {
            Some(id) => (UpperName::Known(id), None),
            None => (
                UpperName::Unknown,
                Some(format!(
                    "the kernel did not report the inode of its upper entry {}",
                    shown()
                )),
            ),
        },
        Err(e) => (
            UpperName::Unknown,
            Some(format!("its upper entry {} cannot be read: {e}", shown())),
        ),
    }
}

/// Whether an entry of `file_type` has an upper name worth reading: any
/// object a process can hold or a loop device can be backed by (not a
/// symlink, which nothing holds by its own inode).
pub(super) fn has_upper_name(file_type: FileType) -> bool {
    file_type != FileType::Symlink
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
