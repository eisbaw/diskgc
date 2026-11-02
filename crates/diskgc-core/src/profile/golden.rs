//! Immutable matching sets for the shipped filesystem sources. Each fixture
//! entry has a path and an entry type; `None` is a symlink. This is a small
//! tree with overlapping source matches, including candidates inside other
//! candidates. A changed pattern or type changes a set and fails this test.

use std::path::Path;
use std::time::Duration;

use super::{
    Action, Body, Cwd, EntryType, Finds, IdleFor, Library, LivenessClass, LivenessContract,
    Release, Roots, Source, SourceAnchor,
};
use crate::engine::Checkouts;
use crate::fs_kind::{Find, path_matches};
use std::collections::BTreeSet;

const TEMP_PROFILE_NAMES: &[&str] = &[
    "node-compile-cache",
    "jest-cache",
    "nix-shell-rc",
    "pytest-tmp",
];

#[test]
fn anchored_cmake_requires_a_source_and_matches_any_name_through_five_levels() {
    let library = Library::builtin().expect("built-ins");
    let Body::Source(source) = &library
        .get("cmake-build-anchored")
        .expect("shipped anchored CMake profile")
        .body
    else {
        panic!("cmake-build-anchored is a source")
    };
    assert_eq!(source.roots, Roots::Required);
    assert_eq!(source.where_.has.as_deref(), Some("CMakeCache.txt"));
    assert_eq!(source.where_.has_dir.as_deref(), Some("CMakeFiles"));
    assert_eq!(source.where_.lacks_entry.as_deref(), Some("CMakeLists.txt"));
    assert_eq!(source.where_.source_anchor, Some(SourceAnchor::Cmake));
    assert_eq!(
        source.min_idle_for,
        Some(super::IdleFor(Duration::from_secs(3 * 24 * 3600)))
    );
    assert_eq!(source.checkouts, Checkouts::Refuse);
    assert_eq!(
        source.action,
        Action::Delete {
            make_writable: false
        }
    );
    let find = Find::of(source, 1000).expect("filesystem source");
    let root = Path::new("/home/mark/topics");
    for (path, expected) in [
        ("/home/mark/topics/app/generated", true),
        ("/home/mark/topics/a/b/c/d/e", true),
        ("/home/mark/topics/a/b/c/d/e/f", false),
        ("/home/mark/other/generated", false),
    ] {
        assert_eq!(
            path_matches(Some(&[root]), &find.patterns, Path::new(path)),
            expected,
            "{path}"
        );
    }
}

#[test]
fn anchored_meson_requires_generated_metadata_and_matches_any_name_through_five_levels() {
    let library = Library::builtin().expect("built-ins");
    let Body::Source(source) = &library
        .get("meson-build-anchored")
        .expect("shipped anchored Meson profile")
        .body
    else {
        panic!("meson-build-anchored is a source")
    };
    assert_eq!(source.roots, Roots::Required);
    assert_eq!(source.where_.has.as_deref(), Some("build.ninja"));
    assert_eq!(source.where_.has_dir.as_deref(), Some("meson-private"));
    assert_eq!(source.where_.lacks_entry.as_deref(), Some("meson.build"));
    assert_eq!(source.where_.source_anchor, Some(SourceAnchor::Meson));
    assert_eq!(
        source.min_idle_for,
        Some(super::IdleFor(Duration::from_secs(3 * 24 * 3600)))
    );
    assert_eq!(source.checkouts, Checkouts::Refuse);
    assert_eq!(
        source.action,
        Action::Delete {
            make_writable: false
        }
    );
    let find = Find::of(source, 1000).expect("filesystem source");
    let root = Path::new("/home/mark/topics");
    for (path, expected) in [
        ("/home/mark/topics/app/generated", true),
        ("/home/mark/topics/a/b/c/d/e", true),
        ("/home/mark/topics/a/b/c/d/e/f", false),
        ("/home/mark/other/generated", false),
    ] {
        assert_eq!(
            path_matches(Some(&[root]), &find.patterns, Path::new(path)),
            expected,
            "{path}"
        );
    }
}

#[test]
fn anchored_autotools_requires_generated_metadata_and_matches_any_name_through_five_levels() {
    let library = Library::builtin().expect("built-ins");
    let Body::Source(source) = &library
        .get("autotools-build-anchored")
        .expect("shipped anchored Autotools profile")
        .body
    else {
        panic!("autotools-build-anchored is a source")
    };
    assert_eq!(source.roots, Roots::Required);
    assert_eq!(source.where_.has.as_deref(), Some("config.status"));
    assert_eq!(source.where_.lacks_entry.as_deref(), Some("configure.ac"));
    assert_eq!(source.where_.source_anchor, Some(SourceAnchor::Autotools));
    assert_eq!(
        source.min_idle_for,
        Some(super::IdleFor(Duration::from_secs(3 * 24 * 3600)))
    );
    assert_eq!(source.checkouts, Checkouts::Refuse);
    assert_eq!(
        source.action,
        Action::Delete {
            make_writable: false
        }
    );
    let find = Find::of(source, 1000).expect("filesystem source");
    let root = Path::new("/home/mark/topics");
    for (path, expected) in [
        ("/home/mark/topics/app/generated", true),
        ("/home/mark/topics/a/b/c/d/e", true),
        ("/home/mark/topics/a/b/c/d/e/f", false),
    ] {
        assert_eq!(
            path_matches(Some(&[root]), &find.patterns, Path::new(path)),
            expected,
            "{path}"
        );
    }
}

#[test]
fn anchored_west_build_has_a_frozen_five_level_match_set_and_source_guard() {
    let library = Library::builtin().expect("built-ins");
    let Body::Source(source) = &library
        .get("west-build-anchored")
        .expect("west-build-anchored")
        .body
    else {
        panic!("west-build-anchored is a source")
    };
    assert_eq!(source.roots, Roots::Required);
    assert_eq!(
        source.finds,
        Finds::Paths {
            patterns: [
                "build",
                "*/build",
                "*/*/build",
                "*/*/*/build",
                "*/*/*/*/build"
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            exclude: Vec::new(),
            entry_type: EntryType::Dir,
        }
    );
    assert_eq!(source.where_.has.as_deref(), Some("CMakeCache.txt"));
    assert_eq!(source.where_.has_dir.as_deref(), Some("zephyr"));
    assert_eq!(source.where_.lacks_entry.as_deref(), Some("CMakeLists.txt"));
    assert_eq!(source.where_.source_anchor, Some(SourceAnchor::Cmake));
    assert_eq!(
        source.min_idle_for,
        Some(super::IdleFor(Duration::from_secs(3 * 24 * 3600)))
    );
    assert_eq!(source.checkouts, Checkouts::Refuse);
    assert_eq!(
        source.action,
        Action::Delete {
            make_writable: false
        }
    );
    let find = Find::of(source, 1000).expect("filesystem source");
    let root = Path::new("/home/mark/src");
    for (path, expected) in [
        ("/home/mark/src/build", true),
        ("/home/mark/src/a/b/c/d/build", true),
        ("/home/mark/src/a/b/c/d/e/build", false),
        ("/home/mark/src/a/b/c/d/not-build", false),
    ] {
        assert_eq!(
            path_matches(Some(&[root]), &find.patterns, Path::new(path)),
            expected,
            "{path}"
        );
    }
}

#[test]
fn platformio_cache_uses_the_documented_default_path_and_own_cleaner() {
    let library = Library::builtin().expect("built-ins");
    let Body::Source(source) = &library
        .get("platformio-cache")
        .expect("platformio-cache")
        .body
    else {
        panic!("platformio-cache is a source")
    };
    assert_eq!(source.roots, Roots::Required);
    assert_eq!(
        source.finds,
        Finds::Paths {
            patterns: vec![".platformio/.cache".to_owned()],
            exclude: Vec::new(),
            entry_type: EntryType::Dir,
        }
    );
    assert_eq!(source.checkouts, Checkouts::Refuse);
    assert_eq!(
        source.action,
        Action::Command {
            argv: [
                "env",
                "PLATFORMIO_CACHE_DIR=.cache",
                "pio",
                "system",
                "prune",
                "--cache",
                "--force",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            cwd: Some(Cwd::Parent),
        }
    );
    let find = Find::of(source, 1000).expect("filesystem source");
    let root = Path::new("/home/mark");
    for (path, expected) in [
        ("/home/mark/.platformio/.cache", true),
        ("/home/mark/.platformio/cache", false),
        ("/home/mark/.platformio/packages", false),
        ("/home/mark/project/.pio/build", false),
    ] {
        assert_eq!(
            path_matches(Some(&[root]), &find.patterns, Path::new(path)),
            expected,
            "{path}"
        );
    }
}

#[test]
fn project_python_caches_have_frozen_depth_and_marker_rules() {
    let library = Library::builtin().expect("built-ins");
    let root = Path::new("/home/mark/src");
    for (name, suffix, marker) in [
        ("python-pycache", "__pycache__", None),
        ("mypy-cache", ".mypy_cache", Some("CACHEDIR.TAG")),
    ] {
        let Body::Source(source) = &library.get(name).expect("project cache").body else {
            panic!("{name} is a source")
        };
        assert_eq!(source.roots, Roots::Required);
        assert_eq!(source.where_.has.as_deref(), marker);
        assert_eq!(source.checkouts, Checkouts::Refuse);
        assert_eq!(
            source.action,
            Action::Delete {
                make_writable: false
            }
        );
        let find = Find::of(source, 1000).expect("filesystem source");
        for depth in 1..=6 {
            let mut relative = vec!["part"; depth - 1];
            relative.push(suffix);
            let path = root.join(relative.join("/"));
            assert_eq!(
                path_matches(Some(&[root]), &find.patterns, &path),
                depth <= 5,
                "{name} depth {depth} at {}",
                path.display()
            );
        }
        let unrelated = root.join(format!("app/{suffix}-backup"));
        assert!(!path_matches(Some(&[root]), &find.patterns, &unrelated));
        let outside = Path::new("/home/mark/other").join(suffix);
        assert!(!path_matches(Some(&[root]), &find.patterns, &outside));
    }
}

/// TASK-79: the project dependency built-ins' frozen definitions: depth
/// one through five below a required root, exactly their directory name
/// (no `.venv-*`, no `node_modules.old`), their contract, the 30-day
/// floor, checkouts refused, plain delete, no other predicate, and no
/// release catalog (Mark's TASK-276/138 call).
#[test]
fn project_dependency_profiles_have_frozen_depth_contract_and_floor() {
    let library = Library::builtin().expect("built-ins");
    let root = Path::new("/home/mark/src");
    for (name, suffix, contract) in [
        (
            "node-modules",
            "node_modules",
            LivenessContract::NodeProject,
        ),
        ("python-venv", ".venv", LivenessContract::PythonProject),
    ] {
        let profile = library.get(name).expect("project dependency built-in");
        assert!(
            profile.releases.is_empty(),
            "{name} is in no release catalog"
        );
        let Body::Source(source) = &profile.body else {
            panic!("{name} is a source")
        };
        assert_eq!(source.roots, Roots::Required);
        let mut predicates = source.where_.clone();
        assert_eq!(
            predicates.liveness_contract.take(),
            Some(contract),
            "{name}"
        );
        assert!(predicates.is_empty(), "{name} has no other predicate");
        assert_eq!(
            source.min_idle_for,
            Some(IdleFor(Duration::from_secs(30 * 86_400))),
            "{name} floor"
        );
        assert_eq!(source.checkouts, Checkouts::Refuse, "{name}");
        assert_eq!(
            source.action,
            Action::Delete {
                make_writable: false
            }
        );
        assert_eq!(
            source.liveness_class,
            Some(LivenessClass::VerifiedHeuristic)
        );
        let find = Find::of(source, 1000).expect("filesystem source");
        assert_eq!(find.entry_type, EntryType::Dir, "{name}");
        for depth in 1..=6 {
            let mut relative = vec!["part"; depth - 1];
            relative.push(suffix);
            let path = root.join(relative.join("/"));
            assert_eq!(
                path_matches(Some(&[root]), &find.patterns, &path),
                depth <= 5,
                "{name} depth {depth} at {}",
                path.display()
            );
        }
        for other in [
            format!("app/{suffix}.old"),
            format!("app/{suffix}-3a9f"),
            format!("app/x{suffix}"),
            format!("app/{suffix}/pkg"),
        ] {
            assert!(
                !path_matches(Some(&[root]), &find.patterns, &root.join(&other)),
                "{name} must not match {other}"
            );
        }
        // Nothing below a hidden directory under the root (tool-managed
        // installs), at any position; a hidden name is not hidden by
        // itself, nor is the root's own name.
        let found = |rel: &str| {
            let path = root.join(rel);
            path_matches(Some(&[root]), &find.patterns, &path)
                && !path_matches(Some(&[root]), &find.exclude, &path)
        };
        for hidden in [
            format!(".npm/{suffix}"),
            format!(".npm/_npx/{suffix}"),
            format!("a/.cache/{suffix}"),
            format!("a/b/.c/{suffix}"),
            format!(".a/b/c/d/{suffix}"),
            format!("a/b/c/.d/{suffix}"),
        ] {
            assert!(!found(&hidden), "{name} must exclude {hidden}");
        }
        for kept in [
            suffix.to_owned(),
            format!("a/b/c/d/{suffix}"),
            format!("a.b/{suffix}"),
        ] {
            assert!(found(&kept), "{name} must find {kept}");
        }
        let hidden_root = Path::new("/home/.mark/src");
        let below = hidden_root.join(suffix);
        assert!(
            path_matches(Some(&[hidden_root]), &find.patterns, &below)
                && !path_matches(Some(&[hidden_root]), &find.exclude, &below),
            "{name}: a hidden directory above the root does not exclude"
        );
    }
}

#[test]
fn jest_cache_has_a_frozen_temp_match_set_and_exact_cleaner_path() {
    let library = Library::builtin().expect("built-ins");
    let Body::Source(source) = &library.get("jest-cache").expect("jest-cache").body else {
        panic!("jest-cache is a source")
    };
    assert_eq!(
        source.roots,
        Roots::Fixed(vec!["/tmp".into(), "/var/tmp".into()])
    );
    assert_eq!(source.checkouts, Checkouts::Refuse);
    assert_eq!(
        source.finds,
        Finds::Paths {
            patterns: super::temp_roots::TEMP_PREFIXES_V1
                .iter()
                .map(|prefix| {
                    if prefix.is_empty() {
                        "jest_*".to_owned()
                    } else {
                        format!("{prefix}/jest_*")
                    }
                })
                .collect(),
            exclude: Vec::new(),
            entry_type: EntryType::Dir,
        }
    );
    assert_eq!(
        source.action,
        Action::Command {
            argv: [
                "jest",
                "--config",
                "{}",
                "--cacheDirectory",
                "{path}",
                "--clearCache"
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            cwd: Some(Cwd::Parent),
        }
    );
    let find = Find::of(source, 1000).expect("filesystem source");
    let roots = [Path::new("/tmp"), Path::new("/var/tmp")];
    for (path, expected) in [
        ("/tmp/jest_rs", true),
        ("/tmp/nix-shell.Ab1/jest_app_hash", true),
        ("/var/tmp/jest_app_hash", true),
        ("/tmp/subdir/jest_app_hash", false),
        ("/tmp/jest", false),
        ("/home/mark/jest_app_hash", false),
    ] {
        assert_eq!(
            path_matches(Some(&roots), &find.patterns, Path::new(path)),
            expected,
            "{path}"
        );
    }
}

const TREE: &[(&str, Option<EntryType>)] = &[
    ("/tmp/pytest-of-mark", Some(EntryType::Dir)),
    ("/tmp/pytest-of-mark/pytest-3", Some(EntryType::Dir)),
    ("/tmp/pytest-of-mark/pytest-3/test_x0", Some(EntryType::Dir)),
    (
        "/tmp/pytest-of-mark/pytest-3/popen-gw0",
        Some(EntryType::Dir),
    ),
    (
        "/tmp/pytest-of-mark/pytest-3/popen-gw0/test_parallel0",
        Some(EntryType::Dir),
    ),
    (
        "/tmp/pytest-of-mark/pytest-3/test_file0",
        Some(EntryType::File),
    ),
    ("/tmp/pytest-of-mark/pytest-3/test_current", None),
    ("/tmp/pytest-of-mark/pytest-5", Some(EntryType::File)),
    ("/tmp/pytest-of-mark/pytest-current", None),
    ("/tmp/pytest-of-mark/garbage-123", Some(EntryType::Dir)),
    ("/tmp/nix-shell.AbC123", Some(EntryType::Dir)),
    (
        "/tmp/nix-shell.AbC123/pytest-of-ci/pytest-4",
        Some(EntryType::Dir),
    ),
    (
        "/tmp/nix-shell.AbC123/pytest-of-ci/pytest-4/test_y0",
        Some(EntryType::Dir),
    ),
    ("/tmp/nix-shell.AbC123/nix-shell.B2", Some(EntryType::Dir)),
    ("/tmp/nix-shell.AbC123/tmp.N3", Some(EntryType::Dir)),
    (
        "/tmp/nix-shell.AbC123/node-compile-cache",
        Some(EntryType::Dir),
    ),
    (
        "/tmp/nix-shell.AbC123/playwright_chromiumdev_profile-X",
        Some(EntryType::Dir),
    ),
    ("/tmp/nix-shell.AbC123/renode-123", Some(EntryType::Dir)),
    ("/tmp/nix-shell.AbC123/jest_app_hash", Some(EntryType::Dir)),
    (
        "/tmp/nix-shell.AbC123/nix-shell.B2/pytest-of-ci/pytest-5",
        Some(EntryType::Dir),
    ),
    (
        "/tmp/nix-shell.AbC123/nix-shell.B2/pytest-of-ci/pytest-5/test_n0",
        Some(EntryType::Dir),
    ),
    ("/tmp/claude-1000", Some(EntryType::Dir)),
    ("/tmp/claude-1000/-home-mark-src-app", Some(EntryType::Dir)),
    (
        "/tmp/claude-1000/-home-mark-src-app/session-1",
        Some(EntryType::Dir),
    ),
    ("/tmp/claude-1000/nix-shell.Ab1", Some(EntryType::Dir)),
    ("/tmp/claude-1000/nix-shell.File", Some(EntryType::File)),
    ("/tmp/claude-1000/tmp.File", Some(EntryType::File)),
    (
        "/tmp/claude-1000/-home-mark-src-app/session-file",
        Some(EntryType::File),
    ),
    ("/tmp/claude-1000/node-compile-cache", Some(EntryType::File)),
    ("/tmp/claude-1000/playwright-log.txt", Some(EntryType::File)),
    (
        "/tmp/claude-1000/nix-shell.Ab1/node-compile-cache",
        Some(EntryType::Dir),
    ),
    (
        "/tmp/claude-1000/nix-shell.Ab1/playwright-artifacts",
        Some(EntryType::Dir),
    ),
    ("/tmp/claude-1000/tmp.X4", Some(EntryType::Dir)),
    (
        "/tmp/claude-1000/nix-shell.Ab1/pytest-of-ci/pytest-6",
        Some(EntryType::Dir),
    ),
    (
        "/tmp/claude-1000/nix-shell.Ab1/pytest-of-ci/pytest-6/test_c0",
        Some(EntryType::Dir),
    ),
    ("/tmp/nix-shell-1-2", Some(EntryType::Dir)),
    (
        "/tmp/nix-shell-1-2/node-compile-cache",
        Some(EntryType::Dir),
    ),
    (
        "/tmp/nix-shell-1-2/playwright-artifacts",
        Some(EntryType::Dir),
    ),
    ("/tmp/nix-shell-1-2/renode-1", Some(EntryType::Dir)),
    (
        "/tmp/nix-shell-1-2/pytest-of-ci/pytest-1/test_s0",
        Some(EntryType::Dir),
    ),
    ("/tmp/nix-develop-1-2", Some(EntryType::Dir)),
    // nix-shell-rc: a file of the rc shape (the type rule), a name the
    // glob matches but the contract filters, and rc shapes below a shell
    // temp directory (top level only).
    ("/tmp/nix-shell-3-4", Some(EntryType::File)),
    ("/tmp/nix-shell-abc-1", Some(EntryType::Dir)),
    ("/tmp/nix-shell-1144586", Some(EntryType::Dir)),
    ("/tmp/nix-shell.AbC123/nix-shell-5-6", Some(EntryType::Dir)),
    ("/tmp/nix-develop-1-2/nix-develop-7-8", Some(EntryType::Dir)),
    (
        "/tmp/nix-develop-1-2/node-compile-cache",
        Some(EntryType::Dir),
    ),
    (
        "/tmp/nix-develop-1-2/playwright-artifacts",
        Some(EntryType::Dir),
    ),
    ("/tmp/nix-develop-1-2/renode-1", Some(EntryType::Dir)),
    (
        "/tmp/nix-develop-1-2/pytest-of-ci/pytest-1/test_d0",
        Some(EntryType::Dir),
    ),
    ("/tmp/tmp.AbC123", Some(EntryType::Dir)),
    ("/tmp/tmp.AbC123/tmp.Inner", Some(EntryType::Dir)),
    (
        "/tmp/tmp.AbC123/pytest-of-ci/pytest-5",
        Some(EntryType::Dir),
    ),
    (
        "/tmp/tmp.AbC123/pytest-of-ci/pytest-5/test_t0",
        Some(EntryType::Dir),
    ),
    ("/tmp/tmp00i1c8qs", Some(EntryType::Dir)),
    ("/tmp/node-compile-cache", Some(EntryType::Dir)),
    ("/tmp/node-compile-cache/v24", Some(EntryType::Dir)),
    ("/tmp/.org.chromium.Chromium.A", Some(EntryType::File)),
    ("/tmp/org.chromium.Chromium.X", Some(EntryType::Dir)),
    ("/tmp/org.mozilla.firefox.X", Some(EntryType::Dir)),
    ("/tmp/playwright-artifacts-1", Some(EntryType::Dir)),
    ("/tmp/zephyr-h5e5-run.X", Some(EntryType::Dir)),
    ("/tmp/zephyr-h5e5-run.X/renode-1", Some(EntryType::Dir)),
    ("/tmp/zephyr-scratch", Some(EntryType::Dir)),
    ("/tmp/zephyr-log.txt", Some(EntryType::File)),
    ("/tmp/renode-1045514-crash", Some(EntryType::Dir)),
    ("/tmp/jest_rs", Some(EntryType::Dir)),
    ("/tmp/jest_file", Some(EntryType::File)),
    ("/tmp/jest", Some(EntryType::Dir)),
    ("/tmp/renode-log.txt", Some(EntryType::File)),
    ("/tmp/loose-file", Some(EntryType::File)),
    ("/tmp/link", None),
    ("/var/tmp/pytest-of-ci", Some(EntryType::Dir)),
    ("/var/tmp/pytest-of-ci/pytest-12", Some(EntryType::Dir)),
    (
        "/var/tmp/pytest-of-ci/pytest-12/test_v0",
        Some(EntryType::Dir),
    ),
    ("/var/tmp/buildah3558241755", Some(EntryType::Dir)),
    ("/var/tmp/jest_project_hash", Some(EntryType::Dir)),
    (
        "/var/tmp/container_images_storage528829445",
        Some(EntryType::File),
    ),
    ("/var/tmp/other-file", Some(EntryType::File)),
    ("/var/tmp/other-dir/buildah1", Some(EntryType::Dir)),
    ("/home/mark/tmp.AbC123", Some(EntryType::Dir)),
    ("/home/mark/.cache/huggingface", Some(EntryType::Dir)),
    ("/home/mark/.cache/nix", Some(EntryType::Dir)),
    ("/home/mark/.cache/.bun", Some(EntryType::Dir)),
    ("/home/mark/.cache/BraveSoftware", Some(EntryType::Dir)),
    ("/home/mark/.cache/google-chrome", Some(EntryType::Dir)),
    ("/home/mark/.cache/chromium", Some(EntryType::Dir)),
    ("/home/mark/.cache/microsoft-edge", Some(EntryType::Dir)),
    ("/home/mark/.cache/mozilla", Some(EntryType::Dir)),
    ("/home/mark/.cache/yarn", Some(EntryType::Dir)),
    ("/home/mark/.cache/sccache", Some(EntryType::Dir)),
    ("/home/mark/.cache/nix-index", Some(EntryType::Dir)),
    ("/home/mark/.cache/icon-cache.kcache", Some(EntryType::File)),
    ("/home/mark/.cache/unknown-repo", Some(EntryType::Dir)),
    ("/home/mark/.cache/symlink", None),
];

fn matches(source: &Source) -> Vec<&'static str> {
    let find = Find::of(source, 1000).expect("fs source");
    let Roots::Fixed(roots) = &source.roots else {
        panic!("golden fs sources have fixed roots");
    };
    let roots: Vec<&Path> = roots.iter().map(Path::new).collect();
    let found: Vec<&str> = TREE
        .iter()
        .filter_map(|(path, entry)| {
            let right_type = match (find.entry_type, *entry) {
                (_, None) => false,
                (EntryType::Any, Some(_)) => true,
                (wanted, Some(actual)) => wanted == actual,
            };
            (right_type
                && path_matches(Some(&roots), &find.patterns, Path::new(path))
                && !path_matches(Some(&roots), &find.exclude, Path::new(path)))
            .then_some(*path)
        })
        .collect();
    // Task-16: a profile's outer candidate owns nested matches of that
    // same profile. The fixture lists parents before children as the walk
    // discovers them; a nested match becomes a candidate only if no
    // ancestor of this profile was already chosen.
    let mut candidates = Vec::new();
    for path in found {
        if !candidates
            .iter()
            .any(|outer: &&str| Path::new(path).starts_with(Path::new(outer)))
        {
            candidates.push(path);
        }
    }
    candidates
}

#[test]
fn every_shipped_tmp_source_has_its_exact_frozen_definition() {
    let library = Library::builtin().expect("built-ins");
    let shipped: BTreeSet<&str> = library
        .profiles()
        .filter_map(|profile| {
            let Body::Source(source) = &profile.body else {
                return None;
            };
            let (Roots::Fixed(roots), Finds::Paths { .. }) = (&source.roots, &source.finds) else {
                return None;
            };
            roots
                .iter()
                .any(|root| root == "/tmp" || root == "/var/tmp")
                .then_some(profile.name.as_str())
        })
        .collect();
    // containers-image-tmp, the one /var/tmp source, was withdrawn (Mark,
    // 2026-10-23; TASK-112): a 3-day floor cannot show that Buildah is done
    // with a spool it may reopen. Re-admission is TASK-125, under a new name.
    assert_eq!(shipped, TEMP_PROFILE_NAMES.iter().copied().collect());

    for name in TEMP_PROFILE_NAMES {
        let profile = library.get(name).expect("shipped profile");
        let Body::Source(source) = &profile.body else {
            panic!("{name} is a source")
        };
        // Only nix-shell-rc and pytest-tmp carry a liveness contract and a
        // floor (TASK-112).
        let (contract, floor) = match *name {
            "nix-shell-rc" => (
                Some(LivenessContract::NixShell),
                Some(IdleFor(Duration::from_secs(3600))),
            ),
            "pytest-tmp" => (
                Some(LivenessContract::Pytest),
                Some(IdleFor(Duration::from_secs(3600))),
            ),
            _ => (None, None),
        };
        let (roots, base, entry_type, preset, checkouts): (&[&str], &[&str], _, _, _) = match *name
        {
            "node-compile-cache" => (
                &["/tmp"],
                &["node-compile-cache"],
                EntryType::Dir,
                true,
                Checkouts::Refuse,
            ),
            "jest-cache" => (
                &["/tmp", "/var/tmp"],
                &["jest_*"],
                EntryType::Dir,
                true,
                Checkouts::Refuse,
            ),
            "nix-shell-rc" => (
                &["/tmp"],
                &["nix-shell-*-*", "nix-develop-*-*"],
                EntryType::Dir,
                false,
                Checkouts::Refuse,
            ),
            "pytest-tmp" => (
                &["/tmp"],
                &["pytest-of-*/pytest-*"],
                EntryType::Dir,
                true,
                Checkouts::Allow,
            ),
            _ => panic!("unreviewed temporary profile {name}"),
        };
        assert_eq!(
            source.roots,
            Roots::Fixed(roots.iter().map(|s| (*s).into()).collect()),
            "{name} roots"
        );
        let expected: Vec<String> = if preset {
            super::temp_roots::TEMP_PREFIXES_V1
                .iter()
                .map(|prefix| {
                    if prefix.is_empty() {
                        base[0].to_owned()
                    } else {
                        format!("{prefix}/{}", base[0])
                    }
                })
                .collect()
        } else {
            base.iter().map(|s| (*s).to_owned()).collect()
        };
        assert_eq!(
            source.finds,
            Finds::Paths {
                patterns: expected,
                exclude: Vec::new(),
                entry_type
            },
            "{name} patterns and type"
        );
        let mut predicates = source.where_.clone();
        assert_eq!(
            predicates.liveness_contract.take(),
            contract,
            "{name} liveness contract"
        );
        assert!(predicates.is_empty(), "{name} predicates");
        assert_eq!(source.min_idle_for, floor, "{name} minimum age");
        assert_eq!(source.checkouts, checkouts, "{name} checkout policy");
        let expected_action = if *name == "jest-cache" {
            Action::Command {
                argv: [
                    "jest",
                    "--config",
                    "{}",
                    "--cacheDirectory",
                    "{path}",
                    "--clearCache",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                cwd: Some(Cwd::Parent),
            }
        } else {
            Action::Delete {
                make_writable: false,
            }
        };
        assert_eq!(source.action, expected_action, "{name} action");
    }
}

#[test]
fn every_tmp_builtin_fs_source_has_an_unchanged_golden_set() {
    let library = Library::builtin().expect("built-ins");
    for name in TEMP_PROFILE_NAMES {
        let profile = library.get(name).expect("shipped /tmp profile");
        let Body::Source(source) = &profile.body else {
            panic!("{name} is a source");
        };
        assert!(matches!(source.finds, Finds::Paths { .. }), "{name} is fs");
        let want: &[&str] = match profile.name.as_str() {
            "node-compile-cache" => &[
                "/tmp/nix-shell.AbC123/node-compile-cache",
                "/tmp/claude-1000/nix-shell.Ab1/node-compile-cache",
                "/tmp/nix-shell-1-2/node-compile-cache",
                "/tmp/nix-develop-1-2/node-compile-cache",
                "/tmp/node-compile-cache",
            ],
            "jest-cache" => &[
                "/tmp/nix-shell.AbC123/jest_app_hash",
                "/tmp/jest_rs",
                "/var/tmp/jest_project_hash",
            ],
            "nix-shell-rc" => &[
                "/tmp/nix-shell-1-2",
                "/tmp/nix-develop-1-2",
                "/tmp/nix-shell-abc-1",
            ],
            "pytest-tmp" => &[
                "/tmp/pytest-of-mark/pytest-3",
                "/tmp/nix-shell.AbC123/pytest-of-ci/pytest-4",
                "/tmp/nix-shell.AbC123/nix-shell.B2/pytest-of-ci/pytest-5",
                "/tmp/claude-1000/nix-shell.Ab1/pytest-of-ci/pytest-6",
                "/tmp/tmp.AbC123/pytest-of-ci/pytest-5",
            ],
            name => panic!("built-in /tmp source `{name}` has no golden set"),
        };
        assert_eq!(
            matches(source),
            want,
            "{} matching set changed",
            profile.name
        );
    }
}

#[test]
fn changing_a_pattern_without_a_new_name_breaks_the_golden_set() {
    let library = Library::builtin().expect("built-ins");
    let Body::Source(original) = &library.get("node-compile-cache").expect("node cache").body
    else {
        panic!("node cache is a source");
    };
    let mut changed = original.clone();
    let Finds::Paths { patterns, .. } = &mut changed.finds else {
        panic!("node cache has path patterns");
    };
    patterns[0] = "node-compile-cachx".to_owned();
    assert_ne!(matches(&changed), matches(original));
}

#[test]
fn every_tmp_builtin_fs_source_has_a_distinguishing_type_mutation() {
    let library = Library::builtin().expect("built-ins");
    for name in TEMP_PROFILE_NAMES {
        let profile = library.get(name).expect("shipped /tmp profile");
        let Body::Source(original) = &profile.body else {
            panic!("{name} is a source");
        };
        assert!(
            matches!(original.finds, Finds::Paths { .. }),
            "{name} is fs"
        );
        let mut changed = original.clone();
        let Finds::Paths { entry_type, .. } = &mut changed.finds else {
            unreachable!("the original was an fs source");
        };
        *entry_type = match *entry_type {
            EntryType::Any => EntryType::Dir,
            EntryType::Dir | EntryType::File => EntryType::Any,
        };
        assert_ne!(
            matches(&changed),
            matches(original),
            "{} has no golden fixture that distinguishes its type rule",
            profile.name
        );
    }
}

/// The liveness guard of a built-in or of a "Liveness contracts" row: the
/// `liveness_contract`, whether `app_not_running` is required, the source
/// floor and the checkout setting. A row states the floor and checkouts
/// only where they matter (`None`: not stated).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Guard {
    contract: Option<String>,
    app_not_running: bool,
    min_idle_for: Option<Duration>,
    checkouts_allow: Option<bool>,
}

/// A row's verdict, by the first words of its Verdict cell
/// (docs/reference.md, Liveness contracts): the first three name a
/// built-in, every other verdict names none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Shipped,
    DecisionPending,
    ProposedWithdrawal,
    NotBuiltIn,
}

impl Verdict {
    fn of(cell: &str) -> Self {
        if cell.starts_with("Shipped") {
            Self::Shipped
        } else if cell.starts_with("Decision pending") {
            Self::DecisionPending
        } else if cell.starts_with("Proposed withdrawal") {
            Self::ProposedWithdrawal
        } else {
            Self::NotBuiltIn
        }
    }
}

/// One row of docs/reference.md "Liveness contracts", for one profile.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LivenessRow {
    guard: Guard,
    class: LivenessClass,
    residuals: String,
    verdict: Verdict,
    /// The Verdict cell records a dated admission ([`admission_recorded`]).
    decided: bool,
    /// The Verdict cell names container tests, or "Container test: pending
    /// (TASK-N)" (docs/safety.md: a verified heuristic needs them where the
    /// kernel's view matters).
    container_tested: bool,
}

/// One built-in as the library holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Builtin {
    /// A source (only sources have a liveness row).
    source: bool,
    guard: Guard,
    class: Option<LivenessClass>,
    released: bool,
}

/// The code span after `key: ` in `cell` (`` `min_idle_for: 1h` ``).
fn cell_value<'a>(cell: &'a str, key: &str) -> Option<&'a str> {
    let open = format!("`{key}: ");
    let start = cell.find(&open)? + open.len();
    let len = cell[start..].find('`')?;
    Some(&cell[start..start + len])
}

/// Who decided an admission recorded in a Verdict cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decider {
    Mark,
    /// mark-emulator, on Mark's delegation (TASK-122); Mark may veto.
    MarkEmulator,
}

/// The admission a Verdict cell records: "admitted by Mark, YYYY-MM-DD" or
/// "admitted by mark-emulator for Mark, YYYY-MM-DD", either case of "a", at
/// the start of the cell or right after ", " (so "not admitted by ..." and
/// "previously admitted by ..." do not count), with an ISO date.
fn admission_recorded(cell: &str) -> Option<(Decider, &str)> {
    let forms = [
        ("admitted by Mark, ", Decider::Mark),
        (
            "admitted by mark-emulator for Mark, ",
            Decider::MarkEmulator,
        ),
    ];
    let starts = std::iter::once(0).chain(cell.match_indices(", ").map(|(at, _)| at + 2));
    for start in starts {
        let rest = &cell[start..];
        for (form, decider) in forms {
            let Some(tail) = rest
                .strip_prefix(form)
                .or_else(|| rest.strip_prefix(&form.replacen('a', "A", 1)))
            else {
                continue;
            };
            let date = tail.get(..10).unwrap_or("");
            let iso = date.len() == 10
                && date.char_indices().all(|(i, c)| {
                    if i == 4 || i == 7 {
                        c == '-'
                    } else {
                        c.is_ascii_digit()
                    }
                });
            if iso {
                return Some((decider, date));
            }
        }
    }
    None
}

/// Whether a Verdict cell records its container tests in one of the two
/// accepted forms: "Container tests: `a`, `b`" (at least one backticked
/// test name) or "Container test: pending (TASK-<digits>)".
fn container_tests_recorded(cell: &str) -> bool {
    let named = cell
        .split("Container tests: ")
        .skip(1)
        .any(|rest| rest.starts_with('`') && rest[1..].find('`').is_some_and(|end| end > 0));
    let pending = cell
        .split("Container test: pending (TASK-")
        .skip(1)
        .any(|rest| {
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            digits > 0 && rest[digits..].starts_with(')')
        });
    named || pending
}

/// The class a Class cell starts with: a class's table name followed by
/// the end, a space, `,` or `:` (`sound (presence marker)`, `verified
/// heuristic: ...`).
fn class_of(cell: &str) -> Option<LivenessClass> {
    LivenessClass::ALL.into_iter().find(|class| {
        cell.strip_prefix(class.table_name())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', ',', ':']))
    })
}

/// The liveness table, one entry per profile named in a row's first cell.
/// A profile named by two rows, and a Class cell that names no class, are
/// problems in themselves. A Contract cell that starts with "planned"
/// describes nothing built.
fn liveness_rows(rows: &[Vec<String>]) -> (Vec<(String, LivenessRow)>, Vec<String>) {
    let mut out: Vec<(String, LivenessRow)> = Vec::new();
    let mut problems = Vec::new();
    for row in rows {
        assert_eq!(row.len(), 6, "Liveness contracts row has 6 cells: {row:?}");
        let cell = &row[2];
        let built = !cell.starts_with("planned");
        let Some(class) = class_of(&row[3]) else {
            problems.push(format!(
                "{}: Class cell names no class: {:?}",
                row[0], row[3]
            ));
            continue;
        };
        let entry = LivenessRow {
            guard: Guard {
                contract: cell_value(cell, "liveness_contract")
                    .filter(|_| built)
                    .map(str::to_owned),
                app_not_running: built && cell.contains("`app_not_running`"),
                min_idle_for: cell_value(cell, "min_idle_for")
                    .filter(|_| built)
                    .map(|text| crate::units::parse_duration(text).expect("a duration")),
                checkouts_allow: cell_value(cell, "checkouts")
                    .filter(|_| built)
                    .map(|text| text == "allow"),
            },
            class,
            residuals: row[4].trim().to_owned(),
            verdict: Verdict::of(&row[5]),
            decided: admission_recorded(&row[5]).is_some(),
            container_tested: container_tests_recorded(&row[5]),
        };
        for name in row[0].split(", ").map(crate::doc::code) {
            if out.iter().any(|(seen, _)| seen == name) {
                problems.push(format!("{name}: named by two Liveness contracts rows"));
            }
            out.push((name.to_owned(), entry.clone()));
        }
    }
    (out, problems)
}

/// The rule's verdict-and-class pairs (docs/reference.md, Liveness
/// contracts): Shipped needs an admissible class, and a verified heuristic
/// Mark's dated decision; Decision pending is only a verified heuristic;
/// class none is only a Proposed withdrawal while it is a built-in.
fn verdict_problem(name: &str, row: &LivenessRow) -> Option<String> {
    let class = row.class.table_name();
    match (row.verdict, row.class) {
        (Verdict::Shipped, LivenessClass::NoContract) => {
            Some(format!("{name}: Shipped with class none"))
        }
        (Verdict::Shipped, LivenessClass::VerifiedHeuristic) if !row.decided => Some(format!(
            "{name}: a verified heuristic Shipped without Mark's dated decision"
        )),
        (Verdict::Shipped, LivenessClass::VerifiedHeuristic) if !row.container_tested => {
            Some(format!(
                "{name}: a verified heuristic Shipped without its container tests or \
                 \"Container test: pending (TASK-N)\""
            ))
        }
        (Verdict::DecisionPending, c) if c != LivenessClass::VerifiedHeuristic => Some(format!(
            "{name}: Decision pending is for a verified heuristic, not {class}"
        )),
        (Verdict::ProposedWithdrawal, c) if c != LivenessClass::NoContract => Some(format!(
            "{name}: Proposed withdrawal is for class none, not {class}"
        )),
        (Verdict::DecisionPending | Verdict::ProposedWithdrawal, _) if row.decided => {
            Some(format!("{name}: a decided row cannot be {:?}", row.verdict))
        }
        _ => None,
    }
}

/// The sync rule between the liveness table, the built-in library and the
/// 0.1 catalog (TASK-112, TASK-122): every built-in source is
/// named by exactly one row whose verdict names a built-in (Shipped,
/// Decision pending, Proposed withdrawal), with builtin.yaml's
/// `liveness_class`, non-empty residuals, and exactly the row's guard (the
/// floor and checkout setting where the row states them; a contract row
/// must state its floor); a built-in with a liveness guard is Shipped; a
/// built-in in a `release` catalog is Shipped; no row names a filter, and
/// no verdict that names a built-in is without one.
fn liveness_sync_problems(
    rows: &[(String, LivenessRow)],
    builtins: &[(String, Builtin)],
) -> Vec<String> {
    let mut problems = Vec::new();
    let row_of = |name: &str| rows.iter().find(|(n, _)| n == name).map(|(_, r)| r);
    for (name, builtin) in builtins {
        let row = row_of(name);
        if !builtin.source {
            if row.is_some() {
                problems.push(format!("{name}: a filter has no Liveness contracts row"));
            }
            continue;
        }
        let Some(row) = row else {
            problems.push(format!("built-in {name} has no Liveness contracts row"));
            continue;
        };
        if row.verdict == Verdict::NotBuiltIn {
            problems.push(format!(
                "{name}: row's verdict names no built-in (excluded, withdrawn or planned) but \
                 it is a built-in"
            ));
        }
        if builtin.class != Some(row.class) {
            problems.push(format!(
                "{name}: row's class {:?}, builtin.yaml's liveness_class {:?}",
                row.class, builtin.class
            ));
        }
        problems.extend(verdict_problem(name, row));
        if row.residuals.is_empty() {
            problems.push(format!("{name}: row names no residuals"));
        }
        if builtin.released && row.verdict != Verdict::Shipped {
            problems.push(format!(
                "{name}: in the 0.1 catalog (release) but its row is {:?}",
                row.verdict
            ));
        }
        let guard = &builtin.guard;
        let guarded = guard.contract.is_some() || guard.app_not_running;
        if guarded && row.verdict != Verdict::Shipped {
            problems.push(format!(
                "built-in {name} has a liveness guard {guard:?}; its row is {:?}, not Shipped",
                row.verdict
            ));
        }
        let documented = &row.guard;
        if documented.contract != guard.contract
            || documented.app_not_running != guard.app_not_running
        {
            problems.push(format!(
                "{name}: row's guard {:?}/app_not_running {}, built-in's {:?}/{}",
                documented.contract,
                documented.app_not_running,
                guard.contract,
                guard.app_not_running
            ));
        }
        if documented.contract.is_some() && documented.min_idle_for.is_none() {
            problems.push(format!("{name}: contract row states no min_idle_for"));
        }
        if documented.min_idle_for.is_some() && documented.min_idle_for != guard.min_idle_for {
            problems.push(format!(
                "{name}: row's min_idle_for {:?}, built-in's {:?}",
                documented.min_idle_for, guard.min_idle_for
            ));
        }
        if documented.checkouts_allow.is_some()
            && documented.checkouts_allow != guard.checkouts_allow
        {
            problems.push(format!(
                "{name}: row's checkouts allow {:?}, built-in's {:?}",
                documented.checkouts_allow, guard.checkouts_allow
            ));
        }
    }
    for (name, row) in rows {
        let builtin = builtins.iter().any(|(n, b)| n == name && b.source);
        if !builtin && row.verdict != Verdict::NotBuiltIn {
            problems.push(format!(
                "{name}: row's verdict is {:?}, no such built-in source",
                row.verdict
            ));
        }
    }
    problems
}

fn builtins_as_loaded(library: &Library) -> Vec<(String, Builtin)> {
    library
        .profiles()
        .map(|profile| {
            let builtin = match &profile.body {
                Body::Source(source) => Builtin {
                    source: true,
                    guard: Guard {
                        contract: source.where_.liveness_contract.map(|c| c.name().to_owned()),
                        app_not_running: source.where_.app_not_running.is_some(),
                        min_idle_for: source.min_idle_for.map(|IdleFor(d)| d),
                        checkouts_allow: Some(source.checkouts == Checkouts::Allow),
                    },
                    class: source.liveness_class,
                    released: profile.releases.contains(&Release::V0_1),
                },
                Body::Filter(_) => Builtin {
                    source: false,
                    guard: Guard {
                        contract: None,
                        app_not_running: false,
                        min_idle_for: None,
                        checkouts_allow: None,
                    },
                    class: None,
                    released: profile.releases.contains(&Release::V0_1),
                },
            };
            (profile.name.clone(), builtin)
        })
        .collect()
}

fn builtin_facts() -> Vec<(String, Builtin)> {
    builtins_as_loaded(&Library::builtin().expect("built-ins"))
}

fn documented_liveness_rows() -> (Vec<(String, LivenessRow)>, Vec<String>) {
    use crate::doc::{reference_md, section, table};
    let section = section(&reference_md(), "### Liveness contracts");
    liveness_rows(&table(&section, "| Profile |"))
}

#[test]
fn liveness_table_and_builtin_library_agree() {
    let (rows, mut problems) = documented_liveness_rows();
    problems.extend(liveness_sync_problems(&rows, &builtin_facts()));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The 0.1 catalog is exactly the built-ins whose `releases:` names it
/// (derived, not listed), and every source in it is Shipped in the
/// liveness table (the sync rule above).
#[test]
fn the_release_catalog_is_the_release_data() {
    let catalog = Library::builtin()
        .expect("built-ins")
        .release_catalog("0.1", Release::V0_1)
        .expect("0.1 catalog");
    let derived: Vec<&str> = catalog.profiles().map(|p| p.name.as_str()).collect();
    let released: Vec<String> = builtin_facts()
        .into_iter()
        .filter(|(_, b)| b.released)
        .map(|(name, _)| name)
        .collect();
    assert_eq!(derived, released);
    assert!(
        catalog
            .profiles()
            .any(|p| matches!(&p.body, Body::Source(_))),
        "the catalog holds a source"
    );
}

/// A `releases:` label on a source outside the release scope (a command action, a
/// Docker kind) is refused when the catalog is built, not trusted.
#[test]
fn the_release_catalog_refuses_an_out_of_scope_source() {
    let yaml = "- name: c\n  category: c\n  description: d\n  find: {roots: required, paths: [.cache/c], type: dir}\n  action: {command: [tool, clean, \"{path}\"]}\n  examples: {match: [/home/m/.cache/c]}\n  verified_with: v\n  liveness_class: delegated\n  releases: [\"0.1\"]\n";
    let library = Library::load(yaml, &[]).unwrap_or_else(|e| panic!("{e}"));
    let error = library
        .release_catalog("0.1", Release::V0_1)
        .expect_err("a command action");
    assert!(
        error.contains("diskgc 0.1's catalog (0.1) built-in `c` is outside the release scope"),
        "{error}"
    );
    let filters_only = "- name: idle-9d\n  category: c\n  description: d\n  where: {idle_for: 9d}\n  releases: [\"0.1\"]\n";
    let library = Library::load(filters_only, &[]).unwrap_or_else(|e| panic!("{e}"));
    let error = library
        .release_catalog("0.1", Release::V0_1)
        .expect_err("no source");
    assert!(error.contains("has no source"), "{error}");
}

/// The sync rule bites each way it can drift, on edits of the real table
/// and library, each with its own message (a manual edit of either file
/// shows the same).
#[test]
fn liveness_sync_rule_catches_each_kind_of_drift() {
    let (rows, _) = documented_liveness_rows();
    let builtins = builtin_facts();
    let edit_builtin = |name: &str, edit: &dyn Fn(&mut Builtin)| {
        let mut b = builtins.clone();
        edit(&mut b.iter_mut().find(|(n, _)| n == name).expect(name).1);
        b
    };
    let edit_row = |name: &str, edit: &dyn Fn(&mut LivenessRow)| {
        let mut r = rows.clone();
        edit(&mut r.iter_mut().find(|(n, _)| n == name).expect(name).1);
        r
    };
    let without = |name: &str| -> Vec<(String, LivenessRow)> {
        rows.iter().filter(|(n, _)| n != name).cloned().collect()
    };
    let mut added = builtins.clone();
    added.push((
        "nix-shell-tmpdir".into(),
        Builtin {
            source: true,
            guard: Guard {
                contract: None,
                app_not_running: false,
                min_idle_for: None,
                checkouts_allow: Some(false),
            },
            class: Some(LivenessClass::NoContract),
            released: false,
        },
    ));
    let mut unrowed = builtins.clone();
    unrowed.push((
        "new-cache".into(),
        Builtin {
            source: true,
            guard: Guard {
                contract: None,
                app_not_running: false,
                min_idle_for: None,
                checkouts_allow: Some(false),
            },
            class: Some(LivenessClass::Tolerant),
            released: false,
        },
    ));
    let cases: Vec<(&str, Vec<String>, &str)> = vec![
        (
            "contract dropped from a built-in",
            liveness_sync_problems(
                &rows,
                &edit_builtin("pytest-tmp", &|b| b.guard.contract = None),
            ),
            "pytest-tmp: row's guard",
        ),
        (
            "contract added to a built-in",
            liveness_sync_problems(
                &rows,
                &edit_builtin("node-compile-cache", &|b| {
                    b.guard.contract = Some("pytest".into())
                }),
            ),
            "node-compile-cache: row's guard",
        ),
        (
            "a guarded built-in whose row is not Shipped",
            liveness_sync_problems(
                &edit_row("nix-shell-rc", &|r| r.verdict = Verdict::DecisionPending),
                &builtins,
            ),
            "built-in nix-shell-rc has a liveness guard",
        ),
        (
            "app_not_running dropped from a built-in",
            liveness_sync_problems(
                &rows,
                &edit_builtin("brave-browser-cache", &|b| b.guard.app_not_running = false),
            ),
            "brave-browser-cache: row's guard",
        ),
        (
            "floor changed in a built-in",
            liveness_sync_problems(
                &rows,
                &edit_builtin("nix-shell-rc", &|b| {
                    b.guard.min_idle_for = Some(Duration::from_secs(60));
                }),
            ),
            "nix-shell-rc: row's min_idle_for",
        ),
        (
            "checkouts changed in a built-in",
            liveness_sync_problems(
                &rows,
                &edit_builtin("pytest-tmp", &|b| b.guard.checkouts_allow = Some(false)),
            ),
            "pytest-tmp: row's checkouts",
        ),
        (
            "class changed in builtin.yaml",
            liveness_sync_problems(
                &rows,
                &edit_builtin("python-pycache", &|b| b.class = Some(LivenessClass::Sound)),
            ),
            "python-pycache: row's class",
        ),
        (
            "class changed in the table",
            liveness_sync_problems(
                &edit_row("node-compile-cache", &|r| {
                    r.class = LivenessClass::Delegated
                }),
                &builtins,
            ),
            "node-compile-cache: row's class",
        ),
        (
            "built-in source without a row",
            liveness_sync_problems(&rows, &unrowed),
            "built-in new-cache has no Liveness contracts row",
        ),
        (
            "row of a contract built-in removed",
            liveness_sync_problems(&without("nix-shell-rc"), &builtins),
            "built-in nix-shell-rc has no Liveness contracts row",
        ),
        (
            "row's contract changed",
            liveness_sync_problems(
                &edit_row("pytest-tmp", &|r| {
                    r.guard.contract = Some("nix-shell".into())
                }),
                &builtins,
            ),
            "pytest-tmp: row's guard",
        ),
        (
            "contract row without a floor",
            liveness_sync_problems(
                &edit_row("pytest-tmp", &|r| r.guard.min_idle_for = None),
                &builtins,
            ),
            "pytest-tmp: contract row states no min_idle_for",
        ),
        (
            "excluded profile shipped",
            liveness_sync_problems(&rows, &added),
            "nix-shell-tmpdir: row's verdict names no built-in",
        ),
        (
            "Shipped row without a built-in",
            liveness_sync_problems(
                &edit_row("mktemp-dir", &|r| r.verdict = Verdict::Shipped),
                &builtins,
            ),
            "mktemp-dir: row's verdict is Shipped, no such built-in source",
        ),
        (
            "class none Shipped",
            liveness_sync_problems(
                &edit_row("node-compile-cache", &|r| {
                    r.class = LivenessClass::NoContract
                }),
                &builtins,
            ),
            "node-compile-cache: Shipped with class none",
        ),
        (
            "verified heuristic Shipped without a decision",
            liveness_sync_problems(
                &edit_row("codex-releases", &|r| r.decided = false),
                &builtins,
            ),
            "codex-releases: a verified heuristic Shipped without Mark's dated decision",
        ),
        (
            "verified heuristic Shipped without container tests",
            liveness_sync_problems(
                &edit_row("bun-cache", &|r| r.container_tested = false),
                &builtins,
            ),
            "bun-cache: a verified heuristic Shipped without its container tests",
        ),
        (
            "Proposed withdrawal of an admissible class",
            liveness_sync_problems(
                &edit_row("bun-cache", &|r| r.verdict = Verdict::ProposedWithdrawal),
                &builtins,
            ),
            "bun-cache: Proposed withdrawal is for class none",
        ),
        (
            "Decision pending of a class that needs none",
            liveness_sync_problems(
                &edit_row("pytest-cache", &|r| r.verdict = Verdict::DecisionPending),
                &builtins,
            ),
            "pytest-cache: Decision pending is for a verified heuristic",
        ),
        (
            "row without residuals",
            liveness_sync_problems(
                &edit_row("go-mod-cache", &|r| r.residuals.clear()),
                &builtins,
            ),
            "go-mod-cache: row names no residuals",
        ),
        (
            "a release for a row that is not Shipped",
            liveness_sync_problems(
                &edit_row("bun-cache", &|r| r.verdict = Verdict::DecisionPending),
                &edit_builtin("bun-cache", &|b| b.released = true),
            ),
            "bun-cache: in the 0.1 catalog (release) but its row is DecisionPending",
        ),
        (
            "a filter with a row",
            liveness_sync_problems(&rows, &edit_builtin("pytest-cache", &|b| b.source = false)),
            "pytest-cache: a filter has no Liveness contracts row",
        ),
    ];
    for (what, problems, expected) in cases {
        assert!(
            problems.iter().any(|p| p.contains(expected)),
            "{what}: expected a problem containing {expected:?}, got {problems:?}"
        );
    }
    let doubled: Vec<Vec<String>> = vec![
        vec![
            "`a`".into(),
            "".into(),
            "none".into(),
            "none".into(),
            "Not a built-in.".into(),
            "Excluded.".into(),
        ],
        vec![
            "`b`, `a`".into(),
            "".into(),
            "none".into(),
            "none".into(),
            "Not a built-in.".into(),
            "Excluded.".into(),
        ],
        vec![
            "`c`".into(),
            "".into(),
            "none".into(),
            "mostly sound".into(),
            "Not a built-in.".into(),
            "Excluded.".into(),
        ],
    ];
    assert_eq!(
        liveness_rows(&doubled).1,
        [
            "a: named by two Liveness contracts rows",
            "`c`: Class cell names no class: \"mostly sound\""
        ]
    );
    assert_eq!(class_of("soundly"), None);
    for (cell, recorded) in [
        ("Shipped. Container test: pending (TASK-137).", true),
        (
            "Shipped. Container tests: `pytest_lifecycle_is_pinned`.",
            true,
        ),
        ("Shipped. Container test: none.", false),
        ("Shipped; no Container test yet.", false),
        ("Shipped. Container test: pending (TASK-).", false),
        ("Shipped. Container test: pending (later).", false),
        ("Shipped. Container tests: ``.", false),
        ("Shipped. Container tests: none yet.", false),
    ] {
        assert_eq!(container_tests_recorded(cell), recorded, "{cell}");
    }
    for (cell, recorded) in [
        (
            "Shipped (TASK-112 step 5), admitted by Mark, 2026-10-23.",
            Some((Decider::Mark, "2026-10-23")),
        ),
        (
            "Admitted by Mark, 2026-10-23.",
            Some((Decider::Mark, "2026-10-23")),
        ),
        (
            "Shipped, admitted by mark-emulator for Mark, 2026-10-24 (TASK-122).",
            Some((Decider::MarkEmulator, "2026-10-24")),
        ),
        ("Shipped; needs Mark's decision.", None),
        ("Shipped, admitted by someone for Mark, 2026-10-24.", None),
        ("Shipped (Mark, 2026-10-23).", None),
        ("Withdrawn; not admitted by Mark, 2026-10-23.", None),
        ("Withdrawn, previously admitted by Mark, 2026-10-23.", None),
        ("Shipped, admitted by Mark, 20xx-10-23.", None),
        ("Shipped, admitted by Mark, 2026.", None),
        ("Shipped \u{2013} \u{ff}admitted by Mark, 2026-10-23.", None),
    ] {
        assert_eq!(admission_recorded(cell), recorded, "{cell}");
    }
    assert_eq!(
        class_of("verified heuristic: lock rows"),
        Some(LivenessClass::VerifiedHeuristic)
    );
}

/// The union veto covers only candidates a contract source found itself
/// (reference.md, Liveness contract predicate): a source without a
/// contract whose candidate *contains* an rc directory or a pytest session
/// would delete it past the contract. No built-in may do that: over the
/// golden tree, no fixed-root built-in matches a strict ancestor of a
/// contract built-in's match. (Required-root built-ins match what the
/// user binds; a user profile is the residual reference.md states.)
#[test]
fn no_builtin_candidate_contains_a_contract_builtins_candidate() {
    let library = Library::builtin().expect("built-ins");
    let fixed_fs: Vec<(&str, &Source)> = library
        .profiles()
        .filter_map(|profile| match &profile.body {
            Body::Source(source)
                if matches!(source.roots, Roots::Fixed(_))
                    && matches!(source.finds, Finds::Paths { .. }) =>
            {
                Some((profile.name.as_str(), source.as_ref()))
            }
            _ => None,
        })
        .collect();
    let guarded: Vec<(&str, &str)> = fixed_fs
        .iter()
        .filter(|(_, source)| source.where_.liveness_contract.is_some())
        .flat_map(|(name, source)| matches(source).into_iter().map(move |m| (*name, m)))
        .collect();
    assert!(
        guarded.iter().any(|(name, _)| *name == "nix-shell-rc")
            && guarded.iter().any(|(name, _)| *name == "pytest-tmp"),
        "the golden tree has candidates of both contract built-ins: {guarded:?}"
    );
    for (other, source) in &fixed_fs {
        for candidate in matches(source) {
            for (name, inner) in &guarded {
                assert!(
                    !Path::new(inner).starts_with(candidate) || inner == &candidate,
                    "{other}'s candidate {candidate} contains {name}'s candidate {inner}"
                );
            }
        }
    }
}

// ---- TASK-94: toolchain sources (pending Mark's admission)

/// The toolchain sources' frozen definitions and match sets: one level
/// below `.rustup/toolchains` or `.elan/toolchains` of the supplied root,
/// directories only, the toolchain guard, a one-hour floor, a plain delete,
/// checkouts refused and no release catalog.
#[test]
fn toolchain_sources_have_frozen_match_sets_and_guards() {
    use super::ToolchainManager;
    let library = Library::builtin().expect("built-ins");
    let root = Path::new("/home/mark");
    for (name, manager, dir, links, matched) in [
        (
            "rustup-toolchains",
            ToolchainManager::Rustup,
            ".rustup",
            &[".rustup/toolchains", ".kani/*"][..],
            [
                "toolchains/stable-x86_64-unknown-linux-gnu",
                "toolchains/nightly-2025-11-21-x86_64-unknown-linux-gnu",
                "toolchains/1.90.0-x86_64-unknown-linux-gnu",
            ],
        ),
        (
            "elan-toolchains",
            ToolchainManager::Elan,
            ".elan",
            &[".elan/toolchains"][..],
            [
                "toolchains/leanprover--lean4---v4.16.0",
                "toolchains/leanprover--lean4---v4.30.0-rc1",
                "toolchains/leanprover--lean4-nightly---nightly-2025-01-01",
            ],
        ),
    ] {
        let profile = library.get(name).expect("toolchain source");
        assert!(profile.releases.is_empty(), "{name} is in no catalog");
        let Body::Source(source) = &profile.body else {
            panic!("{name} is a source")
        };
        assert_eq!(source.roots, Roots::Required);
        assert_eq!(source.where_.protect_toolchains, Some(manager));
        assert_eq!(
            source.where_.predicates(),
            [
                crate::kind::PredicateName::ProtectLinks,
                crate::kind::PredicateName::ProtectToolchains
            ]
        );
        assert_eq!(source.where_.protect_link_dirs, links);
        assert_eq!(
            source.min_idle_for,
            Some(IdleFor(Duration::from_secs(3600)))
        );
        assert_eq!(source.checkouts, Checkouts::Refuse);
        assert_eq!(
            source.liveness_class,
            Some(LivenessClass::VerifiedHeuristic)
        );
        assert_eq!(
            source.action,
            Action::Delete {
                make_writable: false
            }
        );
        let Finds::Paths {
            patterns,
            exclude,
            entry_type,
        } = &source.finds
        else {
            panic!("{name} finds paths")
        };
        assert_eq!(patterns, &[format!("{dir}/toolchains/*")]);
        assert!(exclude.is_empty());
        assert_eq!(*entry_type, EntryType::Dir);
        let find = Find::of(source, 1000).expect("filesystem source");
        let home = root.join(dir);
        for relative in matched {
            assert!(
                path_matches(Some(&[root]), &find.patterns, &home.join(relative)),
                "{name}: {relative}"
            );
        }
        for relative in [
            "toolchains",
            "settings.toml",
            "update-hashes/stable-x86_64-unknown-linux-gnu",
            "downloads/0a1b2c",
            "tmp/0a1b2c",
            "known-projects",
            "toolchains/stable-x86_64-unknown-linux-gnu/bin",
        ] {
            assert!(
                !path_matches(Some(&[root]), &find.patterns, &home.join(relative)),
                "{name}: {relative}"
            );
        }
        let nested = root.join("src").join(dir).join(matched[0]);
        assert!(!path_matches(Some(&[root]), &find.patterns, &nested));
    }
}

// ---- TASK-40: language and package caches (pending Mark's admission)

/// The TASK-40 sources' frozen definitions and match sets: fixed patterns
/// below a supplied home root, directories only, the one completion
/// marker where the tool writes one, a one-hour floor, a plain delete,
/// checkouts refused, a verified heuristic and no release catalog. A
/// changed pattern, marker, floor or action needs a new built-in name once
/// Mark admits one.
#[test]
fn language_cache_sources_have_frozen_definitions_and_match_sets() {
    use crate::kind::PredicateName;
    let library = Library::builtin().expect("built-ins");
    let root = Path::new("/home/mark");
    let hub = ".cache/huggingface/hub";
    let crates = ".cargo/registry/src/index.crates.io-1949cf8c6b5b557f";
    let electron =
        ".cache/electron/c5bfbb83a9a00da9cf9633863c02cca5138e6c2f7c9dbd11a10a5fb33041457f";
    /// Name, patterns, completion marker, paths matched, paths not matched.
    type Case<'a> = (
        &'a str,
        &'a [&'a str],
        Option<&'a str>,
        Vec<String>,
        Vec<String>,
    );
    let cases: [Case; 4] = [
        (
            "huggingface-hub-repos",
            &[
                ".cache/huggingface/hub/models--*",
                ".cache/huggingface/hub/datasets--*",
                ".cache/huggingface/hub/spaces--*",
            ],
            None,
            vec![
                format!("{hub}/models--org--m"),
                format!("{hub}/datasets--org--set"),
                format!("{hub}/spaces--org--demo"),
            ],
            vec![
                hub.to_owned(),
                format!("{hub}/.locks"),
                format!("{hub}/.locks/models--org--m"),
                format!("{hub}/models--org--m/blobs"),
                format!("{hub}/CACHEDIR.TAG"),
                format!("{hub}/version.txt"),
                ".cache/huggingface/token".to_owned(),
                ".cache/huggingface/stored_tokens".to_owned(),
                ".cache/huggingface/xet".to_owned(),
                ".cache/huggingface/assets/models--org--m".to_owned(),
                format!("src/{hub}/models--org--m"),
            ],
        ),
        (
            "cargo-registry-src",
            &[".cargo/registry/src/*/*"],
            Some(".cargo-ok"),
            vec![
                format!("{crates}/serde-1.0.228"),
                ".cargo/registry/src/github.com-1ecc6299db9ec823/libc-0.2.150".to_owned(),
            ],
            vec![
                ".cargo/registry/src".to_owned(),
                crates.to_owned(),
                format!("{crates}/serde-1.0.228/src"),
                ".cargo/registry/cache/index.crates.io-1949cf8c6b5b557f/serde-1.0.228.crate"
                    .to_owned(),
                ".cargo/registry/index/index.crates.io-1949cf8c6b5b557f/.cache".to_owned(),
                ".cargo/git/checkouts/repo-0123456789abcdef/abc1234".to_owned(),
                ".cargo/bin".to_owned(),
                format!("src/{crates}/serde-1.0.228"),
            ],
        ),
        (
            "node-gyp-headers",
            &[".cache/node-gyp/*"],
            Some("installVersion"),
            vec![
                ".cache/node-gyp/20.18.1".to_owned(),
                ".cache/node-gyp/iojs-3.3.1".to_owned(),
            ],
            vec![
                ".cache/node-gyp".to_owned(),
                ".cache/node-gyp/20.18.1/include".to_owned(),
                ".node-gyp/20.18.1".to_owned(),
                ".electron-gyp/33.4.5".to_owned(),
                "src/.cache/node-gyp/20.18.1".to_owned(),
            ],
        ),
        (
            "electron-download-cache",
            &[".cache/electron/*"],
            None,
            vec![electron.to_owned()],
            vec![
                ".cache/electron".to_owned(),
                format!("{electron}/electron-v33.4.5-linux-x64.zip"),
                ".cache/electron-builder/winCodeSign".to_owned(),
                ".config/Electron".to_owned(),
                format!("src/{electron}"),
            ],
        ),
    ];
    for (name, patterns, marker, matched, unmatched) in cases {
        let profile = library.get(name).expect("TASK-40 source");
        assert!(profile.releases.is_empty(), "{name} is in no catalog");
        let Body::Source(source) = &profile.body else {
            panic!("{name} is a source")
        };
        assert_eq!(source.roots, Roots::Required, "{name}");
        assert_eq!(
            source.finds,
            Finds::Paths {
                patterns: patterns.iter().map(|p| (*p).to_owned()).collect(),
                exclude: Vec::new(),
                entry_type: EntryType::Dir,
            },
            "{name}"
        );
        assert_eq!(source.where_.has.as_deref(), marker, "{name}");
        let predicates: &[PredicateName] = if marker.is_some() {
            &[PredicateName::Has]
        } else {
            &[]
        };
        assert_eq!(source.where_.predicates(), predicates, "{name}");
        assert_eq!(
            source.min_idle_for,
            Some(IdleFor(Duration::from_secs(3600))),
            "{name}"
        );
        assert_eq!(source.checkouts, Checkouts::Refuse, "{name}");
        assert_eq!(
            source.liveness_class,
            Some(LivenessClass::VerifiedHeuristic),
            "{name}"
        );
        assert_eq!(
            source.action,
            Action::Delete {
                make_writable: false
            },
            "{name}"
        );
        let find = Find::of(source, 1000).expect("filesystem source");
        for relative in &matched {
            assert!(
                path_matches(Some(&[root]), &find.patterns, &root.join(relative)),
                "{name} should match {relative}"
            );
        }
        for relative in &unmatched {
            assert!(
                !path_matches(Some(&[root]), &find.patterns, &root.join(relative)),
                "{name} should not match {relative}"
            );
        }
    }
}
