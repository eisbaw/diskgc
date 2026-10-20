//! Immutable matching sets for the shipped filesystem sources. Each fixture
//! entry has a path and an entry type; `None` is a symlink. This is a small
//! tree with overlapping source matches, including candidates inside other
//! candidates. A changed pattern or type changes a set and fails this test.

use std::path::Path;
use std::time::Duration;

use super::{Action, Body, Cwd, EntryType, Finds, Library, Roots, Source, SourceAnchor};
use crate::engine::Checkouts;
use crate::fs_kind::{Find, path_matches};
use std::collections::BTreeSet;

const TEMP_PROFILE_NAMES: &[&str] = &[
    "pytest-tmp",
    "mktemp-dir",
    "claude-code-tmp",
    "chromium-tmp",
    "node-compile-cache",
    "playwright-tmp",
    "zephyr-tmp",
    "renode-tmp",
    "jest-cache",
];

#[test]
fn cmake_build_has_a_frozen_five_level_match_set_and_markers() {
    let library = Library::builtin().expect("built-ins");
    let Body::Source(source) = &library
        .get("cmake-build")
        .expect("shipped CMake profile")
        .body
    else {
        panic!("cmake-build is a source")
    };
    assert_eq!(source.roots, Roots::Required);
    assert_eq!(
        source.finds,
        Finds::Paths {
            patterns: ["*", "*/*", "*/*/*", "*/*/*/*", "*/*/*/*/*"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            exclude: Vec::new(),
            entry_type: EntryType::Dir,
        }
    );
    assert_eq!(source.where_.has.as_deref(), Some("CMakeCache.txt"));
    assert_eq!(source.where_.lacks_entry.as_deref(), Some("CMakeLists.txt"));
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
        ("/home/mark/topics/build", true),
        ("/home/mark/topics/a/b/c/d/build", true),
        ("/home/mark/topics/a/b/c/d/e/build", false),
        ("/home/mark/other/build", false),
    ] {
        assert_eq!(
            path_matches(Some(&[root]), &find.patterns, Path::new(path)),
            expected,
            "{path}"
        );
    }
}

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
fn west_build_has_a_frozen_five_level_match_set_and_zephyr_markers() {
    let library = Library::builtin().expect("built-ins");
    let Body::Source(source) = &library.get("west-build").expect("west-build").body else {
        panic!("west-build is a source")
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
    assert_eq!(
        shipped,
        TEMP_PROFILE_NAMES
            .iter()
            .copied()
            .chain(std::iter::once("containers-image-tmp"))
            .collect()
    );

    for name in TEMP_PROFILE_NAMES {
        let profile = library.get(name).expect("shipped profile");
        let Body::Source(source) = &profile.body else {
            panic!("{name} is a source")
        };
        let (roots, base, entry_type, preset, checkouts): (&[&str], &[&str], _, _, _) = match *name
        {
            "pytest-tmp" => (
                &["/tmp", "/var/tmp"],
                &["pytest-of-*/pytest-*/*"],
                EntryType::Dir,
                true,
                Checkouts::Allow,
            ),
            "mktemp-dir" => (
                &["/tmp"],
                &["tmp.*"],
                EntryType::Dir,
                true,
                Checkouts::Refuse,
            ),
            "claude-code-tmp" => (
                &["/tmp"],
                &["claude-{uid}/-*/*"],
                EntryType::Dir,
                false,
                Checkouts::Refuse,
            ),
            "chromium-tmp" => (
                &["/tmp"],
                &[".org.chromium.Chromium.*", "org.chromium.Chromium.*"],
                EntryType::Any,
                false,
                Checkouts::Refuse,
            ),
            "node-compile-cache" => (
                &["/tmp"],
                &["node-compile-cache"],
                EntryType::Dir,
                true,
                Checkouts::Refuse,
            ),
            "playwright-tmp" => (
                &["/tmp"],
                &["playwright*"],
                EntryType::Dir,
                true,
                Checkouts::Refuse,
            ),
            "zephyr-tmp" => (
                &["/tmp"],
                &["zephyr-*"],
                EntryType::Dir,
                false,
                Checkouts::Refuse,
            ),
            "renode-tmp" => (
                &["/tmp"],
                &["renode-*"],
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
        assert!(source.where_.is_empty(), "{name} predicates");
        assert_eq!(source.min_idle_for, None, "{name} minimum age");
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
fn containers_image_tmp_has_a_frozen_idle_gated_golden_set() {
    let library = Library::builtin().expect("built-ins");
    let Body::Source(source) = &library.get("containers-image-tmp").expect("built-in").body else {
        panic!("containers-image-tmp is a source");
    };
    assert_eq!(source.roots, Roots::Fixed(vec!["/var/tmp".into()]));
    assert_eq!(
        source.finds,
        Finds::Paths {
            patterns: vec!["buildah*".into(), "container_images_storage*".into()],
            exclude: Vec::new(),
            entry_type: EntryType::Any,
        }
    );
    assert!(source.where_.is_empty());
    assert_eq!(
        source.min_idle_for,
        Some(super::IdleFor(Duration::from_secs(3 * 24 * 60 * 60)))
    );
    assert_eq!(source.checkouts, Checkouts::Refuse);
    assert_eq!(
        source.action,
        Action::Delete {
            make_writable: false
        }
    );
    assert_eq!(
        matches(source),
        [
            "/var/tmp/buildah3558241755",
            "/var/tmp/container_images_storage528829445"
        ]
    );
    let mut changed = source.clone();
    let Finds::Paths { entry_type, .. } = &mut changed.finds else {
        unreachable!()
    };
    *entry_type = EntryType::Dir;
    assert_ne!(
        matches(&changed),
        matches(source),
        "type mutation must change the golden set"
    );
}

#[test]
fn xdg_cache_entry_has_a_frozen_exclusion_and_golden_set() {
    let library = Library::builtin().expect("built-ins");
    let Body::Source(source) = &library.get("xdg-cache-entry").expect("built-in").body else {
        panic!("xdg-cache-entry is a source");
    };
    assert_eq!(source.roots, Roots::Required);
    assert_eq!(
        source.finds,
        Finds::Paths {
            patterns: vec!["*".into()],
            exclude: [
                ".bun",
                "borg",
                "ccache",
                "claude",
                "containers",
                "cursor_api_sessions",
                "dconf",
                "gh",
                "ghidra",
                "go-build",
                "gpauth",
                "huggingface",
                "Microsoft",
                "nix",
                "opencode",
                "Palo Alto Networks",
                "pip",
                "pre-commit",
                "sdkmanager",
                "sessions",
                "teleport-device",
                "torch",
                "uv",
                "wine",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            entry_type: EntryType::Any,
        }
    );
    assert!(source.where_.is_empty());
    assert_eq!(
        source.min_idle_for,
        Some(super::IdleFor(Duration::from_secs(30 * 24 * 60 * 60)))
    );
    assert_eq!(source.checkouts, Checkouts::Refuse);
    assert_eq!(
        source.action,
        Action::Delete {
            make_writable: false
        }
    );
    let mut bound = source.clone();
    bound.roots = Roots::Fixed(vec!["/home/mark/.cache".into()]);
    assert_eq!(
        matches(&bound),
        [
            "/home/mark/.cache/nix-index",
            "/home/mark/.cache/icon-cache.kcache",
            "/home/mark/.cache/unknown-repo",
        ]
    );
    let mut changed = bound.clone();
    let Finds::Paths { exclude, .. } = &mut changed.finds else {
        unreachable!()
    };
    exclude.retain(|path| path != "huggingface");
    assert_ne!(
        matches(&changed),
        matches(&bound),
        "exclusion mutation must change golden set"
    );
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
            "pytest-tmp" => &[
                "/tmp/pytest-of-mark/pytest-3/test_x0",
                "/tmp/pytest-of-mark/pytest-3/popen-gw0",
                "/tmp/nix-shell.AbC123/pytest-of-ci/pytest-4/test_y0",
                "/tmp/nix-shell.AbC123/nix-shell.B2/pytest-of-ci/pytest-5/test_n0",
                "/tmp/claude-1000/nix-shell.Ab1/pytest-of-ci/pytest-6/test_c0",
                "/tmp/nix-shell-1-2/pytest-of-ci/pytest-1/test_s0",
                "/tmp/nix-develop-1-2/pytest-of-ci/pytest-1/test_d0",
                "/tmp/tmp.AbC123/pytest-of-ci/pytest-5/test_t0",
                "/var/tmp/pytest-of-ci/pytest-12/test_v0",
            ],
            "mktemp-dir" => &[
                "/tmp/nix-shell.AbC123/tmp.N3",
                "/tmp/claude-1000/tmp.X4",
                "/tmp/tmp.AbC123",
            ],
            "claude-code-tmp" => &["/tmp/claude-1000/-home-mark-src-app/session-1"],
            "chromium-tmp" => &[
                "/tmp/.org.chromium.Chromium.A",
                "/tmp/org.chromium.Chromium.X",
            ],
            "node-compile-cache" => &[
                "/tmp/nix-shell.AbC123/node-compile-cache",
                "/tmp/claude-1000/nix-shell.Ab1/node-compile-cache",
                "/tmp/nix-shell-1-2/node-compile-cache",
                "/tmp/nix-develop-1-2/node-compile-cache",
                "/tmp/node-compile-cache",
            ],
            "playwright-tmp" => &[
                "/tmp/nix-shell.AbC123/playwright_chromiumdev_profile-X",
                "/tmp/claude-1000/nix-shell.Ab1/playwright-artifacts",
                "/tmp/nix-shell-1-2/playwright-artifacts",
                "/tmp/nix-develop-1-2/playwright-artifacts",
                "/tmp/playwright-artifacts-1",
            ],
            "zephyr-tmp" => &["/tmp/zephyr-h5e5-run.X", "/tmp/zephyr-scratch"],
            "renode-tmp" => &[
                "/tmp/nix-shell.AbC123/renode-123",
                "/tmp/nix-shell-1-2/renode-1",
                "/tmp/nix-develop-1-2/renode-1",
                "/tmp/zephyr-h5e5-run.X/renode-1",
                "/tmp/renode-1045514-crash",
            ],
            "jest-cache" => &[
                "/tmp/nix-shell.AbC123/jest_app_hash",
                "/tmp/jest_rs",
                "/var/tmp/jest_project_hash",
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
    let Body::Source(original) = &library.get("mktemp-dir").expect("mktemp-dir").body else {
        panic!("mktemp-dir is a source");
    };
    let mut changed = original.clone();
    let Finds::Paths { patterns, .. } = &mut changed.finds else {
        panic!("mktemp-dir has path patterns");
    };
    patterns[0] = "tmp*".to_owned();
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
