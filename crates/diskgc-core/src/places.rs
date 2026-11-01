//! Places in filesystems: comparing directories through bind mounts by
//! where they are, not by path text (docs/safety.md: Before the walk; No
//! container storage; Not protected, not excluded).
//!
//! A bind mount shows a directory of its filesystem under another path, so
//! a lexical prefix test misses what lies above, below or at a directory
//! reached another way. A path's place is the device of the mount it is on
//! and its path from that filesystem's root: the mount's mountinfo root
//! field joined with the rest of the path (TASK-260). A filesystem mounted
//! somewhere is attached at a place on its parent mount, and so on down the
//! parent chain ([`attachments`]).
//!
//! Everything here reads the mount table it is given and nothing else; it
//! looks nothing up. A caller that then opens a path named here resolves it
//! on the mount the table names ([`crate::resolve::resolve_in`]), so a
//! covered mount, or a table that changed since it was read, fails there.
//! Only diskgc's own mount namespace's table is read: a mount made in
//! another namespace is not seen (docs/safety.md states the limit).

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::host::{Dev, MountEntry};
use crate::resolve::Unsafe;

/// The mount a path is on: the deepest mount point that is a (component-
/// wise) prefix of `path`; of mounts stacked on that point, the topmost (the
/// one no other mount there is stacked on).
pub fn mount_for<'m>(mounts: &'m [MountEntry], path: &Path) -> Result<&'m MountEntry, Unsafe> {
    let depth = |m: &MountEntry| m.mount_point.components().count();
    let deepest = mounts
        .iter()
        .filter(|m| path.starts_with(&m.mount_point))
        .map(depth)
        .max()
        .ok_or(Unsafe::NoMount)?;
    let at_point: Vec<&MountEntry> = mounts
        .iter()
        .filter(|m| path.starts_with(&m.mount_point) && depth(m) == deepest)
        .collect();
    let topmost: Vec<&MountEntry> = at_point
        .iter()
        .copied()
        .filter(|m| !at_point.iter().any(|other| other.parent_id == m.id))
        .collect();
    match topmost.as_slice() {
        [one] => Ok(one),
        _ => Err(Unsafe::AmbiguousMounts(
            at_point.iter().map(|m| m.id).collect(),
        )),
    }
}

/// Where `path`, a path on `mount` (at or below its mount point), is in
/// that mount's filesystem: the mount's root field (mountinfo field 4)
/// joined with the rest of `path`. `None` when `path` is not at or below the
/// mount point or the root field is not an absolute path: nothing can be
/// translated then. Nothing is looked up.
pub fn place_in_filesystem(mount: &MountEntry, path: &Path) -> Option<PathBuf> {
    let below = path.strip_prefix(&mount.mount_point).ok()?;
    // No trailing `/` for the mount point itself (`Path` compares and
    // hashes by components, so this is only for what is shown).
    mount.fs_root.is_absolute().then(|| {
        if below.as_os_str().is_empty() {
            mount.fs_root.clone()
        } else {
            mount.fs_root.join(below)
        }
    })
}

/// A directory's place: the device of its filesystem and its path from
/// that filesystem's root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Place {
    pub dev: Dev,
    pub path: PathBuf,
}

impl Place {
    /// Whether this place is `other` or lies inside it.
    pub fn within(&self, other: &Place) -> bool {
        self.dev == other.dev && self.path.starts_with(&other.path)
    }
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} on device {}", self.path.display(), self.dev)
    }
}

/// The table by mount id.
pub fn by_id(mounts: &[MountEntry]) -> HashMap<u64, &MountEntry> {
    mounts.iter().map(|mount| (mount.id, mount)).collect()
}

/// The mount's root field, which must be an absolute path to translate
/// anything on that filesystem.
fn root_of(mount: &MountEntry) -> Result<&Path, String> {
    if mount.fs_root.is_absolute() {
        Ok(&mount.fs_root)
    } else {
        Err(format!(
            "mount {} has filesystem root {}, not an absolute path",
            mount.mount_point.display(),
            mount.fs_root.display()
        ))
    }
}

/// `path`'s place, from the table alone: the mount it is on
/// ([`mount_for`]) and that mount's root field joined with the rest of the
/// path. An unclear table or a root field that is not absolute is the
/// error (the caller fails closed).
pub fn place_of(mounts: &[MountEntry], path: &Path) -> Result<Place, String> {
    let mount = mount_for(mounts, path).map_err(|why| why.to_string())?;
    root_of(mount)?;
    let path = place_in_filesystem(mount, path).expect("mount_for returns a prefix mount");
    Ok(Place {
        dev: mount.dev,
        path,
    })
}

/// The places `mount`'s point is attached at, down its parent chain: on
/// each parent, the parent's root field joined with the child's point below
/// the parent's (a mount stacked on a point: the parent's root). The chain
/// ends at a mount whose parent the table does not list (another
/// namespace's, or outside the process's root) or that is its own parent
/// (the root of the namespace's tree). A child point not below its
/// parent's, or a chain longer than the table, cannot be placed: the error.
pub fn attachments(
    by_id: &HashMap<u64, &MountEntry>,
    mount: &MountEntry,
    out: &mut Vec<Place>,
) -> Result<(), String> {
    let mut child = mount;
    for _ in 0..=by_id.len() {
        let Some(parent) = by_id
            .get(&child.parent_id)
            .filter(|parent| parent.id != child.id)
        else {
            return Ok(());
        };
        out.push(attached_on(parent, child)?);
        child = parent;
    }
    Err(format!(
        "the parent chain of mount {} does not end",
        mount.mount_point.display()
    ))
}

/// Where `child`'s point is on its parent mount `parent`.
fn attached_on(parent: &MountEntry, child: &MountEntry) -> Result<Place, String> {
    let path = place_in_filesystem(parent, &child.mount_point).ok_or_else(|| {
        format!(
            "mount {} cannot be placed on its parent {} (root field {})",
            child.mount_point.display(),
            parent.mount_point.display(),
            parent.fs_root.display()
        )
    })?;
    Ok(Place {
        dev: parent.dev,
        path,
    })
}

/// Every way the table shows a path's place: the place; the mount points
/// of the mounts of its device whose root field overlaps it (one showing
/// the place, or part of it); and the places those mount points are
/// attached at, down their parent chains. Sorted and deduplicated, so two
/// tables that show the place the same way give equal values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ancestry {
    pub place: Place,
    pub shown_at: Vec<PathBuf>,
    pub attached_at: Vec<Place>,
}

/// [`Ancestry`] of `path` in the table `mounts`.
pub fn ancestry(mounts: &[MountEntry], path: &Path) -> Result<Ancestry, String> {
    let by_id = by_id(mounts);
    let place = place_of(mounts, path)?;
    let mut shown_at = Vec::new();
    let mut attached_at = Vec::new();
    for mount in mounts.iter().filter(|mount| mount.dev == place.dev) {
        let root = root_of(mount)?;
        if place.path.starts_with(root) || root.starts_with(&place.path) {
            shown_at.push(mount.mount_point.clone());
            attachments(&by_id, mount, &mut attached_at)?;
        }
    }
    shown_at.sort();
    shown_at.dedup();
    attached_at.sort();
    attached_at.dedup();
    Ok(Ancestry {
        place,
        shown_at,
        attached_at,
    })
}

/// A mount inside the directory `path`, by path or by place: one whose
/// point is at or below `path`, or one attached, on any mount of `path`'s
/// filesystem, at a place at or inside `path`'s place (TASK-290: through a
/// non-recursive bind of its parent, a mount point is a plain directory
/// there, which Linux still refuses to remove, `EBUSY`). Returns the mount
/// and where it lies in `path`'s view; the first by path, else by place,
/// in table order ([`mounts_within`]).
pub fn mount_within<'m>(
    mounts: &'m [MountEntry],
    path: &Path,
) -> Result<Option<(&'m MountEntry, PathBuf)>, String> {
    Ok(mounts_within(mounts, path)?.into_iter().next())
}

/// A mount whose point is the directory `path` itself, by path or by place
/// (attached at `path`'s place on a mount of its filesystem: through a
/// non-recursive bind, the directory it covers). As [`mounts_within`].
pub fn mount_at<'m>(
    mounts: &'m [MountEntry],
    path: &Path,
) -> Result<Option<&'m MountEntry>, String> {
    Ok(mounts_within(mounts, path)?
        .into_iter()
        .find(|(_, at)| at == path)
        .map(|(mount, _)| mount))
}

/// Every mount inside the directory `path` ([`mount_within`]), each with
/// where it lies in `path`'s view: those by path first, then those by place
/// only. A mount whose parent the table does not list is placed by its
/// path only. `path` itself, or a mount on its filesystem's mounts, that
/// cannot be placed is the error.
pub fn mounts_within<'m>(
    mounts: &'m [MountEntry],
    path: &Path,
) -> Result<Vec<(&'m MountEntry, PathBuf)>, String> {
    let mut found: Vec<(&MountEntry, PathBuf)> = mounts
        .iter()
        .filter(|m| m.mount_point.starts_with(path))
        .map(|m| (m, m.mount_point.clone()))
        .collect();
    let by_id = by_id(mounts);
    let place = place_of(mounts, path)?;
    for mount in mounts {
        let Some(parent) = by_id
            .get(&mount.parent_id)
            .filter(|parent| parent.id != mount.id && parent.dev == place.dev)
        else {
            continue;
        };
        let attached = attached_on(parent, mount)?;
        if attached.within(&place) {
            let below = attached
                .path
                .strip_prefix(&place.path)
                .expect("within a place on its device");
            let at = joined(path, below);
            if !found.iter().any(|(_, seen)| *seen == at) {
                found.push((mount, at));
            }
        }
    }
    Ok(found)
}

/// How many places [`places_above`] collects before it gives up: far more
/// than a desktop or CI table needs (the places above one path, over every
/// mount that shows it), small enough to bound a looping table. Beyond it
/// the root is refused (fail closed): a host with tens of thousands of
/// binds of one filesystem (a kubelet node's pod volumes) could reach it.
const PLACES_ABOVE_LIMIT: usize = 4096;

/// Every directory above `path`'s directory in any view the table shows,
/// with a path that reaches it uncovered (TASK-267): the directories above
/// its place on its filesystem; for every mount of that filesystem showing
/// one of them, the places its point is attached at down its parent chain
/// ([`attachments`]), and the directories above those; and so on from each
/// place found. The path's own place and what lies inside it are not above
/// it. Two kinds of place are never asked (mped B1 on the TASK-267 change:
/// a default openSUSE/snapper layout, where `/` shows
/// `/@/.snapshots/1/snapshot` and `/.snapshots`, mode 0750, shows
/// `/@/.snapshots`, refused every root on the volume as unreadable):
///
/// - a place where a mount is attached: the directory the mount covers,
///   which is not the directory above anything shown through the mount
///   (only the directories above it are);
/// - a place above the namespace's root on its filesystem (above `/`'s
///   root field): what `/` shows is taken as the top, as a `chroot` is;
///   nothing there is followed further either.
///
/// Places that no mount of the table shows uncovered are left out: on a
/// btrfs subvolume mounted alone (root field `/@home`), the top-level
/// subvolume above it; on a bind volume of a container, the host's
/// directories above the bound one. The caller states that limit. Sorted by
/// the reaching path, shallowest first.
pub fn places_above(mounts: &[MountEntry], path: &Path) -> Result<Vec<(Place, PathBuf)>, String> {
    let by_id = by_id(mounts);
    let start = place_of(mounts, path)?;
    let top = mount_for(mounts, Path::new("/")).ok().and_then(|root| {
        root_of(root).ok().map(|path| Place {
            dev: root.dev,
            path: path.to_owned(),
        })
    });
    let above_the_top = |place: &Place| {
        top.as_ref()
            .is_some_and(|top| top.within(place) && top.path != place.path)
    };
    // Each place found, and whether it is asked (`false`: only where a
    // mount is attached, so far).
    let mut found: HashMap<Place, bool> = HashMap::from([(start.clone(), false)]);
    let mut todo = vec![start.clone()];
    while let Some(place) = todo.pop() {
        let mut next: Vec<(Place, bool)> = place
            .path
            .ancestors()
            .skip(1)
            .map(|above| {
                let above = Place {
                    dev: place.dev,
                    path: above.to_owned(),
                };
                (above, true)
            })
            .collect();
        for mount in mounts.iter().filter(|mount| mount.dev == place.dev) {
            if place.path.starts_with(root_of(mount)?) {
                let mut attached = Vec::new();
                attachments(&by_id, mount, &mut attached)?;
                next.extend(attached.into_iter().map(|at| (at, false)));
            }
        }
        for (place, ask) in next {
            if above_the_top(&place) {
                continue;
            }
            match found.get_mut(&place) {
                Some(asked) => *asked |= ask,
                None => {
                    found.insert(place.clone(), ask);
                    if found.len() > PLACES_ABOVE_LIMIT {
                        return Err(format!(
                            "more than {PLACES_ABOVE_LIMIT} places above {} in the mount table",
                            path.display()
                        ));
                    }
                    todo.push(place);
                }
            }
        }
    }
    let mut above: Vec<(Place, PathBuf)> = found
        .into_iter()
        .filter(|(place, ask)| *ask && !place.within(&start))
        .filter_map(|(place, _)| view_of(mounts, &place).map(|view| (place, view)))
        .collect();
    above.sort_by(|(_, a), (_, b)| {
        (a.components().count(), a.as_path()).cmp(&(b.components().count(), b.as_path()))
    });
    Ok(above)
}

/// A path that reaches `place` uncovered: through a mount of its device
/// whose root field is at or above it, where no other mount covers it
/// (the path's own place, by the table, is `place`). The first such mount
/// in table order; `None` when the table shows it nowhere.
fn view_of(mounts: &[MountEntry], place: &Place) -> Option<PathBuf> {
    mounts
        .iter()
        .filter(|mount| mount.dev == place.dev)
        .filter_map(|mount| {
            let below = place.path.strip_prefix(root_of(mount).ok()?).ok()?;
            Some(joined(&mount.mount_point, below))
        })
        .find(|view| place_of(mounts, view).is_ok_and(|at| at == *place))
}

/// `base` joined with `below`, which may be empty (no trailing `/`).
fn joined(base: &Path, below: &Path) -> PathBuf {
    if below.as_os_str().is_empty() {
        base.to_owned()
    } else {
        base.join(below)
    }
}

#[cfg(test)]
mod tests;
