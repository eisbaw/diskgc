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
//! group. Only names count, whatever their type, so a check needs a
//! listing of the directory or one `statx` per name and nothing else, and
//! errs on the side of keeping (a project that happens to hold such names
//! is kept, and the skip names the markers).
//!
//! The rules (docs/safety.md has the evidence):
//!
//! - a containers/storage graphroot (Podman, Buildah, CRI-O, Skopeo):
//!   `storage.lock` and a driver's layer store, `<driver>-layers`;
//! - a Docker (Moby) data root with the classic layer store: `image` and a
//!   graph driver's directory;
//! - a Docker data root of 23.0 or later whatever its store: `engine-id`
//!   and `containers`;
//! - a containerd root (also Docker's own, `<data-root>/containerd/daemon`,
//!   with the containerd image store): its metadata or content plugin's
//!   directory.

use std::ffi::{OsStr, OsString};
use std::fmt;

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

/// Moby's graph driver directories (`<data-root>/<driver>`, beside
/// `<data-root>/image/<driver>`).
const DOCKER_GRAPH_DRIVERS: &[&str] = &[
    "overlay2",
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
        what: "a Docker data root",
        groups: &[&["image"], DOCKER_GRAPH_DRIVERS],
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
    /// The skip detail of a directory that is this storage root.
    pub fn detail(&self) -> String {
        format!(
            "container storage, {self}: its image layers and containers are never searched or \
             deleted, whoever owns them"
        )
    }

    /// The skip detail of a candidate that lies inside this storage root,
    /// at `root`.
    pub fn inside_detail(&self, root: &std::path::Path) -> String {
        format!(
            "lies inside container storage at {}, {self}: its image layers and containers are \
             never deleted",
            root.display()
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
    fn docker_and_containerd_roots_are_recognised() {
        let classic = names(&["containers", "image", "network", "overlay2", "volumes"]);
        assert_eq!(
            in_listing(&classic).expect("classic").markers,
            ["image", "overlay2"]
        );
        let vfs = names(&["image", "vfs"]);
        assert!(in_listing(&vfs).is_some());
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
            &["image", "overlay"],
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
