//! Kind `fs` in `ensure --dry-run` (task-68's spike, diskgc-core
//! `fs_kind`): segment-wise find patterns plan candidates, each judged by
//! the engine's eligibility walk and the in-use check, as the reclaim
//! judges them (tests/fs_reclaim.rs). Profiles are user profiles in the
//! fixtures.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{dir, host};
use diskgc_core::config;
use diskgc_core::engine::{
    Candidate, Checkouts, DeleteOptions, End, Rules, delete_unchecked, walk,
};
use diskgc_core::executor::{self, Request, SourceKind, Tier};
use diskgc_core::inuse::MAX_LISTINGS;
use diskgc_core::profile::{Action, Body, Library, parse_profiles};
use diskgc_core::report::{Report, Status};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

/// The user profiles the tests select from. `pytest-of-dir` is a stand-in
/// with whole `pytest-of-*` dirs as candidates (simple fixtures), not the
/// former built-in `pytest-tmp` (removed after the no-descriptor live-test
/// counterexample). `pytest-cache-fixture` pins an older depth
/// rule for the roots test; the shipped cache profile has its own golden
/// fixture below.
const PROFILES: &str = r#"
- name: pytest-of-dir
  find:
    roots: [/tmp, /var/tmp]
    paths: ["pytest-of-*", "nix-shell.*/pytest-of-*", "tmp.*/pytest-of-*"]
    type: dir
  action: delete
  examples: {match: [/tmp/pytest-of-mark]}
- name: tmp-entry
  find: {roots: [/tmp], paths: ["*"], type: any}
  action: delete
  examples: {match: [/tmp/x]}
- name: pytest-cache-fixture
  find:
    roots: required
    paths: [".pytest_cache", "*/.pytest_cache", "*/*/.pytest_cache"]
    type: dir
  action: delete
  examples: {match: [/home/mark/src/app/.pytest_cache]}
- name: marker-build-fixture
  find: {roots: required, paths: ["*", "*/*"], type: dir}
  where: {has: CMakeCache.txt, lacks_entry: CMakeLists.txt}
  action: delete
  examples: {root: /home/mark/src, match: [/home/mark/src/build]}
- name: has-tag
  where: {has: CACHEDIR.TAG}
- name: has-not-tag
  where: {has_not: CACHEDIR.TAG}
- name: tagged-idle-unkept
  where: {has: CACHEDIR.TAG, has_not: .keep, idle_for: 2d}
- name: unmounted
  find: {roots: [/tmp], paths: ["pytest-of-*"], type: dir}
  where: {has_not: mounted}
  action: delete
  examples: {match: [/tmp/pytest-of-mark]}
- name: anything
  find: {roots: required, paths: ["*"], type: any}
  action: delete
  examples: {match: [/srv/x]}
- name: deep-dirs
  find: {roots: required, paths: ["*/*/*"], type: dir}
  action: delete
  examples: {match: [/srv/a/b/c]}
- name: tmp-entry-checkouts
  find: {roots: [/tmp], paths: ["*"], type: any}
  checkouts: allow
  action: delete
  examples: {match: [/tmp/x]}
- name: nested-dir
  find: {roots: [/tmp], paths: ["nix-shell.*/pytest-of-*"], type: dir}
  action: delete
  examples: {match: [/tmp/nix-shell.A/pytest-of-mark]}
- name: deep-dirs-checkouts
  find: {roots: required, paths: ["*/*/*"], type: dir}
  checkouts: allow
  action: delete
  examples: {match: [/srv/a/b/c]}
- name: excluded-prefix
  find:
    roots: [/tmp]
    paths: ["excluded", "excluded/item", "allowed/item"]
    exclude: ["excluded"]
    type: dir
  action: delete
  examples: {match: [/tmp/excluded/item], no_match: [/tmp/excluded]}
- name: generic-versions
  find: {roots: required, paths: [versions/*], type: dir}
  action: delete
  examples: {match: [/home/mark/versions/v1]}
- name: keep-two
  where: {keep_newest: 2}
- name: tagged-keep-one
  where: {has: CACHEDIR.TAG, idle_for: 2d, keep_newest: 1}
- name: union-file-a
  find: {roots: [/tmp], paths: [link-a], type: file}
  action: delete
  examples: {match: [/tmp/link-a]}
- name: union-file-b
  find: {roots: [/tmp], paths: [link-b], type: file}
  action: delete
  examples: {match: [/tmp/link-b]}
- name: negated-descendant
  find: {roots: [/tmp], paths: [neg-parent/child], type: dir}
  where: {has: deliberately-absent}
  action: delete
  examples: {match: [/tmp/neg-parent/child]}
- name: negated-private-tmp
  find: {roots: [/tmp], paths: ["systemd-private-*"], type: dir}
  action: delete
  examples: {match: [/tmp/systemd-private-unit]}
"#;

fn library() -> Library {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"))
}

/// `/` (ext4), `/home` (ext4) and `/tmp` (tmpfs, mount 30, dev 0:42); the
/// clock is 2026-10-15. `extra`: more top-level keys (tree entries first).
fn yaml(extra: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 100G, parent: 1}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}}
tree:
  /usr/bin/tool: {{}}
{extra}"#
    )
}

/// A process that is fully readable (exe, a map, an fd, an environment;
/// an empty one reads as Failed, as on LinuxHost), with `cwd` and further
/// environment entries `env`.
fn process(pid: u32, cwd: &str, env: &str) -> String {
    format!(
        "  - {{pid: {pid}, exe: /usr/bin/tool, cwd: {cwd}, maps: [/usr/bin/tool], fds: [{{path: \
         /usr/bin/tool}}], environ: [PATH=/usr/bin{env}]}}\n"
    )
}

fn request(select: &str, roots: &[&str], path: &str) -> Request {
    let roots: Vec<String> = roots.iter().map(|r| (*r).to_owned()).collect();
    let roots = (!roots.is_empty()).then_some(roots.as_slice());
    Request {
        path: PathBuf::from(path),
        min_free: parse_size("1G").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select(select, &library(), roots, 1000).unwrap_or_else(|e| panic!("{e}")),
    }
}

/// A shipped profile uses its embedded definition, including its real
/// action and exact path patterns, rather than the spike profiles above.
fn built_in_request(select: &str, root: &str) -> Request {
    let library = Library::builtin().expect("built-in profiles");
    let roots = [root.to_owned()];
    Request {
        path: PathBuf::from(root),
        min_free: parse_size("1G").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select(select, &library, Some(&roots), 1000).unwrap_or_else(|e| panic!("{e}")),
    }
}

/// A source's `min_idle_for` floor is applied by the real dry-run executor
/// without a tier filter. The model clock makes 3-day-old, new and future
/// entries deterministic while a process holds one old file open. The
/// profile is a user profile with the matching set of the withdrawn
/// `containers-image-tmp` (TASK-112 D2), which is not shipped.
#[test]
fn a_source_floor_requires_its_age_and_refuses_open_files() {
    let model = yaml(
        r#"  /var/tmp/buildah-old: {.: {mtime: 2026-10-01T00:00:00Z}, image: {size: 1K, mtime: 2026-10-01T00:00:00Z}}
  /var/tmp/container_images_storage-old: {size: 2K, mtime: 2026-10-01T00:00:00Z}
  /var/tmp/buildah-fresh: {size: 1K, mtime: 2026-10-14T00:00:00Z}
  /var/tmp/container_images_storage-busy: {size: 1K, mtime: 2026-10-01T00:00:00Z}
  /var/tmp/buildah-future: {size: 1K, mtime: 2026-10-16T00:00:00Z}
processes:
  - {pid: 42, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{path: /var/tmp/container_images_storage-busy}], environ: [PATH=/usr/bin]}
"#,
    );
    let host = host(&model);
    let user = parse_profiles(
        "- name: spool-probe\n  find: {roots: [/var/tmp], paths: [\"buildah*\", \"container_images_storage*\"], type: any}\n  min_idle_for: 3d\n  action: delete\n  examples: {match: [/var/tmp/buildah1]}\n",
    )
    .unwrap();
    let library = Library::with_user_profiles(&user).unwrap();
    assert!(
        Library::builtin()
            .unwrap()
            .get("containers-image-tmp")
            .is_none()
    );
    let tier = Tier::select("profile:spool-probe", &library, None, 1000).unwrap();
    assert_eq!(
        tier.idle_for,
        Some(diskgc_core::profile::IdleFor(Duration::from_secs(
            3 * 86_400
        )))
    );
    let request = Request {
        path: PathBuf::from("/var/tmp"),
        min_free: parse_size("100G").unwrap(),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier,
    };
    let report = dry_run(&host, &request);
    for (candidate, reason) in [
        ("/var/tmp/buildah-old", None),
        ("/var/tmp/container_images_storage-old", None),
        ("/var/tmp/buildah-fresh", Some("not_idle")),
        ("/var/tmp/container_images_storage-busy", Some("in_use")),
        ("/var/tmp/buildah-future", Some("future_time")),
    ] {
        assert_eq!(reason_of(&report, candidate), Some(reason), "{candidate}");
    }
    nothing_locked_or_deleted(&host);
}

/// Every shipped browser source has its own finite cache match set and lock.
/// Other browsers, profile data, site storage, similarly named directories,
/// and a file at a cache-shaped path must stay outside discovery. A live
/// SingletonLock makes every cache of its browser ineligible.
#[test]
fn browser_cache_builtins_have_golden_sets_and_refuse_live_locks() {
    let browsers = [
        (
            "brave-browser-cache",
            "BraveSoftware/Brave-Browser",
            "BraveSoftware/Brave-Browser",
        ),
        ("chrome-browser-cache", "google-chrome", "google-chrome"),
        ("chromium-browser-cache", "chromium", "chromium"),
        ("edge-browser-cache", "microsoft-edge", "microsoft-edge"),
    ];
    let mut tree = String::new();
    for (_, cache, config) in browsers {
        tree.push_str(&format!(
            r#"  /home/mark/.cache/{cache}/Default/Cache: {{.: {{}}, data: {{size: 4K}}}}
  /home/mark/.cache/{cache}/Default/Code Cache: {{.: {{}}, js: {{size: 2K}}}}
  /home/mark/.cache/{cache}/Profile 1/Cache: {{.: {{}}, data: {{size: 1K}}}}
  /home/mark/.cache/{cache}/Profile 1/Code Cache: {{.: {{}}, js: {{size: 1K}}}}
  /home/mark/.cache/{cache}/Profile 2/Cache: {{size: 1K}}
  /home/mark/.cache/{cache}/Profileish/Cache: {{.: {{}}}}
  /home/mark/.cache/{cache}/Default/Cache.backup: {{.: {{}}}}
  /home/mark/.config/{config}/Default/WebStorage: {{.: {{}}, site: {{size: 9K}}}}
  /home/mark/.config/{config}/Default/Service Worker/CacheStorage: {{.: {{}}, site: {{size: 9K}}}}
"#
        ));
    }
    let stopped_host = host(&yaml(&tree));
    let mut live_tree = tree.clone();
    for (_, _, config) in browsers {
        live_tree.push_str(&format!(
            "  /home/mark/.config/{config}/SingletonLock: {{symlink: model-host-42}}\n"
        ));
    }
    live_tree.push_str(&format!(
        "hostname: model-host\nprocesses:\n{}",
        process(42, "/", "")
    ));
    let live_host = host(&yaml(&live_tree));

    for (profile, cache, _) in browsers {
        let request = built_in_request(&format!("profile:{profile}"), "/home/mark");
        let mut expected: Vec<(String, Option<&'static str>)> = [
            "Default/Cache",
            "Default/Code Cache",
            "Profile 1/Cache",
            "Profile 1/Code Cache",
        ]
        .into_iter()
        .map(|suffix| (format!("/home/mark/.cache/{cache}/{suffix}"), None))
        .collect();
        expected.sort();

        let mut stopped = plan(&dry_run(&stopped_host, &request));
        stopped.sort();
        assert_eq!(stopped, expected, "{profile} stopped match set");

        let mut guarded = plan(&dry_run(&live_host, &request));
        guarded.sort();
        let blocked: Vec<_> = expected
            .iter()
            .map(|(path, _)| (path.clone(), Some("in_use")))
            .collect();
        assert_eq!(guarded, blocked, "{profile} live lock");
    }
    nothing_locked_or_deleted(&stopped_host);
    nothing_locked_or_deleted(&live_host);
}

/// Each entry is present in the same fixture, including similar but
/// out-of-scope names. The expected set pins what every built-in matches;
/// changing it requires a new profile name, even when it narrows a match.
#[test]
fn shipped_home_cache_profiles_have_golden_match_sets() {
    let host = host(&yaml(
        r#"  /home/mark/src/.pytest_cache: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/src/app/.pytest_cache: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/src/a/b/.pytest_cache: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/src/a/b/c/.pytest_cache: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/src/a/b/c/d/.pytest_cache: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cache/nix/gitv3: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cache/nix/tarball-cache: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cache/nix/tarball-cache-v2: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cache/nix/eval-cache-v6: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cache/nix/eval-cache-v7: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cache/nix/fetcher-cache-v1: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.bun/install/cache: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cache/.bun/install/cache: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/go/pkg/mod: {.: {mtime: 2026-10-01T00:00:00Z}, example: {.: {mode: "0555"}, file: {size: 1K}}}
  /home/mark/other/pkg/mod: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/go/pkg/other: {.: {mtime: 2026-10-01T00:00:00Z}}
"#,
    ));
    for (profile, root, expected) in [
        (
            "pytest-cache",
            "/home/mark/src",
            vec![
                "/home/mark/src/.pytest_cache",
                "/home/mark/src/a/b/.pytest_cache",
                "/home/mark/src/a/b/c/.pytest_cache",
                "/home/mark/src/app/.pytest_cache",
            ],
        ),
        (
            "nix-user-cache",
            "/home/mark",
            vec![
                "/home/mark/.cache/nix/eval-cache-v6",
                "/home/mark/.cache/nix/eval-cache-v7",
                "/home/mark/.cache/nix/gitv3",
                "/home/mark/.cache/nix/tarball-cache",
                "/home/mark/.cache/nix/tarball-cache-v2",
            ],
        ),
        (
            "bun-cache",
            "/home/mark",
            vec![
                "/home/mark/.bun/install/cache",
                "/home/mark/.cache/.bun/install/cache",
            ],
        ),
        ("go-mod-cache", "/home/mark", vec!["/home/mark/go/pkg/mod"]),
    ] {
        let report = dry_run(
            &host,
            &built_in_request(&format!("profile:{profile}"), root),
        );
        let mut actual: Vec<_> = plan(&report).into_iter().map(|(path, _)| path).collect();
        actual.sort();
        assert_eq!(actual, expected, "{profile}");
        assert!(
            report.plan.iter().all(|p| p.reason.is_none()),
            "{profile}: {:#?}",
            report.plan
        );
    }
    nothing_locked_or_deleted(&host);
}

/// The command profiles measure cache roots, never a cache's internal data
/// or a neighboring package store. Their exact paths are part of the
/// built-in name's contract.
#[test]
fn command_cache_builtins_have_golden_match_sets() {
    let host = host(&yaml(
        r#"  /usr/bin/pip: {mode: "0755"}
  /usr/bin/uv: {mode: "0755"}
  /usr/bin/go: {mode: "0755"}
  /usr/bin/npm: {mode: "0755"}
  /usr/bin/pnpm: {mode: "0755"}
  /usr/bin/yarn: {mode: "0755"}
  /usr/bin/ccache: {mode: "0755"}
  /usr/bin/env: {mode: "0755"}
  /home/mark/.cache/pip: {.: {}, http-v2: {.: {}, body: {size: 1K}}}
  /home/mark/.cache/uv: {.: {}, archive-v0: {.: {}, package: {size: 1K}}}
  /home/mark/.cache/go-build: {.: {}, 00: {.: {}, object: {size: 1K}}}
  /home/mark/.npm: {.: {}, _cacache: {.: {}, content: {size: 1K}}}
  /home/mark/.local/share/pnpm/store: {.: {}, v10: {.: {}, content: {size: 1K}}}
  /home/mark/.cache/yarn: {.: {}, v6: {.: {}, package: {size: 1K}}}
  /home/mark/.cache/ccache: {.: {}, tmp: {.: {}, object: {size: 1K}}}
  /home/mark/.ccache: {.: {}, 0: {.: {}, object: {size: 1K}}}
  /home/mark/.cache/sccache: {.: {}}
  /home/mark/go/pkg/mod: {.: {}}
  /home/mark/.local/share/pnpm/other: {.: {}}
"#,
    ));
    for (profile, expected) in [
        ("pip-cache", vec!["/home/mark/.cache/pip"]),
        ("uv-cache", vec!["/home/mark/.cache/uv"]),
        ("go-build-cache", vec!["/home/mark/.cache/go-build"]),
        ("npm-cache", vec!["/home/mark/.npm"]),
        ("pnpm-store", vec!["/home/mark/.local/share/pnpm/store"]),
        ("yarn-cache", vec!["/home/mark/.cache/yarn"]),
        (
            "ccache",
            vec!["/home/mark/.cache/ccache", "/home/mark/.ccache"],
        ),
    ] {
        let report = dry_run(
            &host,
            &built_in_request(&format!("profile:{profile}"), "/home/mark"),
        );
        let mut actual: Vec<_> = plan(&report).into_iter().map(|(path, _)| path).collect();
        actual.sort();
        assert_eq!(actual, expected, "{profile}");
    }
    nothing_locked_or_deleted(&host);
}

/// The shipped version sources rank only matching siblings and protect all
/// configured installed pointers, including a relative link into an old
/// version. Every candidate is still visible in the plan with its reason.
#[test]
fn shipped_version_profiles_have_golden_judgements() {
    let host = host(&yaml(
        r#"  /home/mark/.local/share/cursor-agent/versions/v0: {.: {mtime: 2026-10-01T00:00:00Z}, cursor-agent: {size: 1K}}
  /home/mark/.local/share/cursor-agent/versions/v1: {.: {mtime: 2026-10-02T00:00:00Z}, cursor-agent: {size: 1K}}
  /home/mark/.local/share/cursor-agent/versions/v2: {.: {mtime: 2026-10-03T00:00:00Z}, cursor-agent: {size: 1K}}
  /home/mark/.local/share/cursor-agent/versions/v3: {.: {mtime: 2026-10-04T00:00:00Z}, cursor-agent: {size: 1K}}
  /home/mark/.local/share/cursor-agent/versions/v4: {.: {mtime: 2026-10-05T00:00:00Z}, cursor-agent: {size: 1K}}
  /home/mark/.local/bin/agent: {symlink: /home/mark/.local/share/cursor-agent/versions/v1/cursor-agent}
  /home/mark/.local/bin/cursor-agent: {symlink: ../share/cursor-agent/versions/v2/cursor-agent}
  /home/mark/.codex/packages/standalone/releases/v1: {.: {mtime: 2026-10-01T00:00:00Z}, bin: {size: 1K}}
  /home/mark/.codex/packages/standalone/releases/v2: {.: {mtime: 2026-10-02T00:00:00Z}, bin: {size: 1K}}
  /home/mark/.codex/packages/standalone/releases/v3: {.: {mtime: 2026-10-03T00:00:00Z}, bin: {size: 1K}}
  /home/mark/.codex/packages/standalone/releases/v4: {.: {mtime: 2026-10-04T00:00:00Z}, bin: {size: 1K}}
  /home/mark/.codex/packages/standalone/current: {symlink: releases/v1}
  /home/mark/.local/share/claude/versions/1: {size: 1K, mtime: 2026-10-01T00:00:00Z}
  /home/mark/.local/share/claude/versions/2: {size: 1K, mtime: 2026-10-02T00:00:00Z}
  /home/mark/.local/share/claude/versions/3: {size: 1K, mtime: 2026-10-03T00:00:00Z}
  /home/mark/.local/share/claude/versions/4: {size: 1K, mtime: 2026-10-04T00:00:00Z}
  /home/mark/.local/bin/claude: {symlink: ../share/claude/versions/1}
  /home/mark/bin/claude-0.9: {size: 1K}
  /home/mark/.cursor/extensions/a.ext-1.0.0: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cursor/extensions/a.ext-2.0.0: {.: {mtime: 2026-10-02T00:00:00Z}}
  /home/mark/.cursor/extensions/a.ext-3.0.0: {.: {mtime: 2026-10-03T00:00:00Z}}
  /home/mark/.cursor/extensions/a.ext-4.0.0: {.: {mtime: 2026-10-04T00:00:00Z}}
  /home/mark/.cursor/extensions/b.ext-1.0.0: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cursor/extensions/unknown: {.: {mtime: 2026-09-01T00:00:00Z}}
  /home/mark/.cursor/extensions/extensions.json: {content: '[{"relativeLocation":"a.ext-1.0.0","location":{"path":"/home/mark/.cursor/extensions/a.ext-1.0.0"}},{"relativeLocation":"b.ext-1.0.0","location":{"path":"/home/mark/.cursor/extensions/b.ext-1.0.0"}}]'}
"#,
    ));
    for (profile, suffixes) in [
        (
            "cursor-agent-versions",
            vec![
                ("v0", None),
                ("v1", Some("linked_current")),
                ("v2", Some("linked_current")),
                ("v3", Some("newest_version")),
                ("v4", Some("newest_version")),
            ],
        ),
        (
            "codex-releases",
            vec![
                ("v1", Some("linked_current")),
                ("v2", None),
                ("v3", Some("newest_version")),
                ("v4", Some("newest_version")),
            ],
        ),
        (
            "claude-code-versions",
            vec![
                ("1", Some("linked_current")),
                ("2", None),
                ("3", Some("newest_version")),
                ("4", Some("newest_version")),
            ],
        ),
        (
            "cursor-extensions",
            vec![
                ("a.ext-1.0.0", Some("installed_extension")),
                ("a.ext-2.0.0", None),
                ("a.ext-3.0.0", Some("newest_version")),
                ("a.ext-4.0.0", Some("newest_version")),
                ("b.ext-1.0.0", Some("installed_extension")),
                ("unknown", Some("newest_version")),
            ],
        ),
    ] {
        let prefix = match profile {
            "cursor-agent-versions" => "/home/mark/.local/share/cursor-agent/versions",
            "codex-releases" => "/home/mark/.codex/packages/standalone/releases",
            "claude-code-versions" => "/home/mark/.local/share/claude/versions",
            _ => "/home/mark/.cursor/extensions",
        };
        let report = dry_run(
            &host,
            &built_in_request(&format!("profile:{profile}"), "/home/mark"),
        );
        assert_eq!(report.status, Status::DryRun, "{profile}: {report:#?}");
        assert_eq!(
            report.plan.len(),
            suffixes.len(),
            "{profile}: {:#?}",
            report.plan
        );
        for (name, reason) in suffixes {
            assert_eq!(
                reason_of(&report, &format!("{prefix}/{name}")),
                Some(reason),
                "{profile}: {:#?}",
                report.plan
            );
        }
    }
    nothing_locked_or_deleted(&host);
}

#[test]
fn cursor_extension_groups_keep_each_id_newest_even_with_numeric_publisher() {
    let primary_host = host(&yaml(
        r#"  /home/mark/.cursor/extensions/acme-2.tool-1.0.0: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cursor/extensions/acme-2.tool-2.0.0: {.: {mtime: 2026-10-02T00:00:00Z}}
  /home/mark/.cursor/extensions/acme-2.tool-3.0.0: {.: {mtime: 2026-10-03T00:00:00Z}}
  /home/mark/.cursor/extensions/acme-3.tool-1.0.0: {.: {mtime: 2026-10-04T00:00:00Z}}
  /home/mark/.cursor/extensions/acme-3.tool-2.0.0: {.: {mtime: 2026-10-05T00:00:00Z}}
  /home/mark/.cursor/extensions/acme-3.tool-3.0.0: {.: {mtime: 2026-10-06T00:00:00Z}}
  /home/mark/.cursor/extensions/extensions.json: {content: '[]'}
"#,
    ));
    let report = dry_run(
        &primary_host,
        &built_in_request("profile:cursor-extensions", "/home/mark"),
    );
    let base = "/home/mark/.cursor/extensions/";
    assert_eq!(
        reason_of(&report, &format!("{base}acme-2.tool-1.0.0")),
        Some(None)
    );
    assert_eq!(
        reason_of(&report, &format!("{base}acme-3.tool-1.0.0")),
        Some(None)
    );
    for name in [
        "acme-2.tool-2.0.0",
        "acme-2.tool-3.0.0",
        "acme-3.tool-2.0.0",
        "acme-3.tool-3.0.0",
    ] {
        assert_eq!(
            reason_of(&report, &format!("{base}{name}")),
            Some(Some("newest_version")),
            "{name}: {:#?}",
            report.plan
        );
    }
    nothing_locked_or_deleted(&primary_host);

    let ambiguous = host(&yaml(
        r#"  /home/mark/.cursor/extensions/acme-3.tool-1.0.0: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cursor/extensions/acme-3.tool-2.0.0: {.: {mtime: 2026-10-02T00:00:00Z}}
  /home/mark/.cursor/extensions/acme-3.tool-3.0.0: {.: {mtime: 2026-10-03T00:00:00Z}}
  /home/mark/.cursor/extensions/acme-3.tool-3.0.0-2.0.0: {.: {mtime: 2026-10-04T00:00:00Z}}
  /home/mark/.cursor/extensions/extensions.json: {content: '[]'}
"#,
    ));
    let report = dry_run(
        &ambiguous,
        &built_in_request("profile:cursor-extensions", "/home/mark"),
    );
    for name in ["acme-3.tool-1.0.0", "acme-3.tool-3.0.0-2.0.0"] {
        assert_eq!(
            reason_of(&report, &format!("{base}{name}")),
            Some(Some("newest_version")),
            "{name}: {:#?}",
            report.plan
        );
    }
    nothing_locked_or_deleted(&ambiguous);
}

#[test]
fn version_profile_guard_mutations_expose_protected_old_candidates() {
    let cases = [
        (
            "codex-releases",
            r#"  /home/mark/.codex/packages/standalone/releases/v1: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.codex/packages/standalone/releases/v2: {.: {mtime: 2026-10-02T00:00:00Z}}
  /home/mark/.codex/packages/standalone/releases/v3: {.: {mtime: 2026-10-03T00:00:00Z}}
  /home/mark/.codex/packages/standalone/releases/v4: {.: {mtime: 2026-10-04T00:00:00Z}}
  /home/mark/.codex/packages/standalone/current: {symlink: releases/v1}
"#,
            "/home/mark/.codex/packages/standalone/releases/v1",
        ),
        (
            "claude-code-versions",
            r#"  /home/mark/.local/share/claude/versions/1: {size: 1K, mtime: 2026-10-01T00:00:00Z}
  /home/mark/.local/share/claude/versions/2: {size: 1K, mtime: 2026-10-02T00:00:00Z}
  /home/mark/.local/share/claude/versions/3: {size: 1K, mtime: 2026-10-03T00:00:00Z}
  /home/mark/.local/share/claude/versions/4: {size: 1K, mtime: 2026-10-04T00:00:00Z}
  /home/mark/.local/bin/claude: {symlink: ../share/claude/versions/1}
"#,
            "/home/mark/.local/share/claude/versions/1",
        ),
        (
            "cursor-extensions",
            r#"  /home/mark/.cursor/extensions/acme.tool-1.0.0: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cursor/extensions/acme.tool-2.0.0: {.: {mtime: 2026-10-02T00:00:00Z}}
  /home/mark/.cursor/extensions/acme.tool-3.0.0: {.: {mtime: 2026-10-03T00:00:00Z}}
  /home/mark/.cursor/extensions/acme.tool-4.0.0: {.: {mtime: 2026-10-04T00:00:00Z}}
  /home/mark/.cursor/extensions/extensions.json: {content: '[{"relativeLocation":"acme.tool-1.0.0","location":{"path":"/home/mark/.cursor/extensions/acme.tool-1.0.0"}}]'}
"#,
            "/home/mark/.cursor/extensions/acme.tool-1.0.0",
        ),
    ];
    for (profile, tree, candidate) in cases {
        let model = host(&yaml(tree));
        let mut request = built_in_request(&format!("profile:{profile}"), "/home/mark");
        let guarded = dry_run(&model, &request);
        let expected_reason = if profile == "cursor-extensions" {
            "installed_extension"
        } else {
            "linked_current"
        };
        assert_eq!(
            reason_of(&guarded, candidate),
            Some(Some(expected_reason)),
            "{profile}: {:#?}",
            guarded.plan
        );
        let SourceKind::Fs(find) = &mut request.tier.source else {
            panic!("{profile} is a filesystem profile");
        };
        match profile {
            "codex-releases" => find.protect_parent_link = None,
            "claude-code-versions" => find.protect_link_dirs.clear(),
            "cursor-extensions" => find.protect_cursor_extensions = None,
            _ => unreachable!(),
        }
        let mutated = dry_run(&model, &request);
        assert_eq!(
            reason_of(&mutated, candidate),
            Some(None),
            "removing {profile}'s guard must expose {candidate}: {:#?}",
            mutated.plan
        );
        nothing_locked_or_deleted(&model);
    }
}

#[test]
fn keep_newest_filter_really_narrows_a_source_term() {
    let host = host(&yaml(
        r#"  /home/mark/versions/v1: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/versions/v2: {.: {mtime: 2026-10-02T00:00:00Z}}
  /home/mark/versions/v3: {.: {mtime: 2026-10-03T00:00:00Z}}
"#,
    ));
    let bare = dry_run(
        &host,
        &request("profile:generic-versions", &["/home/mark"], "/home/mark"),
    );
    assert!(bare.plan.iter().all(|entry| entry.reason.is_none()));
    let filtered = dry_run(
        &host,
        &request(
            "profile:generic-versions && filter:keep-two",
            &["/home/mark"],
            "/home/mark",
        ),
    );
    assert_eq!(reason_of(&filtered, "/home/mark/versions/v1"), Some(None));
    assert_eq!(
        reason_of(&filtered, "/home/mark/versions/v2"),
        Some(Some("newest_version"))
    );
    assert_eq!(
        reason_of(&filtered, "/home/mark/versions/v3"),
        Some(Some("newest_version"))
    );
    nothing_locked_or_deleted(&host);
}

#[test]
fn entry_and_version_filter_conjoin_in_dry_run() {
    let host = host(&yaml(
        r#"  /home/mark/versions/v1: {.: {mtime: 2026-10-01T00:00:00Z}, CACHEDIR.TAG: {size: 1K, mtime: 2026-10-01T00:00:00Z}}
  /home/mark/versions/v2: {.: {mtime: 2026-10-02T00:00:00Z}, payload: {size: 1K, mtime: 2026-10-02T00:00:00Z}}
  /home/mark/versions/v3: {.: {mtime: 2026-10-03T00:00:00Z}, CACHEDIR.TAG: {size: 1K, mtime: 2026-10-03T00:00:00Z}}
"#,
    ));
    let selection = "profile:generic-versions && filter:tagged-keep-one";
    let report = dry_run(&host, &request(selection, &["/home/mark"], "/home/mark"));
    assert_eq!(reason_of(&report, "/home/mark/versions/v1"), Some(None));
    assert_eq!(
        reason_of(&report, "/home/mark/versions/v2"),
        Some(Some("filtered"))
    );
    assert_eq!(
        reason_of(&report, "/home/mark/versions/v3"),
        Some(Some("newest_version"))
    );
    nothing_locked_or_deleted(&host);
}

/// A dangling launcher inside the source root blocks reclaim; a resolved
/// launcher for an unrelated tool outside it has no candidate to protect.
#[test]
fn unresolvable_installed_link_retains_every_old_version() {
    let versions = r#"  /home/mark/.local/share/cursor-agent/versions/v1: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.local/share/cursor-agent/versions/v2: {.: {mtime: 2026-10-02T00:00:00Z}}
  /home/mark/.local/share/cursor-agent/versions/v3: {.: {mtime: 2026-10-03T00:00:00Z}}
  /home/mark/.local/share/cursor-agent/versions/v4: {.: {mtime: 2026-10-04T00:00:00Z}}
  /home/mark/.local/bin/cursor-agent: {symlink: ../share/cursor-agent/versions/v4}
  /tmp/foreign: {size: 1K}
"#;
    for (target, expected) in [
        (
            "../share/cursor-agent/versions/missing",
            Some("linked_current"),
        ),
        ("/tmp/foreign", None),
    ] {
        let entries = format!("{versions}  /home/mark/.local/bin/agent: {{symlink: {target}}}\n");
        let host = host(&yaml(&entries));
        let report = dry_run(
            &host,
            &built_in_request("profile:cursor-agent-versions", "/home/mark"),
        );
        for old in ["v1", "v2"] {
            assert_eq!(
                reason_of(
                    &report,
                    &format!("/home/mark/.local/share/cursor-agent/versions/{old}")
                ),
                Some(expected),
                "{target}: {:#?}",
                report.plan
            );
        }
        nothing_locked_or_deleted(&host);
    }
}

/// An alias name is not predictable from the app. Clearing the directory
/// predicate in the otherwise identical compiled tier makes v0 eligible,
/// so this fixture checks that the alias rule actually changes the plan.
#[test]
fn launcher_alias_outside_the_named_links_protects_an_old_version() {
    let host = host(&yaml(
        r#"  /home/mark/.local/share/cursor-agent/versions/v0: {.: {mtime: 2026-10-01T00:00:00Z}, agent: {size: 1K}}
  /home/mark/.local/share/cursor-agent/versions/v1: {.: {mtime: 2026-10-02T00:00:00Z}}
  /home/mark/.local/share/cursor-agent/versions/v2: {.: {mtime: 2026-10-03T00:00:00Z}}
  /home/mark/.local/share/cursor-agent/versions/v3: {.: {mtime: 2026-10-04T00:00:00Z}}
  /home/mark/.local/share/cursor-agent/versions/v4: {.: {mtime: 2026-10-05T00:00:00Z}}
  /home/mark/.local/bin/agent: {symlink: /home/mark/.local/share/cursor-agent/versions/v4}
  /home/mark/bin/renamed-launcher: {symlink: /home/mark/.local/share/cursor-agent/versions/v0/agent}
  /home/mark/bin/unrelated: {symlink: /usr/bin/tool}
"#,
    ));
    let mut request = built_in_request("profile:cursor-agent-versions", "/home/mark");
    let guarded = dry_run(&host, &request);
    assert_eq!(
        reason_of(&guarded, "/home/mark/.local/share/cursor-agent/versions/v0"),
        Some(Some("linked_current"))
    );
    assert_eq!(
        reason_of(&guarded, "/home/mark/.local/share/cursor-agent/versions/v1"),
        Some(None)
    );
    let SourceKind::Fs(find) = &mut request.tier.source else {
        panic!("Cursor agent is a filesystem profile")
    };
    find.protect_link_dirs.clear();
    let mutated = dry_run(&host, &request);
    assert_eq!(
        reason_of(&mutated, "/home/mark/.local/share/cursor-agent/versions/v0"),
        Some(None),
        "removing directory-wide protection must expose the old alias target"
    );
    nothing_locked_or_deleted(&host);
}

#[test]
fn absent_launcher_directory_is_empty_but_unsafe_or_unreadable_retains() {
    let versions = r#"  /home/mark/.local/share/claude/versions/1: {size: 1K, mtime: 2026-10-01T00:00:00Z}
  /home/mark/.local/share/claude/versions/2: {size: 1K, mtime: 2026-10-02T00:00:00Z}
  /home/mark/.local/share/claude/versions/3: {size: 1K, mtime: 2026-10-03T00:00:00Z}
  /home/mark/.local/share/claude/versions/4: {size: 1K, mtime: 2026-10-04T00:00:00Z}
  /home/mark/.local/bin/claude: {symlink: ../share/claude/versions/4}
"#;
    for (case, extra, expected) in [
        ("missing", String::new(), None),
        (
            "unsafe symlink directory",
            "  /home/mark/bin: {symlink: /tmp}\n".to_owned(),
            Some("linked_current"),
        ),
        (
            "unreadable",
            "  /home/mark/bin/other: {size: 1K}\nevents:\n  - {at: {op: read_dir, path: /home/mark/bin, when: before}, fail: EACCES}\n".to_owned(),
            Some("linked_current"),
        ),
    ] {
        let host = host(&yaml(&format!("{versions}{extra}")));
        let report = dry_run(
            &host,
            &built_in_request("profile:claude-code-versions", "/home/mark"),
        );
        assert_eq!(
            reason_of(&report, "/home/mark/.local/share/claude/versions/1"),
            Some(expected),
            "{case}: {:#?}",
            report.plan
        );
        nothing_locked_or_deleted(&host);
    }
}

#[test]
fn internal_dotdot_in_current_link_is_ambiguous_and_retains_old_release() {
    let host = host(&yaml(
        r#"  /home/mark/.codex/packages/standalone/releases/v0: {.: {mtime: 2026-10-01T00:00:00Z}, releases: {v1: {size: 1K}}, child: {}}
  /home/mark/.codex/packages/standalone/releases/v1: {.: {mtime: 2026-10-02T00:00:00Z}}
  /home/mark/.codex/packages/standalone/releases/v2: {.: {mtime: 2026-10-03T00:00:00Z}}
  /home/mark/.codex/packages/standalone/releases/v3: {.: {mtime: 2026-10-04T00:00:00Z}}
  /home/mark/.codex/packages/standalone/route: {symlink: releases/v0/child}
  /home/mark/.codex/packages/standalone/current: {symlink: route/../releases/v1}
  /home/mark/.local/bin/.keep: {size: 1K}
  /home/mark/bin/.keep: {size: 1K}
"#,
    ));
    let report = dry_run(
        &host,
        &built_in_request("profile:codex-releases", "/home/mark"),
    );
    assert_eq!(
        reason_of(&report, "/home/mark/.codex/packages/standalone/releases/v0"),
        Some(Some("linked_current")),
        "internal .. can cross a symlink before it is evaluated: {:#?}",
        report.plan
    );
    nothing_locked_or_deleted(&host);
}

#[test]
fn cursor_registry_missing_malformed_unsafe_or_unreadable_retains_versions() {
    let versions = r#"  /home/mark/.cursor/extensions/a.ext-1.0.0: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.cursor/extensions/a.ext-2.0.0: {.: {mtime: 2026-10-02T00:00:00Z}}
  /home/mark/.cursor/extensions/a.ext-3.0.0: {.: {mtime: 2026-10-03T00:00:00Z}}
  /home/mark/.cursor/extensions/a.ext-4.0.0: {.: {mtime: 2026-10-04T00:00:00Z}}
  /home/mark/.local/bin/.keep: {size: 1K}
  /home/mark/bin/.keep: {size: 1K}
"#;
    for (case, manifest, event) in [
        ("missing", "", ""),
        (
            "malformed",
            "  /home/mark/.cursor/extensions/extensions.json: {content: '{bad'}\n",
            "",
        ),
        (
            "unsafe relative path",
            "  /home/mark/.cursor/extensions/extensions.json: {content: '[{\"relativeLocation\":\"../a.ext-1.0.0\",\"location\":{\"path\":\"/home/mark/.cursor/extensions/a.ext-1.0.0\"}}]'}\n",
            "",
        ),
        (
            "unreadable",
            "  /home/mark/.cursor/extensions/extensions.json: {content: '[]'}\n",
            "events:\n  - {at: {op: read_head, nth: 1, when: before}, fail: EACCES}\n",
        ),
    ] {
        let host = host(&yaml(&format!("{versions}{manifest}{event}")));
        let report = dry_run(
            &host,
            &built_in_request("profile:cursor-extensions", "/home/mark"),
        );
        assert_eq!(
            reason_of(&report, "/home/mark/.cursor/extensions/a.ext-1.0.0"),
            Some(Some("installed_extension")),
            "{case}: {:#?}",
            report.plan
        );
        nothing_locked_or_deleted(&host);
    }
}

#[test]
fn a_running_version_binary_is_in_use_after_version_rules_pass() {
    let host = host(&yaml(
        r#"  /home/mark/.codex/packages/standalone/releases/v1: {.: {mtime: 2026-10-01T00:00:00Z}, codex: {size: 1K}}
  /home/mark/.codex/packages/standalone/releases/v2: {.: {mtime: 2026-10-02T00:00:00Z}, codex: {size: 1K}}
  /home/mark/.codex/packages/standalone/releases/v3: {.: {mtime: 2026-10-03T00:00:00Z}, codex: {size: 1K}}
  /home/mark/.codex/packages/standalone/releases/v4: {.: {mtime: 2026-10-04T00:00:00Z}, codex: {size: 1K}}
  /home/mark/.codex/packages/standalone/current: {symlink: releases/v1}
  /home/mark/.local/bin/.keep: {size: 1K}
  /home/mark/bin/.keep: {size: 1K}
processes:
  - {pid: 42, exe: /home/mark/.codex/packages/standalone/releases/v2/codex, cwd: /home/mark, maps: [/home/mark/.codex/packages/standalone/releases/v2/codex], fds: [{path: /home/mark/.codex/packages/standalone/releases/v2/codex}], environ: [PATH=/usr/bin]}
"#,
    ));
    let report = dry_run(
        &host,
        &built_in_request("profile:codex-releases", "/home/mark"),
    );
    assert_eq!(
        reason_of(&report, "/home/mark/.codex/packages/standalone/releases/v1"),
        Some(Some("linked_current"))
    );
    assert_eq!(
        reason_of(&report, "/home/mark/.codex/packages/standalone/releases/v2"),
        Some(Some("in_use"))
    );
    nothing_locked_or_deleted(&host);
}

/// Each shipped cache source says `type: dir`. A matching regular file is
/// outside its frozen set, including at a path that would otherwise match
/// every path segment. Changing one source to `type: any` must turn this red.
#[test]
fn shipped_home_cache_profiles_never_match_regular_files() {
    for (profile, root, path) in [
        (
            "pytest-cache",
            "/home/mark/src",
            "/home/mark/src/.pytest_cache",
        ),
        (
            "nix-user-cache",
            "/home/mark",
            "/home/mark/.cache/nix/gitv3",
        ),
        ("bun-cache", "/home/mark", "/home/mark/.bun/install/cache"),
        ("go-mod-cache", "/home/mark", "/home/mark/go/pkg/mod"),
    ] {
        let host = host(&yaml(&format!("  {path}: {{size: 1K}}\n")));
        let report = dry_run(
            &host,
            &built_in_request(&format!("profile:{profile}"), root),
        );
        assert!(plan(&report).is_empty(), "{profile}: {:#?}", report.plan);
        nothing_locked_or_deleted(&host);
    }
}

/// The Go cache really can contain read-only module directories. Derive the
/// delete option from the shipped profile and prove the model deletion uses
/// it, rather than merely asserting the YAML field's spelling.
#[test]
fn shipped_go_mod_cache_makes_read_only_modules_writable_for_deletion() {
    let host = host(&yaml(
        "  /home/mark/go/pkg/mod: {.: {mode: \"0555\"}, example: {.: {mode: \"0555\"}, file: {size: 1K}}}\n",
    ));
    let library = Library::builtin().expect("built-ins");
    let profile = library.get("go-mod-cache").expect("go-mod-cache");
    let Body::Source(source) = &profile.body else {
        panic!("go-mod-cache is not a source");
    };
    let Action::Delete { make_writable } = &source.action else {
        panic!("go-mod-cache does not delete");
    };
    let parent = dir(&host, "/home/mark/go/pkg");
    let target = Path::new("/home/mark/go/pkg/mod");
    let candidate = Candidate::new(&parent, target).expect("candidate");
    let record = walk(&host, &candidate, &Rules::new(&[1000]), None).expect("eligible");
    let outcome = delete_unchecked(
        &host,
        &candidate,
        &record,
        DeleteOptions {
            make_writable: *make_writable,
            ..DeleteOptions::default()
        },
    );
    assert_eq!(outcome.end, End::Complete);
    assert!(
        host.stat_path(target).is_none(),
        "module cache still exists"
    );
    let chmods: Vec<String> = host
        .trace()
        .iter()
        .filter(|call| call.op == Op::Fchmod)
        .map(|call| {
            call.path
                .as_ref()
                .expect("fchmod path")
                .display()
                .to_string()
        })
        .collect();
    assert_eq!(
        chmods,
        ["/home/mark/go/pkg/mod", "/home/mark/go/pkg/mod/example"]
    );
}

/// A Nix evaluator with an eval-cache SQLite file open protects only that
/// named cache entry. This models the generic in-use rule; the separate
/// container acceptance check must establish that real Nix keeps the fd
/// open while evaluating.
#[test]
fn nix_user_cache_keeps_an_eval_cache_with_an_open_sqlite_file() {
    let entries = "  /usr/bin/nix: {}\n  /home/mark/.cache/nix/eval-cache-v6: {.: {}, db.sqlite: {size: 1K}}\n  /home/mark/.cache/nix/tarball-cache: {.: {}, tar: {size: 1K}}\n";
    let evaluator = "processes:\n  - {pid: 42, exe: /usr/bin/nix, cwd: /home/mark, maps: [/usr/bin/nix], fds: [{path: /home/mark/.cache/nix/eval-cache-v6/db.sqlite}], environ: [PATH=/usr/bin]}\n";
    let busy = host(&yaml(&format!("{entries}{evaluator}")));
    let request = built_in_request("profile:nix-user-cache", "/home/mark");
    let report = dry_run(&busy, &request);
    assert_eq!(
        reason_of(&report, "/home/mark/.cache/nix/eval-cache-v6"),
        Some(Some("in_use"))
    );
    assert_eq!(
        reason_of(&report, "/home/mark/.cache/nix/tarball-cache"),
        Some(None)
    );
    nothing_locked_or_deleted(&busy);

    let idle = host(&yaml(entries));
    let report = dry_run(&idle, &request);
    assert_eq!(
        reason_of(&report, "/home/mark/.cache/nix/eval-cache-v6"),
        Some(None)
    );
}

fn dry_run(host: &ModelHost, request: &Request) -> Report {
    dry_run_logged(host, request).0
}

/// [`dry_run`] and its log lines.
fn dry_run_logged(host: &ModelHost, request: &Request) -> (Report, Vec<String>) {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::dry_run(host, request, &[uid], &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    (report, lines)
}

/// The plan as (candidate, reason) pairs, in order.
fn plan(report: &Report) -> Vec<(String, Option<&'static str>)> {
    report
        .plan
        .iter()
        .map(|p| (p.candidate.display().to_string(), p.reason))
        .collect()
}

fn reason_of(report: &Report, candidate: &str) -> Option<Option<&'static str>> {
    report
        .plan
        .iter()
        .find(|p| p.candidate == Path::new(candidate))
        .map(|p| p.reason)
}

#[test]
fn negated_profile_excludes_ancestor_even_when_its_own_where_fails() {
    let host = host(&yaml(
        "  /tmp/neg-parent: {.: {}, child: {.: {}, payload: {size: 1K}}}\n  /tmp/neg-free: {.: {}, payload: {size: 1K}}\n",
    ));
    let report = dry_run(
        &host,
        &request(
            "profile:tmp-entry && !profile:negated-descendant",
            &[],
            "/tmp",
        ),
    );
    assert_eq!(
        reason_of(&report, "/tmp/neg-parent"),
        Some(Some("excluded"))
    );
    assert_eq!(reason_of(&report, "/tmp/neg-free"), Some(None));
    nothing_locked_or_deleted(&host);
}

#[test]
fn negated_profile_incomplete_private_tmp_scan_excludes_the_tier() {
    let host = host(&yaml(
        "  /tmp/systemd-private-unit: {.: {}, payload: {size: 1K}}\n  /tmp/neg-free: {.: {}, payload: {size: 1K}}\n",
    ));
    let report = dry_run(
        &host,
        &request(
            "profile:tmp-entry && !profile:negated-private-tmp",
            &[],
            "/tmp",
        ),
    );
    assert!(report.plan.is_empty(), "{report:#?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.message.contains("PrivateTmp")),
        "{report:#?}"
    );
    nothing_locked_or_deleted(&host);
}

fn nothing_locked_or_deleted(host: &ModelHost) {
    for op in [
        Op::Flock,
        Op::Unlinkat,
        Op::Fchmod,
        Op::MayDelete,
        Op::InheritLock,
    ] {
        assert!(host.trace().iter().all(|c| c.op != op), "{op} in a dry run");
    }
}

#[test]
fn config_protect_overlaps_both_directions_and_ignores_its_where() {
    let policy = r#"version: 1
profiles:
  - name: all-top
    find: {roots: [/tmp], paths: ['*'], type: dir}
    action: delete
    examples: {match: [/tmp/guarded]}
  - name: child
    find: {roots: [/tmp], paths: [guarded/child], type: dir}
    action: delete
    examples: {match: [/tmp/guarded/child]}
  - name: guard
    find: {roots: [/tmp], paths: [guarded, parent/inside], type: dir}
    where: {has: absent-marker}
    action: delete
    examples: {match: [/tmp/guarded, /tmp/parent/inside]}
protect: [guard]
"#;
    let config = config::parse(Path::new("/tmp/config.yaml"), policy.as_bytes()).unwrap();
    let protections = executor::bind_protections(&config, 1000, false).unwrap();
    let model = yaml(
        "  /tmp/guarded: {child: {payload: {size: 1K}}}\n  /tmp/parent: {inside: {payload: {size: 1K}}}\n  /tmp/other: {payload: {size: 1K}}\n",
    );
    let host = host(&model);
    let mut top = Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("1G").unwrap(),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select("profile:all-top", &config.library, None, 1000).unwrap(),
    };
    top.tier.protections = protections.clone();
    let report = dry_run(&host, &top);
    assert_eq!(reason_of(&report, "/tmp/guarded"), Some(Some("protected")));
    assert_eq!(reason_of(&report, "/tmp/parent"), Some(Some("protected")));
    assert_ne!(reason_of(&report, "/tmp/other"), Some(Some("protected")));

    top.tier = Tier::select("profile:child", &config.library, None, 1000).unwrap();
    top.tier.protections = protections;
    let child_report = dry_run(&host, &top);
    assert_eq!(
        reason_of(&child_report, "/tmp/guarded/child"),
        Some(Some("protected"))
    );
    nothing_locked_or_deleted(&host);
}

#[test]
fn protect_scan_fails_closed_on_an_unsafe_bound_root() {
    let policy = r#"version: 1
profiles:
  - name: guard
    find: {roots: required, paths: ['*'], type: dir}
    action: delete
    examples: {root: /tmp/protected, match: [/tmp/protected/inside]}
protect: [{profile: guard, roots: [/tmp/guard-link]}]
"#;
    let config = config::parse(Path::new("/tmp/config.yaml"), policy.as_bytes()).unwrap();
    let model = yaml(
        "  /tmp/guard-link: {symlink: /tmp/protected}\n  /tmp/protected: {inside: {}}\n  /tmp/other: {payload: {size: 1K}}\n",
    );
    let host = host(&model);
    let mut request = request("profile:tmp-entry", &[], "/tmp");
    request.tier.protections = executor::bind_protections(&config, 1000, false).unwrap();
    let report = dry_run(&host, &request);
    assert_eq!(report.status, Status::Error);
    assert!(report.plan.is_empty());
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.message.contains("guard-link"))
    );
    nothing_locked_or_deleted(&host);
}

#[test]
fn a_local_mount_below_nfs_is_refused_before_any_root_or_ancestor_open() {
    let model = yaml("  /net/local/old: {payload: {size: 1K}}\n  /tmp/other: {payload: {size: 1K}}\n")
        .replacen(
            "tree:\n",
            "  - {id: 50, at: /net, fs: nfs4, dev: \"0:50\", size: 1T, parent: 1}\n  - {id: 51, at: /net/local, fs: tmpfs, dev: \"0:51\", size: 1G, parent: 50}\ntree:\n",
            1,
        );
    let root = Path::new("/net/local");

    let host = common::host(&model);
    let source = request("profile:anything", &["/net/local"], "/tmp");
    let report = dry_run(&host, &source);
    assert!(report.plan.is_empty(), "{report:#?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.message.contains("nfs4") && error.message.contains("/net")),
        "{report:#?}"
    );
    assert!(
        host.trace().iter().all(
            |call| !matches!(call.op, Op::Openat2 | Op::Statx | Op::ReadDir)
                || !call
                    .path
                    .as_deref()
                    .is_some_and(|path| path.starts_with("/net"))
        ),
        "{:#?}",
        host.trace()
    );

    let host = common::host(&model);
    let SourceKind::Fs(find) = &source.tier.source else {
        panic!("filesystem source")
    };
    let (found, stopped) = diskgc_core::fs_kind::discover(&host, &source.tier.roots, find, None);
    assert!(stopped.is_none(), "{stopped:?}");
    assert!(found.candidates.is_empty());
    assert!(
        found
            .refused
            .iter()
            .any(|skip| skip.path == root
                && skip.reason == diskgc_core::engine::Reason::UnsupportedFs),
        "{found:#?}"
    );
    assert!(
        host.trace().iter().all(
            |call| !matches!(call.op, Op::Openat2 | Op::Statx | Op::ReadDir)
                || !call
                    .path
                    .as_deref()
                    .is_some_and(|path| path.starts_with("/net"))
        ),
        "{:#?}",
        host.trace()
    );

    let policy = "version: 1\nprofiles:\n  - name: guard\n    find: {roots: required, paths: ['*'], type: dir}\n    action: delete\n    examples: {root: /tmp/protected, match: [/tmp/protected/inside]}\nprotect: [{profile: guard, roots: [/net/local]}]\n";
    let config = config::parse(Path::new("/tmp/config.yaml"), policy.as_bytes()).unwrap();
    let host = common::host(&model);
    let mut selected = request("profile:tmp-entry", &[], "/tmp");
    selected.tier.protections = executor::bind_protections(&config, 1000, false).unwrap();
    let report = dry_run(&host, &selected);
    assert!(report.plan.is_empty(), "{report:#?}");
    assert!(
        report.errors.iter().any(
            |error| error.message.contains("protect `guard`") && error.message.contains("nfs4")
        ),
        "{report:#?}"
    );
    assert!(
        host.trace().iter().all(
            |call| !matches!(call.op, Op::Openat2 | Op::Statx | Op::ReadDir)
                || !call
                    .path
                    .as_deref()
                    .is_some_and(|path| path.starts_with("/net"))
        ),
        "{:#?}",
        host.trace()
    );
}

#[test]
fn root_mode_refuses_uid_placeholder_in_protect_roots_and_patterns() {
    for (roots, pattern) in [
        ("['/tmp/owner-{uid}']", "['*']"),
        ("[/tmp/owners]", "['owner-{uid}']"),
    ] {
        let policy = format!(
            "version: 1\nprofiles:\n  - name: guard\n    find: {{roots: required, paths: {pattern}, type: dir}}\n    action: delete\n    examples: {{root: /tmp/owners, match: [/tmp/owners/owner-1000]}}\nprotect: [{{profile: guard, roots: {roots}}}]\n"
        );
        let config = config::parse(Path::new("/tmp/config.yaml"), policy.as_bytes()).unwrap();
        assert!(
            executor::bind_protections(&config, 0, true)
                .unwrap_err()
                .contains("--allow-root refuses `{uid}`")
        );
    }
}

#[test]
fn exact_exclusion_still_traverses_a_deeper_positive_pattern() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let host = host(&yaml(&format!(
        "  /tmp/excluded: {{.: {at}, item: {{.: {at}}}}}\n  /tmp/allowed: {{.: {at}, item: {{.: {at}}}}}\n"
    )));
    let report = dry_run(&host, &request("profile:excluded-prefix", &[], "/tmp"));
    let mut actual = plan(&report);
    actual.sort();
    assert_eq!(
        actual,
        [
            ("/tmp/allowed/item".to_owned(), None),
            ("/tmp/excluded/item".to_owned(), None),
        ]
    );
    nothing_locked_or_deleted(&host);
}

#[test]
fn exact_exclusion_never_reports_an_excluded_mount_candidate() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let model = yaml(&format!(
        "  /tmp/allowed: {{.: {at}, item: {{.: {at}}}}}\n"
    ))
    .replace(
        "parent: 1}\ntree:",
        "parent: 1}\n  - {id: 31, at: /tmp/excluded, fs: tmpfs, dev: \"0:43\", size: 1M, parent: 30}\ntree:",
    );
    let host = host(&model);
    let report = dry_run(&host, &request("profile:excluded-prefix", &[], "/tmp"));
    assert_eq!(
        plan(&report),
        [("/tmp/allowed/item".to_owned(), None)],
        "an exact exclusion must not appear even as a skipped mount candidate"
    );
    nothing_locked_or_deleted(&host);
}

/// AC #1: the patterns find exactly what they say, one level per
/// segment: top-level and one level down, directories only (`type: dir`:
/// a file, a symlink or a FIFO of a matching name is no candidate), not two
/// levels down, not the candidate's own insides, and not `/home`.
/// Eligible first, oldest newest-change first.
#[test]
fn pytest_of_dir_plans_exactly_what_its_patterns_find() {
    let host = host(&yaml(
        r#"  /tmp/pytest-of-mark:
    .: {mtime: 2026-10-10T00:00:00Z}
    pytest-1: {.: {mtime: 2026-10-10T00:00:00Z}, f: {size: 10K, mtime: 2026-10-10T00:00:00Z}}
  /tmp/nix-shell.Ab1:
    .: {mtime: 2026-10-01T00:00:00Z}
    pytest-of-mark: {.: {mtime: 2026-10-01T00:00:00Z}, g: {size: 20K, mtime: 2026-10-01T00:00:00Z}}
    x: {pytest-of-deep: {.: {}}}
  /tmp/tmp.Q1/pytest-of-ci: {.: {}}
  /tmp/pytest-of-file: {}
  /tmp/pytest-of-link: {symlink: /tmp/pytest-of-mark}
  /tmp/pytest-of-pipe: {type: fifo}
  /tmp/nix-shell-1-2/pytest-of-mark: {.: {}}
  /home/mark/pytest-of-mark: {.: {}}
"#,
    ));
    let report = dry_run(&host, &request("profile:pytest-of-dir", &[], "/tmp"));
    assert_eq!(report.status, Status::DryRun, "{report:#?}");
    assert_eq!(report.exit_code(), 0);
    let mut found = plan(&report);
    // The eligible order is oldest first; /var/tmp is missing (a skip).
    assert_eq!(
        found.remove(found.len() - 1),
        ("/var/tmp".to_owned(), Some("missing"))
    );
    assert_eq!(
        found,
        [
            ("/tmp/nix-shell.Ab1/pytest-of-mark".to_owned(), None),
            ("/tmp/pytest-of-mark".to_owned(), None),
            ("/tmp/tmp.Q1/pytest-of-ci".to_owned(), None),
        ]
    );
    let sized = report.plan[1].estimated_bytes.expect("eligible: sized");
    assert!(sized >= 10 << 10, "{sized}");
    assert_eq!(report.plan[1].operation, "delete");
    nothing_locked_or_deleted(&host);
}

/// Every refusal of the eligibility walk and the in-use check reaches the
/// plan with its reason; a mount point is never statted; a keep marker in
/// a directory between the root and the candidate counts.
#[test]
fn every_check_of_a_candidate_reaches_the_plan_with_its_reason() {
    let mut extra = String::from(
        r#"  /tmp/pytest-of-ok: {.: {mtime: 2026-10-01T00:00:00Z}}
  /tmp/pytest-of-other: {.: {uid: 1001, mtime: 2026-10-01T00:00:00Z}}
  /tmp/pytest-of-inner-other: {.: {mtime: 2026-10-01T00:00:00Z}, f: {uid: 1001, mtime: 2026-10-01T00:00:00Z}}
  /tmp/pytest-of-kept: {.: {mtime: 2026-10-01T00:00:00Z}, .diskgc-keep: {mtime: 2026-10-01T00:00:00Z}}
  /tmp/tmp.K: {.: {mtime: 2026-10-01T00:00:00Z}, .diskgc-keep: {mtime: 2026-10-01T00:00:00Z}, pytest-of-x: {.: {mtime: 2026-10-01T00:00:00Z}}}
  /tmp/pytest-of-sock: {.: {mtime: 2026-10-01T00:00:00Z}, s: {type: socket, mtime: 2026-10-01T00:00:00Z}}
  /tmp/pytest-of-used: {.: {mtime: 2026-10-01T00:00:00Z}, sub: {.: {mtime: 2026-10-01T00:00:00Z}}}
  /tmp/pytest-of-env: {.: {mtime: 2026-10-01T00:00:00Z}}
  /tmp/pytest-of-new: {.: {mtime: 2026-10-14T23:00:00Z}}
  /tmp/pytest-of-future: {.: {mtime: 2027-01-01T00:00:00Z}}
processes:
"#,
    );
    extra.push_str(&process(7, "/tmp/pytest-of-used/sub", ""));
    extra.push_str(&process(8, "/", ", TMPDIR=/tmp/pytest-of-env/t"));
    let model = yaml(&extra).replace(
        "parent: 1}\ntree:",
        "parent: 1}\n  - {id: 31, at: /tmp/pytest-of-mnt, fs: tmpfs, dev: \"0:43\", size: 1M, \
         parent: 30}\ntree:",
    );
    let host = host(&model);
    let report = dry_run(
        &host,
        &request("profile:pytest-of-dir && filter:idle-3d", &[], "/tmp"),
    );
    for (candidate, want) in [
        ("/tmp/pytest-of-ok", None),
        ("/tmp/pytest-of-other", Some("not_owner")),
        ("/tmp/pytest-of-inner-other", Some("not_owner")),
        ("/tmp/pytest-of-kept", Some("kept")),
        ("/tmp/tmp.K/pytest-of-x", Some("kept")),
        ("/tmp/pytest-of-sock", Some("contains_ipc")),
        ("/tmp/pytest-of-used", Some("in_use")),
        ("/tmp/pytest-of-env", Some("in_use")),
        ("/tmp/pytest-of-new", Some("not_idle")),
        ("/tmp/pytest-of-future", Some("future_time")),
        ("/tmp/pytest-of-mnt", Some("contains_mount")),
    ] {
        assert_eq!(
            reason_of(&report, candidate),
            Some(want),
            "{candidate}: {:#?}",
            report.plan
        );
    }
    assert!(
        host.trace().iter().all(|c| c.op != Op::Statx
            || !c
                .path
                .as_deref()
                .is_some_and(|p| p.starts_with("/tmp/pytest-of-mnt"))),
        "a mount point is never statted"
    );
    nothing_locked_or_deleted(&host);
}

/// `type: any` takes directories and regular files, never a symlink, a
/// socket or a FIFO; `systemd-private-*` is never a candidate (and never
/// entered); a root on another filesystem than PATH's is refused before its
/// candidates are enumerated.
#[test]
fn tmp_entry_takes_files_and_dirs_but_never_private_tmp_or_specials() {
    let host = host(&yaml(
        r#"  /tmp/f: {mtime: 2026-10-01T00:00:00Z}
  /tmp/d: {.: {mtime: 2026-10-01T00:00:00Z}}
  /tmp/l: {symlink: /tmp/f}
  /tmp/s: {type: socket}
  /tmp/p: {type: fifo}
  /tmp/systemd-private-abc-cups.service-x1: {.: {uid: 0, mode: "0700"}, tmp: {.: {uid: 0}}}
"#,
    ));
    let report = dry_run(&host, &request("profile:tmp-entry", &[], "/tmp"));
    assert_eq!(
        plan(&report),
        [
            ("/tmp/d".to_owned(), None),
            ("/tmp/f".to_owned(), None),
            // /usr/bin/tool's parent dirs are not in /tmp: nothing else.
        ]
    );
    assert!(
        host.trace().iter().all(|c| !c
            .path
            .as_deref()
            .is_some_and(|p| p.starts_with("/tmp/systemd-private-abc-cups.service-x1"))),
        "PrivateTmp is never touched"
    );
    // A /tmp root for a /home request cannot free space on /home, so it is
    // reported as skipped before /tmp candidates are inspected.
    let host = common::host(&yaml("  /tmp/f: {}\n  /home/mark/x: {}\n"));
    let report = dry_run(&host, &request("profile:tmp-entry", &[], "/home/mark"));
    assert_eq!(plan(&report), [("/tmp".to_owned(), Some("other_fs"))]);
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert!(
        host.trace()
            .iter()
            .all(|call| call.path.as_deref() != Some(Path::new("/tmp/f"))),
        "foreign-root candidate was inspected"
    );
}

/// `roots: required` with `--roots`: the depths are spelled out (three
/// levels here, a fourth is not found), and overlapping roots list a
/// candidate once, under the outermost root.
#[test]
fn roots_required_profiles_look_where_roots_say_and_list_a_candidate_once() {
    let host = host(&yaml(
        r#"  /home/mark/src/app/.pytest_cache: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/src/a/b/.pytest_cache: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/src/a/b/c/.pytest_cache: {.: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/.pytest_cache: {.: {mtime: 2026-10-01T00:00:00Z}}
"#,
    ));
    let report = dry_run(
        &host,
        &request(
            "profile:pytest-cache-fixture",
            &["/home/mark/src", "/home/mark"],
            "/home/mark",
        ),
    );
    let mut found: Vec<String> = plan(&report).into_iter().map(|(c, _)| c).collect();
    found.sort();
    assert_eq!(
        found,
        [
            "/home/mark/.pytest_cache",
            "/home/mark/src/a/b/.pytest_cache",
            "/home/mark/src/app/.pytest_cache",
        ],
        "{:#?}",
        report.plan
    );
    assert!(report.plan.iter().all(|p| p.reason.is_none()));
}

/// A source's own `where` decides what is a candidate at all: a directory
/// holding the `has` file as a regular file is one (and is not entered: the
/// nested build dir is part of it), one without it is not, nor one where
/// the name is a directory; one also holding the `has_not` file is not
/// either (an in-source CMake build: a source tree, task-68's finding).
#[test]
fn a_sources_own_has_narrows_what_is_a_candidate() {
    let host = host(&yaml(
        r#"  /home/mark/src/build:
    .: {mtime: 2026-10-01T00:00:00Z}
    CMakeCache.txt: {mtime: 2026-10-01T00:00:00Z}
    sub: {.: {mtime: 2026-10-01T00:00:00Z}, CMakeCache.txt: {mtime: 2026-10-01T00:00:00Z}}
  /home/mark/src/plain/f: {}
  /home/mark/src/odd/CMakeCache.txt: {.: {}}
  /home/mark/src/insource: {CMakeCache.txt: {}, CMakeLists.txt: {}, main.c: {}}
"#,
    ));
    let report = dry_run(
        &host,
        &request(
            "profile:marker-build-fixture",
            &["/home/mark/src"],
            "/home/mark",
        ),
    );
    assert_eq!(
        plan(&report),
        [("/home/mark/src/build".to_owned(), None)],
        "{:#?}",
        report.plan
    );
}

/// `has_not` keeps its v1 meaning: a symlink with the marker name is not
/// a regular file. `lacks_entry` is the stricter guard used by anchored builds.
#[test]
fn has_not_still_allows_a_nonregular_entry() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let host = host(&yaml(&format!(
        "  /tmp/pytest-of-link: {{.: {at}, mounted: {{symlink: /tmp/absent, mtime: \
         2026-10-01T00:00:00Z}}}}\n  /tmp/pytest-of-file: {{.: {at}, mounted: {at}}}\n"
    )));
    let report = dry_run(&host, &request("profile:unmounted", &[], "/tmp"));
    assert_eq!(
        plan(&report),
        [("/tmp/pytest-of-link".to_owned(), None)],
        "{:#?}",
        report.plan
    );
    nothing_locked_or_deleted(&host);
}

/// An arbitrary output name is safe only when its CMake cache names an
/// existing source ancestor outside the output. Marker-only, missing-source,
/// in-source, recent and checkout cases cannot be eligible.
#[test]
fn anchored_cmake_needs_a_surviving_source_and_idle_output() {
    let old = "{mtime: 2026-10-01T00:00:00Z}";
    let host = host(&yaml(&format!(
        "  /home/mark/src/app/CMakeLists.txt: {old}\n  \
         /home/mark/src/app/arbitrary-name: {{.: {old}, CMakeCache.txt: {{content: \
         \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/src/app\\n\", mtime: \
         2026-10-01T00:00:00Z}}, CMakeFiles: {{.: {old}, rule: {old}}}, obj: {old}}}\n  \
         /home/mark/src/missing/output: {{.: {old}, CMakeCache.txt: {{content: \
         \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/src/missing\\n\", mtime: \
         2026-10-01T00:00:00Z}}, CMakeFiles: {{.: {old}}}}}\n  \
         /home/mark/src/app/fake-output: {{.: {old}, CMakeCache.txt: {{content: \
         \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/src/other\\n\", mtime: \
         2026-10-01T00:00:00Z}}, CMakeFiles: {{.: {old}}}}}\n  \
         /home/mark/src/recent/CMakeLists.txt: {old}\n  \
         /home/mark/src/recent/output: {{.: {{mtime: 2026-10-15T02:00:00Z}}, \
         CMakeCache.txt: {{content: \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/src/recent\\n\", \
         mtime: 2026-10-01T00:00:00Z}}, CMakeFiles: {{.: {old}}}}}\n  \
         /home/mark/src/checkout/CMakeLists.txt: {old}\n  \
         /home/mark/src/checkout/output: {{.: {old}, CMakeCache.txt: {{content: \
         \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/src/checkout\\n\", mtime: \
         2026-10-01T00:00:00Z}}, CMakeFiles: {{.: {old}}}, .git: {{.: {old}, HEAD: {old}}}}}\n  \
         /home/mark/src/insource: {{.: {old}, CMakeLists.txt: {old}, CMakeCache.txt: {{content: \
         \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/src/insource\\n\", mtime: \
         2026-10-01T00:00:00Z}}, CMakeFiles: {{.: {old}}}}}\n"
    )));
    let report = dry_run(
        &host,
        &built_in_request("profile:cmake-build-anchored", "/home/mark/src"),
    );
    let mut actual = plan(&report);
    actual.sort();
    assert_eq!(
        actual,
        [
            ("/home/mark/src/app/arbitrary-name".to_owned(), None),
            (
                "/home/mark/src/checkout/output".to_owned(),
                Some("contains_checkout")
            ),
            ("/home/mark/src/recent/output".to_owned(), Some("not_idle")),
        ],
        "{:#?}",
        report.plan
    );
    let evidence = report
        .plan
        .iter()
        .find(|entry| entry.reason.is_none())
        .unwrap();
    assert_eq!(
        evidence.build_evidence.as_ref().unwrap().marker,
        "CMakeCache.txt"
    );
    assert_eq!(
        evidence.build_evidence.as_ref().unwrap().source,
        PathBuf::from("/home/mark/src/app")
    );
    assert!(
        evidence
            .build_evidence
            .as_ref()
            .unwrap()
            .idle_age_seconds
            .unwrap()
            >= 3 * 24 * 3600
    );
    for skipped in report.plan.iter().filter(|entry| entry.reason.is_some()) {
        let evidence = skipped.build_evidence.as_ref().unwrap();
        assert_eq!(evidence.marker, "CMakeCache.txt");
        assert_eq!(evidence.idle_age_seconds, None);
    }
    nothing_locked_or_deleted(&host);
}

/// The West profile retains the Zephyr marker check, but an output is only
/// eligible when its recorded CMake source still exists under the bound root.
/// Its depth-five match set and checkout guard remain explicit.
#[test]
fn anchored_west_build_requires_source_and_zephyr_directory() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let root = "/home/mark/topics";
    let deep = "/home/mark/topics/a/b/c/d/build";
    let host = host(&yaml(&format!(
        "  /home/mark/topics/app/CMakeLists.txt: {at}\n  \
         /home/mark/topics/app/build: {{.: {at}, CMakeCache.txt: {{content: \
         \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/topics/app\\n\", mtime: 2026-10-01T00:00:00Z}}, \
         zephyr: {{.: {at}, zephyr.elf: {at}}}}}\n  \
         /home/mark/topics/missing/build: {{.: {at}, CMakeCache.txt: {{content: \
         \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/topics/missing\\n\"}}, zephyr: {{.: {at}}}}}\n  \
         /home/mark/elsewhere/CMakeLists.txt: {at}\n  \
         /home/mark/topics/outside/build: {{.: {at}, CMakeCache.txt: {{content: \
         \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/elsewhere\\n\"}}, zephyr: {{.: {at}}}}}\n  \
         /home/mark/topics/plain/build: {{.: {at}, CMakeCache.txt: {at}}}\n  \
         /home/mark/topics/file-marker/build: {{.: {at}, CMakeCache.txt: {at}, zephyr: {at}}}\n  \
         /home/mark/topics/link-marker/build: {{.: {at}, CMakeCache.txt: {at}, zephyr: {{symlink: /home/mark/topics/app/build/zephyr, mtime: 2026-10-01T00:00:00Z}}}}\n  \
         /home/mark/topics/insource/build: {{.: {at}, CMakeCache.txt: {at}, zephyr: {{.: {at}}}, CMakeLists.txt: {at}}}\n  \
         /home/mark/topics/not-a-file/build: {{.: {at}, CMakeCache.txt: {{.: {at}}}, zephyr: {{.: {at}}}}}\n  \
         /home/mark/topics/checkout/CMakeLists.txt: {at}\n  \
         /home/mark/topics/checkout/build: {{.: {at}, CMakeCache.txt: {{content: \
         \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/topics/checkout\\n\", mtime: 2026-10-01T00:00:00Z}}, \
         zephyr: {{.: {at}}}, .git: {{.: {at}, HEAD: {at}}}}}\n  \
         /home/mark/topics/a/b/c/d/CMakeLists.txt: {at}\n  \
         {deep}: {{.: {at}, CMakeCache.txt: {{content: \
         \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/topics/a/b/c/d\\n\", mtime: 2026-10-01T00:00:00Z}}, zephyr: {{.: {at}}}}}\n  \
         {deep}/deeper/build: {{.: {at}, CMakeCache.txt: {at}, zephyr: {{.: {at}}}}}\n"
    )));
    // Creating a nested build changed the outer build directory's mtime.
    // The depth-five candidate is still found, then refused by the idle floor.
    let report = dry_run(
        &host,
        &built_in_request("profile:west-build-anchored", root),
    );
    let mut actual = plan(&report);
    actual.sort();
    assert_eq!(
        actual,
        [
            (deep.to_owned(), Some("not_idle")),
            (format!("{root}/app/build"), None),
            (format!("{root}/checkout/build"), Some("contains_checkout")),
        ],
        "{:#?}",
        report.plan
    );
    assert!(
        report
            .plan
            .iter()
            .filter(|entry| entry.reason.is_none())
            .all(|entry| {
                let evidence = entry.build_evidence.as_ref().expect("source evidence");
                evidence.marker == "CMakeCache.txt"
                    && evidence.source.starts_with(root)
                    && evidence
                        .idle_age_seconds
                        .is_some_and(|age| age >= 3 * 24 * 3600)
            })
    );
    nothing_locked_or_deleted(&host);
}

/// Python bytecode and mypy caches are disposable independently of project
/// recency. A tagged mypy cache is recognized; an untagged namesake is not.
/// The checkout guard still protects a cache containing an embedded repo.
#[test]
fn shipped_project_python_caches_find_only_cache_directories() {
    let root = "/home/mark/src";
    let host = host(&yaml(
        r#"  /home/mark/src/app/__pycache__:
    .: {mtime: 2026-10-01T00:00:00Z}
    main.cpython-313.pyc: {size: 1K}
    nested: {__pycache__: {.: {}, inner.pyc: {size: 1K}}}
  /home/mark/src/app/pkg/__pycache__: {.: {}, module.cpython-313.pyc: {size: 1K}}
  /home/mark/src/app/repo/__pycache__: {.: {}, .git: {.: {}, HEAD: {}}}
  /home/mark/src/app/.mypy_cache: {.: {}, CACHEDIR.TAG: {}, "3.13": {.: {}, module.meta.json: {size: 1K}}}
  /home/mark/src/app/untagged/.mypy_cache: {.: {}, cache.json: {size: 1K}}
  /home/mark/src/app/tag-dir/.mypy_cache: {.: {}, CACHEDIR.TAG: {.: {}}}
  /home/mark/src/app/tag-link/.mypy_cache: {.: {}, CACHEDIR.TAG: {symlink: /tmp/absent}}
  /home/mark/src/app/.mypy_cache-backup: {.: {}, CACHEDIR.TAG: {}}
"#,
    ));
    let pycache = dry_run(&host, &built_in_request("profile:python-pycache", root));
    let mut actual = plan(&pycache);
    actual.sort();
    assert_eq!(
        actual,
        [
            ("/home/mark/src/app/__pycache__".to_owned(), None),
            ("/home/mark/src/app/pkg/__pycache__".to_owned(), None),
            (
                "/home/mark/src/app/repo/__pycache__".to_owned(),
                Some("contains_checkout")
            ),
        ],
        "{:#?}",
        pycache.plan
    );
    let mypy = dry_run(&host, &built_in_request("profile:mypy-cache", root));
    assert_eq!(
        plan(&mypy),
        [("/home/mark/src/app/.mypy_cache".to_owned(), None)],
        "{:#?}",
        mypy.plan
    );
    nothing_locked_or_deleted(&host);
}

/// The compiled term retains entry and idle filters. Unsupported forms are
/// refused rather than silently ignored.
#[test]
fn positive_has_filter_and_filesystem_union_are_compiled() {
    let library = library();
    let expr = "profile:tmp-entry && filter:has-tag";
    let tier = Tier::select(expr, &library, None, 1000).expect(expr);
    let SourceKind::Fs(find) = tier.source else {
        panic!("entry filter requires fs source");
    };
    assert_eq!(find.entry_filters.len(), 1);
    assert_eq!(find.entry_filters[0].entry, "CACHEDIR.TAG");
    assert!(find.entry_filters[0].has);
    assert!(!find.entry_filters[0].negated);
    let union = Tier::select(
        "profile:tmp-entry || profile:pytest-of-dir",
        &library,
        None,
        1000,
    )
    .expect("filesystem delete union is runnable");
    assert_eq!(union.fs_union.len(), 1);
    let tier = Tier::select("profile:tmp-entry && !filter:idle-1d", &library, None, 1000)
        .expect("an idle complement is runnable");
    assert_eq!(
        tier.idle_before.map(|idle| idle.0),
        Some(Duration::from_secs(86_400))
    );
}

#[test]
fn filesystem_union_dedupes_hardlink_aliases_by_inode_identity() {
    let host = host(&yaml(
        "  /tmp/link-a: {size: 1M}\n  /tmp/link-b: {link: /tmp/link-a}\n",
    ));
    let plan = dry_run(
        &host,
        &request("profile:union-file-a || profile:union-file-b", &[], "/tmp"),
    );
    let aliases: Vec<_> = plan
        .plan
        .iter()
        .filter(|entry| {
            entry.candidate == Path::new("/tmp/link-a")
                || entry.candidate == Path::new("/tmp/link-b")
        })
        .collect();
    assert_eq!(aliases.len(), 1, "{plan:#?}");
    assert_eq!(aliases[0].candidate, Path::new("/tmp/link-a"));
}

#[test]
fn entry_filters_have_known_and_unknown_verdicts() {
    let tree = "  /tmp/pytest-of-tagged: {.: {}, CACHEDIR.TAG: {size: 1K}}\n  /tmp/pytest-of-plain: {.: {}, f: {size: 1K}}\n";
    for (selection, eligible, filtered) in [
        (
            "profile:pytest-of-dir && !filter:has-tag",
            "/tmp/pytest-of-plain",
            "/tmp/pytest-of-tagged",
        ),
        (
            "profile:pytest-of-dir && !filter:has-not-tag",
            "/tmp/pytest-of-tagged",
            "/tmp/pytest-of-plain",
        ),
        (
            "profile:pytest-of-dir && filter:has-tag",
            "/tmp/pytest-of-tagged",
            "/tmp/pytest-of-plain",
        ),
        (
            "profile:pytest-of-dir && filter:has-not-tag",
            "/tmp/pytest-of-plain",
            "/tmp/pytest-of-tagged",
        ),
    ] {
        let host = host(&yaml(tree));
        let report = dry_run(&host, &request(selection, &[], "/tmp"));
        assert!(
            report
                .plan
                .iter()
                .any(|p| p.candidate == Path::new(eligible) && p.reason.is_none()),
            "{report:#?}"
        );
        assert!(
            report
                .plan
                .iter()
                .any(|p| p.candidate == Path::new(filtered) && p.reason == Some("filtered")),
            "{report:#?}"
        );
        nothing_locked_or_deleted(&host);
    }

    let unknown = "events:\n  - {at: {op: statx, path: /tmp/pytest-of-plain/CACHEDIR.TAG, when: before}, fail: EACCES}\n";
    for selection in [
        "profile:pytest-of-dir && !filter:has-tag",
        "profile:pytest-of-dir && filter:has-tag",
    ] {
        let host = host(&yaml(&format!("{tree}{unknown}")));
        let report = dry_run(&host, &request(selection, &[], "/tmp"));
        assert!(
            report
                .plan
                .iter()
                .any(|p| p.candidate == Path::new("/tmp/pytest-of-plain")
                    && p.reason == Some("unreadable")),
            "{report:#?}"
        );
        assert!(host.unfired_events().is_empty());
        nothing_locked_or_deleted(&host);
    }
}

#[test]
fn positive_mixed_entry_and_idle_filter_requires_every_predicate() {
    let selection = "profile:pytest-of-dir && filter:tagged-idle-unkept";
    let tier = Tier::select(selection, &library(), None, 1000).expect(selection);
    assert_eq!(tier.idle_for.unwrap().0, Duration::from_secs(2 * 86_400));
    let SourceKind::Fs(find) = tier.source else {
        panic!("filesystem source");
    };
    assert_eq!(find.entry_filters.len(), 2);

    let tree = "  /tmp/pytest-of-good: {.: {mtime: 2026-10-01T00:00:00Z}, CACHEDIR.TAG: {size: 1K, mtime: 2026-10-01T00:00:00Z}}\n  /tmp/pytest-of-new: {.: {mtime: 2026-10-14T00:00:00Z}, CACHEDIR.TAG: {size: 1K, mtime: 2026-10-14T00:00:00Z}}\n  /tmp/pytest-of-untagged: {.: {mtime: 2026-10-01T00:00:00Z}, f: {size: 1K, mtime: 2026-10-01T00:00:00Z}}\n  /tmp/pytest-of-kept: {.: {mtime: 2026-10-01T00:00:00Z}, CACHEDIR.TAG: {size: 1K, mtime: 2026-10-01T00:00:00Z}, .keep: {size: 1K, mtime: 2026-10-01T00:00:00Z}}\n";
    let host = host(&yaml(tree));
    let report = dry_run(&host, &request(selection, &[], "/tmp"));
    for (name, reason) in [
        ("good", None),
        ("new", Some("not_idle")),
        ("untagged", Some("filtered")),
        ("kept", Some("filtered")),
    ] {
        let path = format!("/tmp/pytest-of-{name}");
        assert!(
            report
                .plan
                .iter()
                .any(|entry| entry.candidate == Path::new(&path) && entry.reason == reason),
            "{path}: {report:#?}"
        );
    }
    nothing_locked_or_deleted(&host);

    let negated = "profile:pytest-of-dir && !filter:tagged-idle-unkept";
    let tier = Tier::select(negated, &library(), None, 1000).expect(negated);
    let SourceKind::Fs(find) = tier.source else {
        panic!("filesystem source");
    };
    assert_eq!(find.negated_mixed_filters.len(), 1);
    let negated_host = common::host(&yaml(tree));
    let report = dry_run(&negated_host, &request(negated, &[], "/tmp"));
    for (name, reason) in [
        ("good", Some("filtered")),
        ("new", None),
        ("untagged", None),
        ("kept", None),
    ] {
        let path = format!("/tmp/pytest-of-{name}");
        assert!(
            report
                .plan
                .iter()
                .any(|entry| entry.candidate == Path::new(&path) && entry.reason == reason),
            "{path}: {report:#?}"
        );
    }
    nothing_locked_or_deleted(&negated_host);
}

#[test]
fn unknown_marker_type_passes_neither_source_has_not_nor_term_has_not() {
    let tree = "  /tmp/pytest-of-a: {.: {}, mounted: {size: 1K}, CACHEDIR.TAG: {size: 1K}}\n";
    for (selection, marker) in [
        ("profile:unmounted", "/tmp/pytest-of-a/mounted"),
        (
            "profile:pytest-of-dir && filter:has-not-tag",
            "/tmp/pytest-of-a/CACHEDIR.TAG",
        ),
        (
            "profile:pytest-of-dir && !filter:tagged-idle-unkept",
            "/tmp/pytest-of-a/CACHEDIR.TAG",
        ),
    ] {
        let host = host(&yaml(tree));
        host.omit_statx_type_at(PathBuf::from(marker));
        let report = dry_run(&host, &request(selection, &[], "/tmp"));
        assert!(
            report.plan.iter().any(|entry| {
                entry.candidate
                    == Path::new(if selection == "profile:unmounted" {
                        marker
                    } else {
                        "/tmp/pytest-of-a"
                    })
                    && entry.reason == Some("unsupported_fs")
            }),
            "{selection}: {report:#?}"
        );
        nothing_locked_or_deleted(&host);
    }
}

#[test]
fn negated_mixed_filter_unknown_second_entry_type_is_not_a_complement_match() {
    let tree =
        "  /tmp/pytest-of-a: {.: {}, CACHEDIR.TAG: {size: 1K}, .keep: {size: 1K}, f: {size: 1M}}\n";
    let host = host(&yaml(tree));
    host.omit_statx_type_at(PathBuf::from("/tmp/pytest-of-a/.keep"));
    let selection = "profile:pytest-of-dir && !filter:tagged-idle-unkept";
    let report = dry_run(&host, &request(selection, &[], "/tmp"));
    assert!(
        report
            .plan
            .iter()
            .any(|entry| entry.candidate == Path::new("/tmp/pytest-of-a")
                && entry.reason == Some("unsupported_fs")),
        "{report:#?}"
    );
    nothing_locked_or_deleted(&host);
}

/// A dry run reuses one process scan for later candidates for up to
/// the plan checker's one-minute limit (it deletes nothing: the deletion's freshness
/// bound does not apply): with 5 s passing at each of three candidates'
/// walks, one scan serves all three; past the reuse time a new one is made.
#[test]
fn a_dry_run_reuses_a_process_scan_across_candidates() {
    let scans_with = |advance: &str| {
        let host = host(&yaml(&format!(
            "  /tmp/pytest-of-a: {{.: {{mtime: 2026-10-01T00:00:00Z}}}}\n  /tmp/pytest-of-b: {{.: \
             {{mtime: 2026-10-01T00:00:00Z}}}}\n  /tmp/pytest-of-c: {{.: {{mtime: \
             2026-10-01T00:00:00Z}}}}\nprocesses:\n{}events:\n  - {{at: {{op: read_dir, path: \
             /tmp/pytest-of-b, when: before}}, advance: {advance}}}\n  - {{at: {{op: read_dir, \
             path: /tmp/pytest-of-c, when: before}}, advance: {advance}}}\n",
            process(7, "/", "")
        )));
        let report = dry_run(&host, &request("profile:pytest-of-dir", &[], "/tmp"));
        assert_eq!(
            report.plan.iter().filter(|p| p.reason.is_none()).count(),
            3,
            "{:#?}",
            report.plan
        );
        assert!(host.unfired_events().is_empty(), "the clock moved");
        host.trace().iter().filter(|c| c.op == Op::Pids).count()
    };
    let one = scans_with("5s");
    let more = scans_with("61s");
    assert!(one >= 1, "a scan was made");
    assert!(
        more > one,
        "past the reuse time a new scan is made ({one} vs {more})"
    );
    assert_eq!(scans_with("1s"), one);
}

/// The first scan is unsettled and its bounded retry crosses the deadline.
/// Even a settled retry cannot turn this candidate into a late plan entry.
#[test]
fn a_retry_that_crosses_the_deadline_leaves_the_candidate_unjudged() {
    let forks: String = (1..=MAX_LISTINGS)
        .map(|listing| {
            format!(
                "  - {{at: {{op: pids, nth: {listing}, when: after}}, fork: {{pid: 7, child: {}}}}}\n",
                100 + listing
            )
        })
        .collect();
    let host = host(&yaml(&format!(
        "  /tmp/pytest-of-a: {{.: {{mtime: 2026-10-01T00:00:00Z}}}}\nprocesses:\n{}events:\n{forks}  - {{at: {{op: pids, nth: {}, when: after}}, advance: 700s}}\n",
        process(7, "/", ""),
        MAX_LISTINGS + 1
    )));
    let report = dry_run(&host, &request("profile:pytest-of-dir", &[], "/tmp"));
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert_eq!(report.exit_code(), 124);
    assert_eq!(
        reason_of(&report, "/tmp/pytest-of-a"),
        None,
        "the candidate is unjudged: {report:#?}"
    );
    assert!(
        host.unfired_events().is_empty(),
        "the retry crossed the deadline"
    );
    nothing_locked_or_deleted(&host);
}

/// A scan that blocks every candidate (here: an unreadable process of the
/// user) is not reused for a minute: once it is older than the deletion's
/// reuse limit, the next candidate gets a new scan. `a` is judged while
/// pid 9 cannot be read; pid 9 exits and 5 s pass before `b` is walked.
#[test]
fn a_blocking_scan_is_not_reused_past_the_reuse_limit() {
    let host = host(&yaml(&format!(
        "  /tmp/pytest-of-a: {{.: {{mtime: 2026-10-01T00:00:00Z}}}}\n  /tmp/pytest-of-b: {{.: \
         {{mtime: 2026-10-01T00:00:00Z}}}}\nprocesses:\n{}  - {{pid: 9, exe: /usr/bin/tool, \
         proc: failed}}\nevents:\n  - {{at: {{op: read_dir, path: /tmp/pytest-of-b, when: \
         before}}, exit: 9}}\n  - {{at: {{op: read_dir, path: /tmp/pytest-of-b, when: before}}, \
         advance: 5s}}\n",
        process(7, "/", "")
    )));
    let report = dry_run(&host, &request("profile:pytest-of-dir", &[], "/tmp"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(reason_of(&report, "/tmp/pytest-of-a"), Some(Some("in_use")));
    assert_eq!(
        reason_of(&report, "/tmp/pytest-of-b"),
        Some(None),
        "{:#?}",
        report.plan
    );
}

/// F2 (task-68 review): mounts come first, also before the source's own
/// `where`. `has_not: mounted` would `statx` a name inside the candidate;
/// on a dead FUSE mount there that hangs. The candidate is
/// `contains_mount` and nothing at or below the mount is touched.
#[test]
fn a_mount_inside_is_refused_before_the_sources_where_looks_inside() {
    let model = yaml("  /tmp/pytest-of-m: {.: {mtime: 2026-10-01T00:00:00Z}}\n").replace(
        "parent: 1}\ntree:",
        "parent: 1}\n  - {id: 31, at: /tmp/pytest-of-m/mounted, fs: fuse.sshfs, dev: \"0:44\", \
         size: 1M, parent: 30}\ntree:",
    );
    let host = host(&model);
    let report = dry_run(&host, &request("profile:unmounted", &[], "/tmp"));
    assert_eq!(
        plan(&report),
        [("/tmp/pytest-of-m".to_owned(), Some("contains_mount"))],
        "{:#?}",
        report.plan
    );
    let touched: Vec<_> = host
        .trace()
        .into_iter()
        .filter(|c| {
            c.path
                .as_deref()
                .is_some_and(|p| p.starts_with("/tmp/pytest-of-m/mounted"))
        })
        .collect();
    assert!(touched.is_empty(), "{touched:#?}");
}

/// F3 (task-68 review): a root inside systemd's PrivateTmp is never
/// searched: checked on the root's own names, before it is opened, and
/// reported as a misconfigured root.
#[test]
fn a_root_inside_private_tmp_is_never_searched() {
    let private = "/tmp/systemd-private-abc-cups.service-x1/tmp";
    let host = host(&yaml(&format!("  {private}/f: {{}}\n  /tmp/g: {{}}\n")));
    let report = dry_run(&host, &request("profile:anything", &[private], "/tmp"));
    assert_eq!(plan(&report), [], "{:#?}", report.plan);
    assert_eq!(report.status, Status::Error);
    assert_eq!(report.exit_code(), 3);
    // Reported as a misconfigured root (an error), as the reclaim does.
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.message.contains("PrivateTmp")),
        "{:#?}",
        report.errors
    );
    let touched: Vec<_> = host
        .trace()
        .into_iter()
        .filter(|c| {
            c.path
                .as_deref()
                .is_some_and(|p| p.starts_with("/tmp/systemd-private-abc-cups.service-x1"))
        })
        .collect();
    assert!(touched.is_empty(), "{touched:#?}");
}

/// F4 (task-68 review): a big flat directory of names that match but are
/// no candidates (files, for `type: dir`) cannot run past the deadline:
/// the walk checks it after each listing and before each `statx`, so the
/// dry run ends `deadline` (124), never `dry_run` (0).
#[test]
fn discovery_stops_at_the_deadline_after_a_listing_and_between_statx_calls() {
    let files = "  /tmp/pytest-of-1: {}\n  /tmp/pytest-of-2: {}\n  /tmp/pytest-of-3: {}\n";
    for (tree, event) in [
        // The listing of /tmp itself outlasts the deadline (no name in it
        // matches, so no statx follows to notice).
        (
            "  /tmp/a: {}\n  /tmp/b: {}\n",
            "{at: {op: read_dir, path: /tmp, when: after}, advance: 700s}",
        ),
        // A statx of the first matching name does.
        (
            files,
            "{at: {op: statx, path: /tmp/pytest-of-1, when: after}, advance: 700s}",
        ),
    ] {
        let host = host(&yaml(&format!("{tree}events:\n  - {event}\n")));
        let report = dry_run(&host, &request("profile:pytest-of-dir", &[], "/tmp"));
        assert!(host.unfired_events().is_empty(), "{event}");
        assert_eq!(report.status, Status::Deadline, "{event}: {report:#?}");
        assert_eq!(report.exit_code(), 124, "{event}");
    }
}

/// task-68's review residuals (Codex F4): discovery no longer ends `0`
/// past the deadline through any of its other exits: the last matching
/// name's `statx` crossing it (no `statx` follows to notice, its type is no
/// candidate's, and the profile has one root, so no other root's check
/// follows), a listing that fails (a skip, so no candidate follows either),
/// and the source's `where` between its `has` and its `has_not`. Each ends
/// `deadline` (124). The checks back each other up: the first two need the
/// check after the listing or the one before discovery reports; the third
/// is also caught by the plan's check before it judges a candidate.
#[test]
fn discovery_stops_at_the_deadline_through_every_exit() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let cases = [
        (
            "profile:unmounted",
            None,
            "/tmp",
            "  /tmp/pytest-of-1: {}\n  /tmp/pytest-of-2: {}\n".to_owned(),
            "  - {at: {op: statx, path: /tmp/pytest-of-2, when: after}, advance: 700s}\n",
        ),
        (
            "profile:nested-dir",
            None,
            "/tmp",
            "  /tmp/nix-shell.A: {.: {}}\n".to_owned(),
            "  - {at: {op: read_dir, path: /tmp/nix-shell.A, when: before}, advance: 700s}\n  - \
             {at: {op: read_dir, path: /tmp/nix-shell.A, when: before}, fail: EACCES}\n",
        ),
        (
            "profile:marker-build-fixture",
            Some("/home/mark/src"),
            "/home/mark",
            format!(
                "  /home/mark/src/build:\n    .: {at}\n    CMakeCache.txt: {at}\n    obj: {at}\n"
            ),
            "  - {at: {op: statx, path: /home/mark/src/build/CMakeCache.txt, when: after}, \
             advance: 700s}\n",
        ),
    ];
    for (select, root, path, tree, events) in cases {
        let host = host(&yaml(&format!("{tree}events:\n{events}")));
        let roots: Vec<&str> = root.into_iter().collect();
        let report = dry_run(&host, &request(select, &roots, path));
        assert!(host.unfired_events().is_empty(), "{events}");
        assert_eq!(report.status, Status::Deadline, "{events}: {report:#?}");
        assert_eq!(report.exit_code(), 124, "{events}");
    }
}

/// The size estimate counts each object once (docs/architecture.md, The
/// index: `est_bytes`; task-68 found the fs estimate counting a hard link
/// per name): a candidate holding a 1M file under two names is estimated
/// as one holding it under one.
#[test]
fn the_size_estimate_counts_a_hard_linked_object_once() {
    let at = "mtime: 2026-10-01T00:00:00Z";
    let host = host(&yaml(&format!(
        "  /tmp/pytest-of-two:\n    .: {{{at}}}\n    f: {{size: 1M, {at}}}\n    g: {{link: \
         /tmp/pytest-of-two/f}}\n  /tmp/pytest-of-one:\n    .: {{{at}}}\n    f: {{size: 1M, \
         {at}}}\n"
    )));
    let report = dry_run(&host, &request("profile:unmounted", &[], "/tmp"));
    let estimate = |candidate: &str| {
        report
            .plan
            .iter()
            .find(|p| p.candidate == Path::new(candidate))
            .and_then(|p| p.estimated_bytes)
            .unwrap_or_else(|| panic!("{candidate}: {:#?}", report.plan))
    };
    assert!(estimate("/tmp/pytest-of-one") >= 1 << 20);
    assert_eq!(
        estimate("/tmp/pytest-of-two"),
        estimate("/tmp/pytest-of-one")
    );
}

/// task-71: any entry named `.git`, `.hg` or `.svn`, at any depth the walk
/// sees and of any type, refuses a plain-delete candidate as
/// `contains_checkout`: a `/tmp` directory holding two Git trees (QA's
/// tmp-entry case), a worktree's `.git` *file* deep inside, a `.git`
/// symlink (never followed), Mercurial's and Subversion's directories.
/// Names that only look alike (`.gitignore`, `.github`, `git`, `.hgrc`)
/// are plain entries: that candidate stays eligible.
#[test]
fn a_checkout_marker_anywhere_in_a_plain_delete_candidate_is_contains_checkout() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let host = host(&yaml(&format!(
        "  /tmp/two-repos:\n    .: {at}\n    a: {{.: {at}, .git: {{.: {at}, HEAD: {at}}}}}\n    b: \
         {{.: {at}, .git: {{.: {at}, HEAD: {at}}}}}\n  /tmp/worktree:\n    .: {at}\n    x: {{.: \
         {at}, y: {{.: {at}, z: {{.: {at}, .git: {at}, f: {at}}}}}}}\n  /tmp/gitlink:\n    .: \
         {at}\n    sub: {{.: {at}, .git: {{symlink: /tmp/elsewhere, mtime: \
         2026-10-01T00:00:00Z}}}}\n  /tmp/hg:\n    .: {at}\n    r: {{.: {at}, .hg: {{.: {at}, \
         store: {{.: {at}}}}}}}\n  /tmp/svn:\n    .: {at}\n    .svn: {{.: {at}, wc.db: {at}}}\n  \
         /tmp/look-alike:\n    .: {at}\n    .gitignore: {at}\n    .github: {{.: {at}}}\n    \
         git: {{.: {at}}}\n    .hgrc: {at}\n    .svnignore: {at}\n"
    )));
    let report = dry_run(&host, &request("profile:tmp-entry", &[], "/tmp"));
    let mut found = plan(&report);
    found.sort();
    assert_eq!(
        found,
        [
            ("/tmp/gitlink".to_owned(), Some("contains_checkout")),
            ("/tmp/hg".to_owned(), Some("contains_checkout")),
            ("/tmp/look-alike".to_owned(), None),
            ("/tmp/svn".to_owned(), Some("contains_checkout")),
            ("/tmp/two-repos".to_owned(), Some("contains_checkout")),
            ("/tmp/worktree".to_owned(), Some("contains_checkout")),
        ],
        "{:#?}",
        report.plan
    );
    // The walk stops at the marker: one `statx` of it (to admit it), and
    // no other call on it or below it (no open, no listing).
    let marker = Path::new("/tmp/two-repos/a/.git");
    let touched: Vec<_> = host
        .trace()
        .into_iter()
        .filter(|c| c.path.as_deref().is_some_and(|p| p.starts_with(marker)))
        .collect();
    assert!(
        matches!(touched.as_slice(), [only] if only.op == Op::Statx
            && only.path.as_deref() == Some(marker)),
        "expected exactly one statx of the marker: {touched:#?}"
    );
    nothing_locked_or_deleted(&host);
}

/// task-80: the checkout guard is the source profile's setting. A user
/// profile without `checkouts` refuses a candidate holding a checkout
/// (`contains_checkout`); the same profile with `checkouts: allow` takes
/// it, and the dry run's log says so once, naming the profile. Everything
/// else is judged the same: a keep marker still keeps.
#[test]
fn a_profile_with_checkouts_allow_takes_a_candidate_holding_a_checkout() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let host = || {
        host(&yaml(&format!(
            "  /tmp/repo:\n    .: {at}\n    a: {{.: {at}, .git: {{.: {at}, HEAD: {at}}}}}\n  \
             /tmp/hg: {{.: {at}, .hg: {{.: {at}, store: {{.: {at}}}}}}}\n  /tmp/kept-repo:\n    \
             .: {at}\n    .diskgc-keep: {at}\n    .git: {{.: {at}}}\n  /tmp/plain: {{.: {at}, f: \
             {at}}}\n"
        )))
    };
    let allows = |lines: &[String]| {
        lines
            .iter()
            .filter(|l| l.contains("checkouts: allow"))
            .count()
    };
    let refusing = host();
    let (report, lines) = dry_run_logged(&refusing, &request("profile:tmp-entry", &[], "/tmp"));
    let mut found = plan(&report);
    found.sort();
    assert_eq!(
        found,
        [
            ("/tmp/hg".to_owned(), Some("contains_checkout")),
            ("/tmp/kept-repo".to_owned(), Some("kept")),
            ("/tmp/plain".to_owned(), None),
            ("/tmp/repo".to_owned(), Some("contains_checkout")),
        ],
        "{:#?}",
        report.plan
    );
    assert_eq!(allows(&lines), 0, "{lines:#?}");
    assert!(
        report.plan.iter().all(|p| p.checkouts == Checkouts::Refuse),
        "{:#?}",
        report.plan
    );
    // As `--json` emits it: every plan entry says `refuse`.
    let json: serde_json::Value =
        serde_json::from_str(&report.to_json()).unwrap_or_else(|e| panic!("{e}"));
    let emitted: Vec<&serde_json::Value> = json["plan"]
        .as_array()
        .expect("a plan")
        .iter()
        .map(|p| &p["checkouts"])
        .collect();
    assert!(
        !emitted.is_empty() && emitted.iter().all(|c| *c == "refuse"),
        "{emitted:?}"
    );
    nothing_locked_or_deleted(&refusing);

    let allowing = host();
    let (report, lines) = dry_run_logged(
        &allowing,
        &request("profile:tmp-entry-checkouts", &[], "/tmp"),
    );
    let mut found = plan(&report);
    found.sort();
    assert_eq!(
        found,
        [
            ("/tmp/hg".to_owned(), None),
            ("/tmp/kept-repo".to_owned(), Some("kept")),
            ("/tmp/plain".to_owned(), None),
            ("/tmp/repo".to_owned(), None),
        ],
        "{:#?}",
        report.plan
    );
    // The executor applies the policy; config loading emits the operator
    // warning once, before any tier runs.
    assert_eq!(allows(&lines), 0, "{lines:#?}");
    // And in the plan (`--json`): every entry was judged under `allow`.
    assert!(
        report.plan.iter().all(|p| p.checkouts == Checkouts::Allow),
        "{:#?}",
        report.plan
    );
    nothing_locked_or_deleted(&allowing);
}

/// task-80: the path rule follows the same setting. Below a project root,
/// `proj/.git/objects` is `contains_checkout` for a profile without
/// `checkouts`, and eligible for the same patterns with `checkouts:
/// allow`.
#[test]
fn the_checkout_path_rule_follows_the_profiles_setting() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let host = || {
        host(&yaml(&format!(
            "  /home/mark/src/proj/.git/objects: {{.: {at}, ab: {{.: {at}, cdef: {at}}}}}\n  \
             /home/mark/src/proj/build/out: {{.: {at}, a.o: {at}}}\n"
        )))
    };
    for (profile, metadata) in [
        ("deep-dirs", Some("contains_checkout")),
        ("deep-dirs-checkouts", None),
    ] {
        let host = host();
        let report = dry_run(
            &host,
            &request(
                &format!("profile:{profile}"),
                &["/home/mark/src"],
                "/home/mark",
            ),
        );
        let mut found = plan(&report);
        found.sort();
        assert_eq!(
            found,
            [
                ("/home/mark/src/proj/.git/objects".to_owned(), metadata),
                ("/home/mark/src/proj/build/out".to_owned(), None),
            ],
            "{profile}: {:#?}",
            report.plan
        );
        nothing_locked_or_deleted(&host);
    }
}

/// A directory used as TMPDIR after exec is indistinguishable from stale
/// scratch when the process has no descriptor beneath it. The model only
/// records the initial environment, as LinuxHost reads from /proc. This
/// negative case is why the broad temp sources are not built-ins.
#[test]
fn dormant_post_exec_tmpdir_is_a_known_process_scan_false_negative() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let host = host(&yaml(&format!(
        "  /tmp/tmp.post-exec: {{.: {at}, scratch: {at}}}\n  \
         /tmp/tmp.initial: {{.: {at}, scratch: {at}}}\n  \
         /tmp/nix-shell.post-exec: {{.: {at}, scratch: {at}}}\n  \
         /tmp/claude-1000/nix-shell.nested: {{.: {at}, scratch: {at}}}\nprocesses:\n{}{}",
        process(20, "/", ""),
        process(21, "/", ", TMPDIR=/tmp/tmp.initial"),
    )));
    let report = dry_run(&host, &request("profile:tmp-entry", &[], "/tmp"));
    assert_eq!(reason_of(&report, "/tmp/tmp.post-exec"), Some(None));
    assert_eq!(reason_of(&report, "/tmp/tmp.initial"), Some(Some("in_use")));
    assert_eq!(reason_of(&report, "/tmp/nix-shell.post-exec"), Some(None));
    assert_eq!(reason_of(&report, "/tmp/claude-1000"), Some(None));
    let shipped = Library::builtin().expect("built-ins");
    for name in [
        "mktemp-dir",
        "nix-shell-tmpdir",
        "tmp-entry",
        "containers-image-tmp",
    ] {
        assert!(shipped.get(name).is_none(), "{name} must not ship");
    }
    nothing_locked_or_deleted(&host);
}

/// Jest's fixed temp source discovers whole directories at the default
/// prefix and within the frozen TMPDIR prefixes. A file or an arbitrary
/// nested directory with the same name is never passed to the cleaner.
#[test]
fn jest_cache_finds_only_default_temp_directories_in_the_model() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let host = host(&yaml(&format!(
        "  /usr/bin/jest: {{mode: \"0755\"}}\n  \
         /tmp/jest_rs: {{.: {at}, payload: {{size: 1M}}}}\n  \
         /tmp/nix-shell.Ab1/jest_app_hash: {{.: {at}, payload: {{size: 1M}}}}\n  \
         /tmp/subdir/jest_decoy: {{.: {at}, data: {at}}}\n  \
         /tmp/jest_file: {at}\n  \
         /tmp/jest: {{.: {at}}}\n  \
         /var/tmp/jest_foreign: {{.: {at}, payload: {{size: 1M}}}}\n"
    )));
    let report = dry_run(&host, &request("profile:jest-cache", &[], "/tmp"));
    assert!(report.errors.is_empty(), "{report:#?}");
    assert_eq!(reason_of(&report, "/var/tmp"), Some(Some("other_fs")));
    let mut actual: Vec<String> = plan(&report)
        .into_iter()
        .filter(|(_, reason)| reason.is_none())
        .map(|(path, _)| path)
        .collect();
    actual.sort();
    assert_eq!(actual, ["/tmp/jest_rs", "/tmp/nix-shell.Ab1/jest_app_hash"]);
    assert!(
        host.trace()
            .iter()
            .all(|call| call.path.as_deref() != Some(Path::new("/var/tmp/jest_foreign"))),
        "foreign-root child was inspected"
    );
    nothing_locked_or_deleted(&host);
}

/// node-compile-cache plans one Node cache tree per TMPDIR. Path lookalikes
/// (Claude session and Chromium scratch entries, whose built-ins were
/// withdrawn in TASK-122) and a nested Node version directory do not
/// become extra candidates.
#[test]
fn node_compile_cache_builtin_plans_only_its_units() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let host = host(&yaml(&format!(
        "  /tmp/claude-1000/-home-mark-src-app/session-1: {{.: {at}, output: {at}}}\n  \
         /tmp/claude-1000/nix-shell.Ab1/pytest-of-mark: {{.: {at}}}\n  \
         /tmp/.org.chromium.Chromium.A: {at}\n  \
         /tmp/org.chromium.Chromium.X: {{.: {at}, data: {at}}}\n  \
         /tmp/org.mozilla.firefox.X: {{.: {at}}}\n  \
         /tmp/node-compile-cache: {{.: {at}, v24: {{.: {at}}}}}\n  \
         /tmp/nix-shell.Ab1/node-compile-cache: {{.: {at}}}\n  \
         /tmp/claude-1000/nix-shell.Ab1/node-compile-cache: {{.: {at}}}\n"
    )));
    let report = dry_run(&host, &request("profile:node-compile-cache", &[], "/tmp"));
    let mut actual: Vec<String> = plan(&report)
        .into_iter()
        .filter(|(_, reason)| reason.is_none())
        .map(|(path, _)| path)
        .collect();
    actual.sort();
    let mut want = vec![
        "/tmp/node-compile-cache",
        "/tmp/nix-shell.Ab1/node-compile-cache",
        "/tmp/claude-1000/nix-shell.Ab1/node-compile-cache",
    ];
    want.sort();
    assert_eq!(actual, want, "{:#?}", report.plan);
    nothing_locked_or_deleted(&host);
}

/// task-71 (Mark, 2026-10-18): a candidate that lies inside a checkout's
/// metadata is refused by its path alone, as `contains_checkout`: a broad
/// pattern under a project root finds `proj/.git/objects` and
/// `proj/.hg/store`, whose own trees hold no marker. Look-alike names on
/// the way (`.github`) and an ordinary build directory stay eligible.
#[test]
fn a_candidate_inside_a_checkouts_metadata_is_contains_checkout_by_its_path() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let host = host(&yaml(&format!(
        "  /home/mark/src/proj/.git/objects: {{.: {at}, ab: {{.: {at}, cdef: {at}}}}}\n  \
         /home/mark/src/proj/.hg/store: {{.: {at}, data: {{.: {at}}}}}\n  \
         /home/mark/src/proj/.github/workflows: {{.: {at}, ci.yml: {at}}}\n  \
         /home/mark/src/proj/build/out: {{.: {at}, a.o: {at}}}\n"
    )));
    let report = dry_run(
        &host,
        &request("profile:deep-dirs", &["/home/mark/src"], "/home/mark"),
    );
    let mut found = plan(&report);
    found.sort();
    assert_eq!(
        found,
        [
            (
                "/home/mark/src/proj/.git/objects".to_owned(),
                Some("contains_checkout")
            ),
            ("/home/mark/src/proj/.github/workflows".to_owned(), None),
            (
                "/home/mark/src/proj/.hg/store".to_owned(),
                Some("contains_checkout")
            ),
            ("/home/mark/src/proj/build/out".to_owned(), None),
        ],
        "{:#?}",
        report.plan
    );
    nothing_locked_or_deleted(&host);
}
