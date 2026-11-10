//! Which differences between two mount tables can change what a `protect`
//! or `!profile:` enumeration from given roots finds (TASK-300;
//! docs/safety.md, Not protected, not excluded): [`unchanged_for`].
//!
//! The enumeration reads the table only through questions about paths and
//! places it derives from its roots: the mount a path is on and the mounts
//! on its way (`mount_for`, the mount screen, the covered-mount check), the
//! mount points at, inside or directly in a directory by path
//! (`is_mount_point`, `MountedNames`, `mounts_within`), the mounts attached
//! at or inside a place (`mounts_within`, `mount_at`), and every directory
//! above a root's place that some mount shows, with the views showing it
//! (`places_above`, asked for container storage). The paths are a root,
//! what lies below it, its ancestors, and the views of those places. A
//! difference that touches none of these answers changes nothing the
//! enumeration computes, so the matches it found under one table are the
//! ones it finds under the other (the filesystem being the same).
//!
//! What a table difference can be: a mount that came or went, or one whose
//! line changed. A line with the same id and device is the same mount
//! (a mount's filesystem never changes; the 32-bit id is reused, the
//! device of an anonymous filesystem too, but not both for one mount
//! still listed with another device): its mount point or root field moved
//! with a rename of a directory above it, or the mount was moved, or
//! remounted. A rename is a change of the filesystem the table only
//! reflects: it may have moved a directory above a root, which the old
//! and the new line of the mount below it both share with the root
//! (TASK-294: no mount event comes with it). Lines whose id differs, or
//! whose device does, are a mount gone and another come.
//!
//! [`Scope`] is everything the questions can reach in one table: the
//! roots' places and the places of the mounts at or below a root; to a
//! fixed point, every mount of a reached device whose root field overlaps a
//! reached place (it shows the place, a directory above it or a part of it
//! elsewhere), with the places its point is attached at down its parent
//! chain; and the views (paths) through those mounts of every reached
//! place. A changed mount is apart from the scope ([`Scope::apart`]) when
//! no mount is attached on it; its point is not directly in a root, a view
//! or a directory above one (a mount point is also a name the container
//! storage probe sees, so a mount directly in `/` is never apart); the
//! place its point is attached at on its parent lies not at or inside a
//! root's place; and, for a mount still listed with a changed line, no
//! directory below its parent's root field is above both that place and a
//! reached place, and no directory is above both its own root field and a
//! reached place below the root fields of the scope's mounts showing that
//! place (a rename of such a directory would move both; one at or above
//! those root fields would change one of their lines, a difference of its
//! own). A mount of a reached device with a root field that is no absolute
//! path makes the scope an error, as it makes `places_above` one. A mount of the scope is never apart: its point
//! is the start of its own views. A mount outside the scope shows no
//! reached place, so no question about the roots looks at it but by its
//! point (rules above) or by a mount attached on it (none). Such a mount
//! takes no part in any question: removing it from the table, or adding
//! it, leaves every answer for the roots as it was, and the scope too
//! (it is not in it, and no mount of the scope is attached on it). By
//! induction over the changes, the two tables answer alike when every
//! change is apart from its own table's scope and the mounts both list
//! keep their lines and order. Anything else (a table that cannot be
//! placed, duplicate ids, a reordering) counts as a change.
//!
//! Propagation (`shared:`, `master:`) is not parsed into [`MountEntry`]:
//! a propagation change alone is no difference here, as it was none for
//! the whole-table comparison; what it propagates later arrives as mounts
//! of their own.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{Place, attached_on, attachments, by_id, joined, place_of, root_of};
use crate::host::MountEntry;

/// Whether every question the enumeration from `roots` asks of a mount
/// table gets the same answer from `old` and from `new` (module doc).
/// `false` whenever that cannot be shown.
pub fn unchanged_for(old: &[MountEntry], new: &[MountEntry], roots: &[&Path]) -> bool {
    if old == new {
        return true;
    }
    let Some(changes) = changes(old, new) else {
        return false;
    };
    let (Ok(before), Ok(after)) = (Scope::of(old, roots), Scope::of(new, roots)) else {
        return false;
    };
    changes.into_iter().all(|change| match change {
        Change::Gone(was) => before.apart(was, Line::Gone),
        Change::Came(now) => after.apart(now, Line::Came),
        Change::Altered { was, now } => {
            before.apart(was, Line::Altered) && after.apart(now, Line::Altered)
        }
    })
}

/// One difference between two tables.
#[derive(Debug, Clone, Copy)]
enum Change<'m> {
    Gone(&'m MountEntry),
    Came(&'m MountEntry),
    /// The same mount (id and device) with another line.
    Altered {
        was: &'m MountEntry,
        now: &'m MountEntry,
    },
}

/// Which side of a change a line is ([`Scope::apart`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    Gone,
    Came,
    Altered,
}

/// The differences, or `None` when the tables cannot be compared mount by
/// mount: an id listed twice, or the mounts both list with the same line
/// in another order.
fn changes<'m>(old: &'m [MountEntry], new: &'m [MountEntry]) -> Option<Vec<Change<'m>>> {
    let old_by_id = by_id(old);
    let new_by_id = by_id(new);
    if old_by_id.len() != old.len() || new_by_id.len() != new.len() {
        return None;
    }
    let kept = |mount: &MountEntry, other: &HashMap<u64, &MountEntry>| {
        other.get(&mount.id).is_some_and(|line| *line == mount)
    };
    let old_kept = old.iter().filter(|m| kept(m, &new_by_id)).map(|m| m.id);
    let new_kept = new.iter().filter(|m| kept(m, &old_by_id)).map(|m| m.id);
    if !old_kept.eq(new_kept) {
        return None;
    }
    let mut changes = Vec::new();
    for was in old {
        match new_by_id.get(&was.id) {
            None => changes.push(Change::Gone(was)),
            Some(now) if *now == was => {}
            Some(now) if now.dev != was.dev => {
                changes.push(Change::Gone(was));
                changes.push(Change::Came(now));
            }
            Some(now) => changes.push(Change::Altered { was, now }),
        }
    }
    changes.extend(
        new.iter()
            .filter(|now| !old_by_id.contains_key(&now.id))
            .map(Change::Came),
    );
    Some(changes)
}

/// What the questions about `roots` can reach in one table (module doc).
#[derive(Debug)]
struct Scope<'m> {
    by_id: HashMap<u64, &'m MountEntry>,
    /// The ids of the mounts something is mounted on.
    parents: HashSet<u64>,
    roots: Vec<PathBuf>,
    /// The roots' places: what lies at or inside one is asked about.
    root_places: Vec<Place>,
    /// Reached places: the roots' and every place a mount of the scope is
    /// attached at down its parent chain. Of those, only the directories
    /// above (and the names directly in them) are asked about: an
    /// attachment place is the directory a mount covers.
    places: Vec<Place>,
    /// Every view of a reached place through a mount of the scope: asked
    /// about on the way to it and for the names directly in it.
    views: Vec<PathBuf>,
    /// Per reached place, how many leading components of it no rename can
    /// move without changing a line of the scope.
    shown_from: Vec<usize>,
}

impl<'m> Scope<'m> {
    fn of(table: &'m [MountEntry], roots: &[&Path]) -> Result<Self, String> {
        let by_id = by_id(table);
        let mut root_places = roots
            .iter()
            .map(|root| place_of(table, root))
            .collect::<Result<Vec<_>, _>>()?;
        // A mount below a root shows its own filesystem there: what lies
        // inside it is below the root by path, whatever its place.
        for mount in table {
            if roots.iter().any(|root| mount.mount_point.starts_with(root)) {
                root_of(mount)?;
                root_places.push(Place {
                    dev: mount.dev,
                    path: mount.fs_root.clone(),
                });
            }
        }
        let mut places = root_places.clone();
        let mut in_scope = vec![false; table.len()];
        // Each pass adds a mount or ends: at most one pass per mount, and
        // one more.
        for _ in 0..=table.len() {
            let mut grew = false;
            for (index, mount) in table.iter().enumerate() {
                if in_scope[index] || !shows_a_place(mount, &places) {
                    continue;
                }
                root_of(mount)?;
                in_scope[index] = true;
                grew = true;
                attachments(&by_id, mount, &mut places)?;
            }
            if !grew {
                break;
            }
        }
        places.sort();
        places.dedup();
        // `places_above` reads the root field of every mount of a reached
        // device before it compares it, and fails on one that is not an
        // absolute path: so does the scope (mped review), whether or not
        // the mount shows a reached place.
        for mount in table {
            if places.iter().any(|place| place.dev == mount.dev) {
                root_of(mount)?;
            }
        }
        let mut views = Vec::new();
        // Per place, the deepest root field of a mount of the scope showing
        // it: a rename at or above that directory changes that mount's
        // line (a difference of its own, never apart).
        let mut shown_from = vec![1; places.len()];
        for (mount, _) in table.iter().zip(&in_scope).filter(|(_, inside)| **inside) {
            for (place, depth) in places.iter().zip(shown_from.iter_mut()) {
                if place.dev != mount.dev {
                    continue;
                }
                if let Ok(below) = place.path.strip_prefix(&mount.fs_root) {
                    views.push(joined(&mount.mount_point, below));
                    *depth = (*depth).max(mount.fs_root.components().count());
                }
            }
        }
        views.sort();
        views.dedup();
        let parents = table
            .iter()
            .filter(|mount| mount.parent_id != mount.id)
            .map(|mount| mount.parent_id)
            .collect();
        Ok(Self {
            parents,
            by_id,
            roots: roots.iter().map(|root| root.to_path_buf()).collect(),
            root_places,
            places,
            views,
            shown_from,
        })
    }

    /// Whether `mount`, a line of this scope's table that differs from the
    /// other table (`line`), takes no part in any question (module doc).
    fn apart(&self, mount: &MountEntry, line: Line) -> bool {
        if self.parents.contains(&mount.id) {
            return false;
        }
        let point = &mount.mount_point;
        let Some(dir) = point.parent() else {
            return false;
        };
        // On the way to a root or a view, or a name directly in a directory
        // on that way. (A mount at or below a root is on a reached device:
        // its own place is one of the roots', above.)
        if self
            .roots
            .iter()
            .chain(&self.views)
            .any(|path| path.starts_with(dir))
        {
            return false;
        }
        // Where its point is on its parent mount. Only this first link is
        // asked about by place (`mounts_within` looks one parent up); a
        // change further down the chain is the change of a mount there,
        // judged on its own.
        let Some(parent) = self
            .by_id
            .get(&mount.parent_id)
            .filter(|parent| parent.id != mount.id)
        else {
            return true;
        };
        let Ok(at) = attached_on(parent, mount) else {
            return false;
        };
        if self.root_places.iter().any(|place| at.within(place)) {
            return false;
        }
        // A line that changed with the same mount may show a rename of a
        // directory: one above both where its point is attached and a
        // reached place, below the parent's root field (else the parent's
        // line changed); or one above both its own root field and a
        // reached place, below the root fields of the scope's mounts
        // showing that place (else one of their lines changed).
        if line != Line::Altered {
            return true;
        }
        let parent_root_depth = parent.fs_root.components().count();
        !self
            .places
            .iter()
            .zip(&self.shown_from)
            .any(|(place, shown_from)| {
                (place.dev == at.dev && common_depth(&at.path, &place.path) > parent_root_depth)
                    || (place.dev == mount.dev
                        && common_depth(&mount.fs_root, &place.path) > *shown_from)
            })
    }
}

/// Whether `mount` shows a reached place, an ancestor or a part of one:
/// its device's, its root field and the place's path one inside the other.
fn shows_a_place(mount: &MountEntry, places: &[Place]) -> bool {
    places.iter().any(|place| {
        place.dev == mount.dev
            && (place.path.starts_with(&mount.fs_root) || mount.fs_root.starts_with(&place.path))
    })
}

/// How many leading components (`/` counts as one) `a` and `b` share.
fn common_depth(a: &Path, b: &Path) -> usize {
    a.components()
        .zip(b.components())
        .take_while(|(x, y)| x == y)
        .count()
}

#[cfg(test)]
mod tests;
