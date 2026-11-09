//! The nested-build guard (TASK-367; Mark, 2026-11-07, option 1). A build
//! directory inside another build system's managed tree is never taken by
//! an anchor whose source is the build directory's own ancestor (CMake,
//! Meson, Autotools; West through CMake). Meta-build systems (Buildroot,
//! OpenWrt, Yocto) build each upstream package with that package's own
//! build system inside a tree they manage, and record their progress in
//! stamp files: deleting the inner build directory leaves the outer stamp
//! saying "configured" or "built", and the outer system's next build fails
//! until the package is cleaned there.
//!
//! Three signals, data below; any vetoes the output (`Anchored::Vetoed`:
//! not a candidate, and discovery does not enter it, since a build nested
//! in it is in the same tree though its own climb may not reach a stamp),
//! and an ancestor that cannot be reached or read is a visible refusal,
//! pruned the same way:
//!
//! - **Stamps** ([`OuterBuild::stamps`]) in the build directory's parent or
//!   a directory above it: at most [`STAMP_LEVELS`] directories above the
//!   build directory or above any build directory it lies in (a directory
//!   holding a guarded anchor's marker, [`BUILD_MARKERS`]), none above the
//!   outermost root of its discovery that holds its root (`stamp_root`).
//!   Each is listed, since some stamp names carry a hash. So a build nested
//!   in a vetoed one is vetoed too, whatever root, profile or order found
//!   it (TASK-367, Codex's review: the same function of the path wherever
//!   it is asked, no state shared between walks).
//! - **Build roots** ([`OuterBuild::root_marker`]): a directory anywhere
//!   above the build directory, up to `/`, that holds the marker of a
//!   build tree diskgc's own anchors recognise (`build_trees`), looked up
//!   by name.
//! - **Managed trees by name** ([`OuterBuild::tree_name`]): a directory of
//!   that name anywhere on the build directory's path (npm's
//!   `node_modules`, where cmake-js builds a native addon in the package's
//!   `build/`), read from the path alone.

use std::ffi::OsStr;
use std::path::Path;

use super::build_trees::{BAZEL_MARKER, BUILDROOT_MARKER, YOCTO_MARKER, entry};
use super::{AUTOTOOLS_STATUS, CMAKE_CACHE, MESON_CORE_DATA, unreachable_root_as};
use crate::engine::{DEFAULT_MAX_ENTRIES, Reason, Skip};
use crate::fs_kind::discover::check_deadline;
use crate::fs_kind::judge::skip_or_stop;
use crate::host::{DirListing, Host, MonoTime};
use crate::outcome::Judged;
use crate::profile::SourceAnchor;
use crate::resolve::{Want, resolve_in};

/// The anchors whose build directory lies inside its own source, as a
/// meta-build system's per-package build directory does. The build-tree
/// anchors take the outer trees themselves.
pub(super) const GUARDED: &[SourceAnchor] = &[
    SourceAnchor::Cmake,
    SourceAnchor::Meson,
    SourceAnchor::Autotools,
];

/// A build system that builds other projects inside a tree it manages.
pub(super) struct OuterBuild {
    pub(super) system: &'static str,
    /// Names it writes in a package's directory once a step is done: a
    /// literal, or a prefix ending in `*`.
    pub(super) stamps: &'static [&'static str],
    /// The entry that makes a directory one of its build roots.
    pub(super) root_marker: Option<&'static str>,
    /// The name of the directories it manages whole: a build directory
    /// below one is its package's.
    pub(super) tree_name: Option<&'static str>,
}

/// Verified names (docs/safety.md, Builds inside another build system's
/// tree): Buildroot 2026.08.1 writes `.stamp_<step>` in each package's
/// directory (package/pkg-generic.mk; seen in its real per-package
/// directories in the container, TASK-367) and `.br2-external.mk` in
/// every output, `O=` ones included; OpenWrt v25.12.6 writes `.prepared`
/// (hashed for target packages), `.configured` (`.configured_<hash>` for
/// target packages) and `.built` in `PKG_BUILD_DIR` or `HOST_BUILD_DIR`
/// (include/package.mk, include/host-build.mk; read, not run); a Yocto
/// TMPDIR holds `saved_tmpdir`, a Bazel output base `DO_NOT_BUILD_HERE`;
/// npm installs packages in `node_modules`, where cmake-js configures an
/// addon in `<package>/build` by default (its documented `--out`; read,
/// not run) and npm does not build an installed package again.
pub(super) const OUTER_BUILDS: &[OuterBuild] = &[
    OuterBuild {
        system: "Buildroot",
        stamps: &[".stamp_*"],
        root_marker: Some(BUILDROOT_MARKER),
        tree_name: None,
    },
    OuterBuild {
        system: "OpenWrt",
        stamps: &[".built", ".configured*", ".prepared*"],
        root_marker: None,
        tree_name: None,
    },
    OuterBuild {
        system: "Yocto",
        stamps: &[],
        root_marker: Some(YOCTO_MARKER),
        tree_name: None,
    },
    OuterBuild {
        system: "Bazel",
        stamps: &[],
        root_marker: Some(BAZEL_MARKER),
        tree_name: None,
    },
    OuterBuild {
        system: "npm",
        stamps: &[],
        root_marker: None,
        tree_name: Some("node_modules"),
    },
];

/// The parent and the three directories above it: Buildroot 2026.08.1's
/// deepest out-of-source package build (`tensorflow-lite`,
/// `SUBDIR = tensorflow/lite`) has its stamps three levels up; OpenWrt's
/// (`openwrt-build`, `build`) one.
pub(super) const STAMP_LEVELS: usize = 4;

/// What makes a directory a build directory of a guarded anchor, by the
/// file its anchor reads: CMake's (West's too), Meson's, Autoconf's. A
/// directory holding one, of any type, widens the stamp climb to the four
/// directories above it.
const BUILD_MARKERS: &[&str] = &[CMAKE_CACHE, MESON_CORE_DATA, AUTOTOOLS_STATUS];

/// Why `candidate`, a build directory found below `root`, lies inside
/// another build system's managed tree; `None` when no signal says so.
pub(super) fn outer_build<H: Host>(
    host: &H,
    stamp_root: &Path,
    candidate: &Path,
    deadline: Option<MonoTime>,
) -> Judged<Option<String>> {
    if let Some(found) = named_tree(candidate) {
        return Ok(Ok(Some(found)));
    }
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            return Ok(Err(Skip::new(
                Reason::UnsupportedFs,
                candidate.to_owned(),
                format!("cannot check for another build system's tree: {error}"),
            )));
        }
    };
    // Stamps count up to this level (0: the parent): the candidate's four,
    // widened past every build directory it lies in.
    let mut reach = STAMP_LEVELS - 1;
    for (level, dir_path) in candidate.ancestors().skip(1).enumerate() {
        check_deadline(host, deadline, dir_path)?;
        let dir = match resolve_in(host, &mounts, dir_path, Want::Directory) {
            Ok(dir) => dir,
            Err(error) => {
                return Ok(Err(unreachable_root_as(
                    candidate,
                    dir_path,
                    "another build system's tree",
                    error,
                )?));
            }
        };
        for outer in OUTER_BUILDS {
            let Some(marker) = outer.root_marker else {
                continue;
            };
            if judged!(entry(
                host,
                candidate,
                &dir.fd,
                dir_path,
                dir.mount.id,
                marker,
                deadline
            ))
            .is_some()
            {
                return Ok(Ok(Some(format!(
                    "{} is a {} build root ({marker})",
                    dir_path.display(),
                    outer.system
                ))));
            }
        }
        if !dir_path.starts_with(stamp_root) {
            continue;
        }
        if level + STAMP_LEVELS > reach {
            for marker in BUILD_MARKERS {
                if judged!(entry(
                    host,
                    candidate,
                    &dir.fd,
                    dir_path,
                    dir.mount.id,
                    marker,
                    deadline
                ))
                .is_some()
                {
                    reach = level + STAMP_LEVELS;
                    break;
                }
            }
        }
        if level > reach {
            continue;
        }
        let names = match host.read_dir(&dir.fd, DEFAULT_MAX_ENTRIES) {
            Ok(DirListing::Names(names)) => names,
            Ok(DirListing::TooMany) => {
                return Ok(Err(Skip::new(
                    Reason::TooManyEntries,
                    candidate.to_owned(),
                    format!(
                        "cannot check {} for another build system's stamps: more than \
                         {DEFAULT_MAX_ENTRIES} entries",
                        dir_path.display()
                    ),
                )));
            }
            Err(error) => {
                let skip = skip_or_stop(dir_path.to_owned(), "list", &error)?;
                return Ok(Err(Skip::new(
                    skip.reason,
                    candidate.to_owned(),
                    format!(
                        "cannot check {} for another build system's stamps: {}",
                        dir_path.display(),
                        skip.detail
                    ),
                )));
            }
        };
        if let Some((system, name)) = stamp_among(&names) {
            return Ok(Ok(Some(format!(
                "{} holds {system}'s stamp {}",
                dir_path.display(),
                name.display()
            ))));
        }
    }
    Ok(Ok(None))
}

/// A directory on `candidate`'s path named as an outer build system's
/// managed tree.
fn named_tree(candidate: &Path) -> Option<String> {
    candidate.ancestors().skip(1).find_map(|dir| {
        let name = dir.file_name()?;
        OUTER_BUILDS
            .iter()
            .find(|outer| outer.tree_name.is_some_and(|tree| name == tree))
            .map(|outer| format!("{} is {}'s tree", dir.display(), outer.system))
    })
}

/// The first of `names` that is an outer build system's stamp.
fn stamp_among<N: AsRef<OsStr>>(names: &[N]) -> Option<(&'static str, &Path)> {
    names.iter().find_map(|name| {
        let name = name.as_ref();
        OUTER_BUILDS
            .iter()
            .find(|outer| outer.stamps.iter().any(|stamp| is_stamp(stamp, name)))
            .map(|outer| (outer.system, Path::new(name)))
    })
}

/// `name` is `stamp`, or starts with it when `stamp` ends in `*`.
fn is_stamp(stamp: &str, name: &OsStr) -> bool {
    let name = name.as_encoded_bytes();
    match stamp.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix.as_bytes()),
        None => name == stamp.as_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_the_verified_names_and_prefixes() {
        let found = |names: &[&str]| stamp_among(names).map(|(system, _)| system);
        for (name, system) in [
            (".stamp_configured", "Buildroot"),
            (".stamp_built", "Buildroot"),
            (".stamp_dircleaned", "Buildroot"),
            (".built", "OpenWrt"),
            (".configured", "OpenWrt"),
            (".configured_68b329da9893e34099c7d8ad5cb9c940", "OpenWrt"),
            (".prepared", "OpenWrt"),
            (".prepared_d41d8cd98f00b204e9800998ecf8427e_", "OpenWrt"),
        ] {
            assert_eq!(found(&["CMakeLists.txt", name]), Some(system), "{name}");
        }
        for name in [
            "stamp_built",
            ".stamp",
            ".built.bak",
            "built",
            ".built_x",
            "x.configured",
            "configured",
            ".prepare",
        ] {
            assert_eq!(found(&[name]), None, "{name}");
        }
    }

    #[test]
    fn a_managed_tree_counts_anywhere_on_the_path_but_the_build_itself() {
        assert!(named_tree(Path::new("/home/u/src/app/node_modules/addon/build")).is_some());
        assert!(named_tree(Path::new("/home/u/node_modules/@scope/addon/sub/build")).is_some());
        assert!(named_tree(Path::new("/home/u/src/app/build")).is_none());
        assert!(named_tree(Path::new("/home/u/src/node_modules")).is_none());
        assert!(named_tree(Path::new("/home/u/src/node_modules.old/x/build")).is_none());
    }

    #[test]
    fn every_outer_build_has_a_signal() {
        for outer in OUTER_BUILDS {
            assert!(
                !outer.stamps.is_empty()
                    || outer.root_marker.is_some()
                    || outer.tree_name.is_some(),
                "{} has no signal",
                outer.system
            );
            for stamp in outer.stamps {
                let literal = stamp.strip_suffix('*').unwrap_or(stamp);
                assert!(
                    literal.len() > 1 && !literal.contains(['*', '/']),
                    "{stamp}"
                );
            }
        }
    }
}
