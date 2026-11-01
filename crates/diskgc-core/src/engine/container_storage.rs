//! The container-storage recogniser (docs/safety.md, The eligibility walk:
//! no container storage; TASK-242): a fixed structural recogniser, like the
//! Cargo lock names and the checkout markers, independent of the profile
//! library and always on.
//!
//! Rootless container engines keep their image layers and containers below
//! the user's home (`~/.local/share/containers/storage`,
//! `~/.local/share/docker`), and a file a container's root wrote belongs to
//! the user there. A layer that ran `cargo build` holds a Cargo target that
//! discovery would find, the ownership walk would admit and the idle
//! measure would call old: deleting it corrupts the image. Rootful
//! storage (`/var/lib/containers/storage`, `/var/lib/docker`,
//! `/var/lib/containerd`) is the same under `--allow-root`.
//!
//! A storage root is recognised by the names it holds, never by its path:
//! every engine lets its root be moved (`graphroot`, `data-root`, `root`).
//! Each rule is a set of groups of names; a directory is a storage root
//! when, for every group of one rule, it holds at least one name of that
//! group.

//! Where it is asked (docs/safety.md, No container storage): every listing
//! of the eligibility walk and of discovery ([`check_listing`]); every
//! directory between a root and a candidate ([`storage_in`]); and every
//! directory above a root, from `/` down and above its place in its
//! filesystem wherever the mount table shows it ([`storage_above`]), so a
//! root named inside container storage, or bound from inside it, is refused
//! too. Only names count, whatever their type, so a check needs a
//! listing of the directory or one `statx` per name and nothing else, and
//! errs on the side of keeping (a project that happens to hold such names
//! is kept, and the skip names the markers).
//!
//! The rules (docs/safety.md has the evidence):
//!
//! - a containers/storage graphroot (Podman, Buildah, CRI-O, Skopeo):
//!   `storage.lock` and a driver's layer store, `<driver>-layers`;
//! - a containers/storage image store (`imagestore=`, or an additional
//!   image store), which holds no `storage.lock`: a driver's layer store
//!   and image store, `<driver>-layers` and `<driver>-images`;
//! - a Docker (Moby) data root with the classic layer store: `image`, a
//!   graph driver's directory and `containers`;
//! - a Docker data root of 23.0 or later whatever its store: `engine-id`
//!   and `containers`;
//! - a containerd root (also Docker's own, `<data-root>/containerd/daemon`,
//!   with the containerd image store): its metadata or content plugin's
//!   directory.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use super::{Reason, Skip};
use crate::host::{Access, Host, HostError, MountEntry};
use crate::places;
use crate::resolve::{
    MountedNames, ResolveError, Resolved, Unsafe, Want, mount_for, name_present_in, open_beneath,
    resolve_in,
};

/// One kind of storage root and the names that make one: for every group,
/// at least one of its names.
struct Rule {
    what: &'static str,
    groups: &'static [&'static [&'static str]],
}

/// The containers/storage graph drivers' layer stores
/// (`<graphroot>/<driver>-layers`, containers/storage `layers.go`).
const CONTAINERS_STORAGE_LAYERS: &[&str] = &[
    "overlay-layers",
    "vfs-layers",
    "btrfs-layers",
    "zfs-layers",
    "aufs-layers",
    "devicemapper-layers",
];

/// Their image stores (`<driver>-images`, containers/storage `images.go`).
const CONTAINERS_STORAGE_IMAGES: &[&str] = &[
    "overlay-images",
    "vfs-images",
    "btrfs-images",
    "zfs-images",
    "aufs-images",
    "devicemapper-images",
];

/// Moby's graph driver directories (`<data-root>/<driver>`, beside
/// `<data-root>/image/<driver>`); `overlay` is Docker 20's legacy driver.
const DOCKER_GRAPH_DRIVERS: &[&str] = &[
    "overlay2",
    "overlay",
    "fuse-overlayfs",
    "vfs",
    "btrfs",
    "zfs",
    "aufs",
    "devicemapper",
];

const RULES: &[Rule] = &[
    Rule {
        what: "a containers/storage graphroot (Podman, Buildah, CRI-O)",
        groups: &[&["storage.lock"], CONTAINERS_STORAGE_LAYERS],
    },
    Rule {
        what: "a containers/storage image store (Podman, Buildah, CRI-O)",
        groups: &[CONTAINERS_STORAGE_LAYERS, CONTAINERS_STORAGE_IMAGES],
    },
    // `containers` too: the daemon makes it at start, and `image` beside
    // a directory named like a driver is common in source trees (U-Boot's
    // `test/` holds `image/` and `overlay/`, seen on the development
    // machine).
    Rule {
        what: "a Docker data root",
        groups: &[&["image"], DOCKER_GRAPH_DRIVERS, &["containers"]],
    },
    Rule {
        what: "a Docker data root",
        groups: &[&["engine-id"], &["containers"]],
    },
    Rule {
        what: "a containerd root",
        groups: &[&[
            "io.containerd.metadata.v1.bolt",
            "io.containerd.content.v1.content",
        ]],
    },
];

/// A recognised storage root: what kind, and the names that showed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerStorage {
    pub what: &'static str,
    pub markers: Vec<&'static str>,
}

impl fmt::Display for ContainerStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} marked by ", self.what)?;
        for (i, marker) in self.markers.iter().enumerate() {
            if i > 0 {
                f.write_str(" and ")?;
            }
            write!(f, "`{marker}`")?;
        }
        Ok(())
    }
}

impl ContainerStorage {
    /// The refusal of the directory at `at`, which is this storage root.
    pub fn refusal(&self, at: &Path) -> Skip {
        Skip::new(
            Reason::ContainsSpecial,
            at.to_owned(),
            format!(
                "container storage, {self}: its image layers and containers are never searched \
                 or deleted, whoever owns them"
            ),
        )
    }

    /// The refusal of `inside` (a candidate, or a root), which lies inside
    /// this storage root at `at`. The skip's path is the storage root.
    pub fn inside(&self, at: &Path, inside: &Path) -> Skip {
        Skip::new(
            Reason::ContainsSpecial,
            at.to_owned(),
            format!(
                "{} lies inside container storage, {self}: its image layers and containers are \
                 never searched or deleted",
                inside.display()
            ),
        )
    }
}

/// Whether a directory is a storage root, asked one marker name at a time
/// through `has` (a `statx` of the name, or a lookup in a listing). Every
/// rule's first group is asked; a later group only when the earlier ones
/// held. An error of `has` stops it (fail closed is the caller's).
pub fn recognise<E>(
    mut has: impl FnMut(&str) -> Result<bool, E>,
) -> Result<Option<ContainerStorage>, E> {
    'rules: for rule in RULES {
        let mut markers = Vec::with_capacity(rule.groups.len());
        for group in rule.groups {
            let mut found = None;
            for name in *group {
                if has(name)? {
                    found = Some(*name);
                    break;
                }
            }
            match found {
                Some(name) => markers.push(name),
                None => continue 'rules,
            }
        }
        return Ok(Some(ContainerStorage {
            what: rule.what,
            markers,
        }));
    }
    Ok(None)
}

/// Whether a directory whose complete listing is `names` (in any order) is
/// a storage root.
pub fn in_listing(names: &[OsString]) -> Option<ContainerStorage> {
    // One pass keeps the marker names present; the rules then look only
    // at those, so the cost is linear in the listing whatever its order.
    let present: Vec<&OsStr> = names
        .iter()
        .map(OsString::as_os_str)
        .filter(|name| is_marker(name))
        .collect();
    let found: Result<_, std::convert::Infallible> =
        recognise(|marker| Ok(present.iter().any(|name| *name == marker)));
    match found {
        Ok(found) => found,
        Err(never) => match never {},
    }
}

/// The directory at `path`, whose complete listing is `names`, is no
/// storage root; its refusal otherwise (`contains_special`, the storage
/// root as its path). One rule for the walk and both discoveries.
pub fn check_listing(path: &Path, names: &[OsString]) -> Result<(), Skip> {
    match in_listing(names) {
        None => Ok(()),
        Some(storage) => Err(storage.refusal(path)),
    }
}

/// Whether the open directory `dir` (at `path`) is a storage root, asked
/// by [`name_present_in`] (a `statx` per name, not followed; a mount point
/// of that name counts without being touched; the table is searched for the
/// directory's mount points once, [`MountedNames`]). A name that cannot be
/// told is the error, with the path it was asked at: fail closed is the
/// caller's.
pub fn storage_in<H: Host>(
    host: &H,
    dir: &H::Fd,
    path: &Path,
    mounts: &[MountEntry],
) -> Result<Option<ContainerStorage>, (PathBuf, HostError)> {
    let mounted = MountedNames::in_dir(mounts, path);
    recognise(|name| {
        name_present_in(host, dir, &mounted, OsStr::new(name)).map_err(|e| (path.join(name), e))
    })
}

/// Whether a root's resolution asks the directories above it
/// ([`storage_above`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Above {
    /// Every resolution that judges a candidate (its approach, every
    /// recheck) and the first of a discovery round.
    Ask,
    /// A later step of a resumable discovery round, whose first step asked
    /// (the deletion-time checks ask again), or a path that is no root.
    Skip,
}

/// The refusal of `root` when a directory above it is a storage root: the
/// root lies inside container storage (`contains_special`, the storage root
/// as its path). Above means on its path, from `/` down to its parent, and
/// in its filesystem (TASK-267): a bind mount of a directory inside
/// storage has no storage root on its path, so every directory above the
/// root's place that the table shows somewhere ([`places::places_above`])
/// is asked too, at the path that shows it. Each directory is reached as a
/// root is (no symlink, no mount crossed but at a mount point, where its
/// mount is resolved anew) and asked by [`storage_in`]. A directory that
/// cannot be reached or asked, or a root the table cannot place, is the
/// error: the caller refuses the root as it refuses one it cannot reach
/// (fail closed). A directory above the root's place that no mount shows
/// (the top of a btrfs volume above a subvolume mounted alone, a host
/// directory above a container's bind volume) is not asked: a stated limit
/// (docs/safety.md). It costs one open and a handful of `statx` calls per
/// level, and per directory shown only elsewhere; it holds nothing open
/// when it returns.
pub fn storage_above<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    root: &Path,
) -> Result<Option<Skip>, ResolveError> {
    if let Some(found) = storage_on_the_path(host, mounts, root)? {
        return Ok(Some(found));
    }
    let unplaceable = |why: String| ResolveError::UnsafePath {
        path: root.to_owned(),
        why: Unsafe::Unplaceable(why),
    };
    let on_the_path = root
        .ancestors()
        .skip(1)
        .map(|at| places::place_of(mounts, at))
        .collect::<Result<HashSet<_>, _>>()
        .map_err(unplaceable)?;
    for (place, at) in places::places_above(mounts, root).map_err(unplaceable)? {
        if on_the_path.contains(&place) {
            continue;
        }
        let dir = resolve_in(host, mounts, &at, Want::Directory)?;
        match storage_in(host, &dir.fd, &at, mounts) {
            Ok(None) => {}
            Ok(Some(storage)) => return Ok(Some(storage.inside(&at, root))),
            Err((_, e)) => return Err(ResolveError::Host(e)),
        }
    }
    Ok(None)
}

/// [`storage_above`] on the root's path: each directory from `/` down to
/// its parent, one open below the last where they share a mount.
fn storage_on_the_path<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    root: &Path,
) -> Result<Option<Skip>, ResolveError> {
    let mut above: Vec<&Path> = root.ancestors().skip(1).collect();
    above.reverse();
    let mut held: Option<Resolved<H::Fd>> = None;
    for at in above {
        let same_mount = |dir: &Resolved<H::Fd>| {
            mount_for(mounts, at).is_ok_and(|mount| mount.id == dir.mount.id)
        };
        let dir = match (held.take(), at.file_name()) {
            (Some(parent), Some(name)) if same_mount(&parent) => {
                let (fd, stat) = open_beneath(
                    host,
                    &parent.fd,
                    parent.mount.id,
                    Path::new(name),
                    Access::Path,
                    Want::Directory,
                )
                .map_err(|e| e.at(at))?;
                Resolved {
                    fd,
                    mount: parent.mount,
                    stat,
                }
            }
            // `/`, or a mount point: resolved from its mount.
            _ => resolve_in(host, mounts, at, Want::Directory)?,
        };
        match storage_in(host, &dir.fd, at, mounts) {
            Ok(None) => {}
            Ok(Some(storage)) => return Ok(Some(storage.inside(at, root))),
            Err((_, e)) => return Err(ResolveError::Host(e)),
        }
        held = Some(dir);
    }
    Ok(None)
}

fn is_marker(name: &OsStr) -> bool {
    RULES
        .iter()
        .flat_map(|rule| rule.groups.iter())
        .any(|group| group.iter().any(|marker| name == *marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_rootless_podman_graphroot_as_measured_is_recognised() {
        // `ls ~/.local/share/containers/storage` on the development
        // machine, Podman 5.8 (TASK-242).
        let graphroot = names(&[
            "artifacts",
            "db.sql",
            "defaultNetworkBackend",
            "libpod",
            "networks",
            "overlay",
            "overlay-containers",
            "overlay-images",
            "overlay-layers",
            "secrets",
            "storage.lock",
            "userns.lock",
            "volumes",
        ]);
        let found = in_listing(&graphroot).expect("recognised");
        assert_eq!(found.markers, ["storage.lock", "overlay-layers"]);
        // The rootful one on the same machine, unsorted.
        let rootful = names(&[
            "volumes",
            "storage.lock",
            "db.sql",
            "libpod",
            "overlay",
            "overlay-containers",
            "overlay-images",
            "overlay-layers",
            "tmp",
            "userns.lock",
        ]);
        assert!(in_listing(&rootful).is_some());
    }

    #[test]
    fn a_separate_podman_image_store_without_storage_lock_is_recognised() {
        // `imagestore=` in containers-storage.conf: the layers and images
        // live there, `storage.lock` stays in the graphroot.
        let store = names(&["overlay", "overlay-images", "overlay-layers"]);
        assert_eq!(
            in_listing(&store).expect("image store").markers,
            ["overlay-layers", "overlay-images"]
        );
    }

    #[test]
    fn docker_and_containerd_roots_are_recognised() {
        let classic = names(&["containers", "image", "network", "overlay2", "volumes"]);
        assert_eq!(
            in_listing(&classic).expect("classic").markers,
            ["image", "overlay2", "containers"]
        );
        let vfs = names(&["containers", "image", "vfs"]);
        assert!(in_listing(&vfs).is_some());
        // Docker 20's legacy `overlay` graph driver.
        let legacy = names(&["containers", "image", "overlay", "volumes"]);
        assert_eq!(
            in_listing(&legacy).expect("legacy overlay").markers,
            ["image", "overlay", "containers"]
        );
        let snapshotter = names(&[
            "buildkit",
            "containerd",
            "containers",
            "engine-id",
            "volumes",
        ]);
        assert_eq!(
            in_listing(&snapshotter).expect("engine-id").markers,
            ["engine-id", "containers"]
        );
        let containerd = names(&["io.containerd.content.v1.content", "tmpmounts"]);
        assert_eq!(
            in_listing(&containerd).expect("containerd").what,
            "a containerd root"
        );
    }

    #[test]
    fn one_marker_of_a_pair_is_no_storage_root() {
        for alone in [
            &["storage.lock", "src", "Cargo.toml"][..],
            &["overlay-layers"],
            &["image", "layers"],
            // U-Boot's `test/` on the development machine.
            &[
                "Makefile", "cmd", "dm", "image", "overlay", "py", "run", "ut.c",
            ],
            &["overlay-images", "overlay"],
            &["image", "docs"],
            &["engine-id"],
            &["containers", "volumes"],
            &["io.containerd.example"],
        ] {
            assert_eq!(in_listing(&names(alone)), None, "{alone:?}");
        }
    }

    #[test]
    fn a_probe_asks_a_later_group_only_after_an_earlier_one_held() {
        let mut asked = Vec::new();
        let found: Result<_, ()> = recognise(|name| {
            asked.push(name.to_owned());
            Ok(false)
        });
        assert_eq!(found, Ok(None));
        assert_eq!(
            asked,
            [
                "storage.lock",
                "overlay-layers",
                "vfs-layers",
                "btrfs-layers",
                "zfs-layers",
                "aufs-layers",
                "devicemapper-layers",
                "image",
                "engine-id",
                "io.containerd.metadata.v1.bolt",
                "io.containerd.content.v1.content"
            ]
        );
    }

    #[test]
    fn a_probe_error_stops_the_recogniser() {
        let found: Result<Option<ContainerStorage>, &str> = recognise(|_| Err("EIO"));
        assert_eq!(found, Err("EIO"));
    }

    #[test]
    fn the_detail_names_the_markers() {
        let found = in_listing(&names(&["storage.lock", "vfs-layers"])).expect("recognised");
        assert_eq!(
            found.to_string(),
            "a containers/storage graphroot (Podman, Buildah, CRI-O) marked by `storage.lock` \
             and `vfs-layers`"
        );
    }
}
