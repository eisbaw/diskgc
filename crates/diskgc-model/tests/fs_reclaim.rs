//! Kind `fs` in a reclaiming `ensure` (task-18; docs/safety.md,
//! Filesystem candidates: Deletion; diskgc-core `fs_kind::reclaim`): every
//! candidate discovery found is reached again from its root, `flock`ed,
//! judged again under the lock, checked against a fresh process scan and
//! deleted by the engine's identity-checked delete. Every call is on the
//! trace, so "never touched" and "deleted nothing else" are asserted on
//! what was called and on the model's tree, not on what was reported.
//!
//! The profiles are user profiles in the fixtures (no built-in names: the
//! built-in /tmp library ships with task-65).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::engine::{Checkouts, Rules};
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::fs_kind::{self, EntryFilter, Find, Judge};
use diskgc_core::host::Host;
use diskgc_core::inuse::{Checker, REUSE_LIMIT};
use diskgc_core::outcome::{ReclaimEnd, Stop};
use diskgc_core::profile::{EntryType, Library, Where, parse_profiles};
use diskgc_core::report::{Report, Status};
use diskgc_core::units::parse_size;
use diskgc_model::spec::{FileSpec, NodeSpec, Op, StatxField};
use diskgc_model::{Call, ModelHost};

const PROFILES: &str = r#"
- name: pytest-of-dir
  find:
    roots: [/tmp]
    paths: ["pytest-of-*", "nix-shell.*/pytest-of-*"]
    type: dir
  action: delete
  examples: {match: [/tmp/pytest-of-mark]}
- name: tmp-entry
  find: {roots: [/tmp], paths: ["*"], type: any}
  action: delete
  examples: {match: [/tmp/x]}
- name: has-tag
  where: {has: CACHEDIR.TAG}
- name: has-not-tag
  where: {has_not: CACHEDIR.TAG}
- name: tagged-idle-unkept
  where: {has: CACHEDIR.TAG, has_not: .keep, idle_for: 2d}
- name: anything
  find: {roots: required, paths: ["*"], type: any}
  action: delete
  examples: {match: [/srv/x]}
- name: pytest-cache-fixture
  find: {roots: required, paths: [".pytest_cache", "*/.pytest_cache"], type: dir}
  action: delete
  examples: {match: [/home/mark/src/app/.pytest_cache]}
- name: marker-build-fixture
  find: {roots: required, paths: ["*", "*/*"], type: dir}
  where: {has: CMakeCache.txt, lacks_entry: CMakeLists.txt}
  action: delete
  examples: {root: /home/mark/src, match: [/home/mark/src/build]}
- name: deep-dirs
  find: {roots: required, paths: ["*/*/*"], type: dir}
  action: delete
  examples: {match: [/srv/a/b/c]}
- name: deep-dirs-checkouts
  find: {roots: required, paths: ["*/*/*"], type: dir}
  checkouts: allow
  action: delete
  examples: {match: [/srv/a/b/c]}
- name: writable-tmp
  find: {roots: [/tmp], paths: ["ro-*"], type: dir}
  action: {delete: {make_writable: true}}
  examples: {match: [/tmp/ro-x]}
- name: plain-tmp
  find: {roots: [/tmp], paths: ["ro-*"], type: dir}
  action: delete
  examples: {match: [/tmp/ro-x]}
- name: cleaner
  find: {roots: [/tmp], paths: ["pytest-of-*"], type: dir}
  action: {command: [rm, -rf, "{path}"]}
  examples: {match: [/tmp/pytest-of-mark]}
- name: missing-cleaner
  find: {roots: [/tmp], paths: ["pytest-of-*"], type: dir}
  action: {command: [diskgc-no-such-tool, "{path}"]}
  examples: {match: [/tmp/pytest-of-mark]}
- name: env-cleaner
  find: {roots: [/tmp], paths: ["pytest-of-*"], type: dir}
  action: {command: [env, "PATH=/usr/bin", rm, -rf, "{path}"]}
  examples: {match: [/tmp/pytest-of-mark]}
- name: guarded-cleaner
  find: {roots: [/tmp], paths: ["pytest-of-*"], type: dir}
  where: {app_not_running: /tmp/browser-config/SingletonLock}
  action: {command: [rm, -rf, "{path}"]}
  examples: {match: [/tmp/pytest-of-mark]}
- name: union-allow
  find: {roots: [/tmp], paths: ["union-a*", "union-shared*"], type: dir}
  checkouts: allow
  action: delete
  examples: {match: [/tmp/union-a]}
- name: union-refuse
  find: {roots: [/tmp], paths: ["union-b*", "union-shared*"], type: dir}
  action: delete
  examples: {match: [/tmp/union-b]}
- name: metadata-root-allow
  find: {roots: [/tmp], paths: [repo/.git/cache-file], type: file}
  checkouts: allow
  action: delete
  examples: {match: [/tmp/repo/.git/cache-file]}
- name: metadata-root-refuse
  find: {roots: [/tmp/repo/.git], paths: [cache-file], type: file}
  action: delete
  examples: {match: [/tmp/repo/.git/cache-file]}
- name: alias-cache-allow
  find: {roots: [/tmp], paths: [cache-file], type: file}
  checkouts: allow
  action: delete
  examples: {match: [/tmp/cache-file]}
- name: alias-cache-refuse
  find: {roots: [/tmp], paths: [cache-file], type: file}
  action: delete
  examples: {match: [/tmp/cache-file]}
- name: alias-marker-allow
  find: {roots: [/tmp], paths: [.git], type: file}
  checkouts: allow
  action: delete
  examples: {match: [/tmp/.git]}
- name: alias-marker-refuse
  find: {roots: [/tmp], paths: [.git], type: file}
  action: delete
  examples: {match: [/tmp/.git]}
- name: union-recent
  find: {roots: [/tmp], paths: ["union-clock*"], type: dir}
  action: delete
  examples: {match: [/tmp/union-clock]}
- name: union-old
  find: {roots: [/tmp], paths: ["union-clock*"], type: dir}
  action: delete
  examples: {match: [/tmp/union-clock]}
- name: union-bad-root
  find: {roots: [/tmp/untrusted], paths: [item], type: dir}
  action: delete
  examples: {match: [/tmp/untrusted/item]}
"#;

const OLD: &str = "2026-10-01T00:00:00Z";
const HOST_LOCK: &str = "  /run/diskgc/lock: {uid: 0, gid: 0}\n";

fn library() -> Library {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"))
}

/// `/` and `/home` (ext4), `/run` (tmpfs, the host lock provisioned) and
/// `/tmp` (a 1 GiB tmpfs, 500 MiB used outside the model, mount 30, dev
/// 0:42); the clock is 2026-10-15; process 7 is a readable bystander.
/// `mounts`: further mount lines; `tree`: tree entries; `rest`: further
/// top-level keys (processes after pid 7, locks, events).
fn yaml(mounts: &str, tree: &str, rest: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 100G, other_used: 99G, parent: 1}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, other_used: 500M, parent: 1}}
{mounts}tree:
  /usr/bin/tool: {{}}
  /usr/bin/rm: {{mode: "0755"}}
  /usr/bin/env: {{mode: "0755"}}
{HOST_LOCK}{tree}processes:
  - {{pid: 7, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{{path: /usr/bin/tool}}], environ: [PATH=/usr/bin]}}
{rest}"#
    )
}

/// An old directory (every time [`OLD`]) holding a file `f` of `size`.
fn old_dir(size: &str) -> String {
    format!("{{.: {{mtime: {OLD}}}, f: {{size: {size}, mtime: {OLD}}}}}")
}

fn request(select: &str, roots: &[&str], path: &str, min: &str) -> Request {
    let roots: Vec<String> = roots.iter().map(|r| (*r).to_owned()).collect();
    let roots = (!roots.is_empty()).then_some(roots.as_slice());
    Request {
        path: PathBuf::from(path),
        min_free: parse_size(min).expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select(select, &library(), roots, 1000).unwrap_or_else(|e| panic!("{e}")),
    }
}

/// `--select SELECT` over /tmp, never met (`--min-free 100%`): every
/// candidate is processed.
fn tmp(select: &str) -> Request {
    request(select, &[], "/tmp", "100%")
}

#[test]
fn command_action_union_is_explicitly_refused() {
    let selection = "profile:cleaner || profile:cleaner";
    let error = Tier::select(selection, &library(), None, 1000)
        .expect_err("command action union has no lock-time disjunction protocol");
    assert!(error.contains("a union of this kind or action"), "{error}");
    assert!(error.contains("task-16"), "{error}");
    let mixed_delete = "profile:plain-tmp || profile:writable-tmp";
    assert!(
        Tier::select(mixed_delete, &library(), None, 1000).is_err(),
        "different delete options must never share one union action"
    );
}

#[test]
fn negated_idle_filter_uses_fresh_age_under_candidate_lock() {
    let tree = "  /tmp/pytest-of-recent: {.: {mtime: 2026-10-13T03:01:00Z}, f: {size: 1M, mtime: 2026-10-13T03:01:00Z}}\n  /tmp/pytest-of-ancient: {.: {mtime: 2026-10-01T00:00:00Z}, f: {size: 1M, mtime: 2026-10-01T00:00:00Z}}\n";
    let selection = "profile:pytest-of-dir && !filter:idle-2d";
    let plan_host = host(&yaml("", tree, ""));
    let plan = executor::dry_run(&plan_host, &tmp(selection), &[1000], &mut |_| {});
    assert!(
        plan.plan
            .iter()
            .any(|e| e.candidate == Path::new("/tmp/pytest-of-recent") && e.reason.is_none()),
        "{plan:#?}"
    );
    assert!(
        plan.plan
            .iter()
            .any(|e| e.candidate == Path::new("/tmp/pytest-of-ancient")
                && e.reason == Some("too_idle")),
        "{plan:#?}"
    );

    let event =
        "events:\n  - {at: {op: flock, path: /tmp/pytest-of-recent, when: after}, advance: 120s}\n";
    let host = host(&yaml("", tree, event));
    let report = executor::ensure(&host, &tmp(selection), &[1000], &mut |_| {});
    assert!(report.actions.is_empty(), "{report:#?}");
    assert!(
        report
            .skipped
            .iter()
            .any(|e| e.candidate == Path::new("/tmp/pytest-of-recent") && e.reason == "too_idle"),
        "{report:#?}"
    );
    assert!(
        host.unfired_events().is_empty(),
        "candidate lock event should fire"
    );
}

#[test]
fn entry_filters_are_rechecked_after_candidate_lock() {
    for (selection, tree, event, candidate) in [
        (
            "profile:pytest-of-dir && !filter:has-tag",
            "  /tmp/pytest-of-a: {.: {}, f: {size: 1M}}\n",
            "events:\n  - {at: {op: flock, path: /tmp/pytest-of-a, when: after}, create: /tmp/pytest-of-a/CACHEDIR.TAG}\n",
            "/tmp/pytest-of-a",
        ),
        (
            "profile:pytest-of-dir && !filter:has-not-tag",
            "  /tmp/pytest-of-a: {.: {}, CACHEDIR.TAG: {size: 1K}, f: {size: 1M}}\n",
            "events:\n  - {at: {op: flock, path: /tmp/pytest-of-a, when: after}, remove: /tmp/pytest-of-a/CACHEDIR.TAG}\n",
            "/tmp/pytest-of-a",
        ),
        (
            "profile:pytest-of-dir && filter:has-tag",
            "  /tmp/pytest-of-a: {.: {}, CACHEDIR.TAG: {size: 1K}, f: {size: 1M}}\n",
            "events:\n  - {at: {op: flock, path: /tmp/pytest-of-a, when: after}, remove: /tmp/pytest-of-a/CACHEDIR.TAG}\n",
            "/tmp/pytest-of-a",
        ),
        (
            "profile:pytest-of-dir && filter:has-not-tag",
            "  /tmp/pytest-of-a: {.: {}, f: {size: 1M}}\n",
            "events:\n  - {at: {op: flock, path: /tmp/pytest-of-a, when: after}, create: /tmp/pytest-of-a/CACHEDIR.TAG}\n",
            "/tmp/pytest-of-a",
        ),
    ] {
        let host = host(&yaml("", tree, event));
        let report = ensure(&host, &tmp(selection));
        assert!(report.actions.is_empty(), "{report:#?}");
        assert!(
            report
                .skipped
                .iter()
                .any(|s| s.candidate == Path::new(candidate) && s.reason == "filtered"),
            "{report:#?}"
        );
        assert!(exists(&host, candidate));
        assert!(host.unfired_events().is_empty());
    }
}

#[test]
fn mixed_entry_and_idle_filter_rechecks_under_candidate_lock() {
    let selection = "profile:pytest-of-dir && filter:tagged-idle-unkept";
    let tree = format!(
        "  /tmp/pytest-of-a: {{.: {{mtime: {OLD}}}, CACHEDIR.TAG: {{size: 1K, mtime: {OLD}}}, f: {{size: 1M, mtime: {OLD}}}}}\n"
    );
    for (event, expected) in [
        ("create: /tmp/pytest-of-a/.keep", "filtered"),
        ("remove: /tmp/pytest-of-a/CACHEDIR.TAG", "filtered"),
        ("create: /tmp/pytest-of-a/newer", "not_idle"),
    ] {
        let event = format!(
            "events:\n  - {{at: {{op: flock, path: /tmp/pytest-of-a, when: after}}, {event}}}\n"
        );
        let host = host(&yaml("", &tree, &event));
        let report = ensure(&host, &tmp(selection));
        assert!(report.actions.is_empty(), "{report:#?}");
        assert!(
            report
                .skipped
                .iter()
                .any(|skip| skip.candidate == Path::new("/tmp/pytest-of-a")
                    && skip.reason == expected),
            "{report:#?}"
        );
        assert!(exists(&host, "/tmp/pytest-of-a"));
        assert!(host.unfired_events().is_empty());
    }
    let host = host(&yaml("", &tree, ""));
    let report = ensure(&host, &tmp(selection));
    assert!(
        report
            .actions
            .iter()
            .any(|action| action.candidate == Path::new("/tmp/pytest-of-a")),
        "{report:#?}"
    );
}

#[test]
fn union_checkout_policy_uses_only_terms_admitted_by_their_filters() {
    let tree = "  /tmp/union-shared-clear: {.: {}, .git: {HEAD: {size: 1K}}, f: {size: 1M}}\n  /tmp/union-shared-tagged: {.: {}, .git: {HEAD: {size: 1K}}, CACHEDIR.TAG: {size: 1K}, f: {size: 1M}}\n";
    for selection in [
        "profile:union-allow || (profile:union-refuse && filter:has-tag)",
        "(profile:union-refuse && filter:has-tag) || profile:union-allow",
    ] {
        let planned_host = host(&yaml("", tree, ""));
        let request = tmp(selection);
        let plan = executor::dry_run(&planned_host, &request, &[1000], &mut |_| {});
        assert!(
            plan.plan.iter().any(|entry| {
                entry.candidate == Path::new("/tmp/union-shared-clear")
                    && entry.reason.is_none()
                    && entry.checkouts == diskgc_core::engine::Checkouts::Allow
            }),
            "{selection}: {plan:#?}"
        );
        assert!(
            plan.plan.iter().any(|entry| {
                entry.candidate == Path::new("/tmp/union-shared-tagged")
                    && entry.reason == Some("contains_checkout")
            }),
            "{selection}: {plan:#?}"
        );

        let reclaim_host = host(&yaml("", tree, ""));
        let report = ensure(&reclaim_host, &request);
        assert_eq!(
            actions(&report),
            ["/tmp/union-shared-clear"],
            "{selection}: {report:#?}"
        );
        assert!(!exists(&reclaim_host, "/tmp/union-shared-clear"));
        assert!(exists(&reclaim_host, "/tmp/union-shared-tagged"));
        assert_eq!(unlinked_below(&reclaim_host, "/tmp/union-shared-tagged"), 0);
        assert!(
            !reclaim_host.trace().iter().any(|call| {
                call.op == Op::Flock
                    && call.path.as_deref() == Some(Path::new("/tmp/union-shared-tagged"))
            }),
            "a checkout vetoed before the ranked reclaim queue: {selection}"
        );
    }
}

#[test]
fn mixed_filter_union_uses_only_admitted_terms_in_plan_and_prequeue_tally() {
    let tree = "  /tmp/union-shared-old: {.: {mtime: @OLD@}, .git: {.: {mtime: @OLD@}, HEAD: {size: 1K, mtime: @OLD@}}, CACHEDIR.TAG: {size: 1K, mtime: @OLD@}, f: {size: 1M, mtime: @OLD@}}\n  /tmp/union-shared-recent: {.: {}, .git: {HEAD: {size: 1K}}, CACHEDIR.TAG: {size: 1K}, f: {size: 1M}}\n  /tmp/union-shared-untagged: {.: {mtime: @OLD@}, .git: {.: {mtime: @OLD@}, HEAD: {size: 1K, mtime: @OLD@}}, f: {size: 1M, mtime: @OLD@}}\n  /tmp/union-shared-marked: {.: {mtime: @OLD@}, .git: {.: {mtime: @OLD@}, HEAD: {size: 1K, mtime: @OLD@}}, CACHEDIR.TAG: {size: 1K, mtime: @OLD@}, .keep: {size: 1K, mtime: @OLD@}, f: {size: 1M, mtime: @OLD@}}\n".replace("@OLD@", OLD);
    for selection in [
        "profile:union-allow || (profile:union-refuse && filter:tagged-idle-unkept)",
        "(profile:union-refuse && filter:tagged-idle-unkept) || profile:union-allow",
    ] {
        let request = tmp(selection);
        let planned_host = host(&yaml("", &tree, ""));
        let plan = executor::dry_run(&planned_host, &request, &[1000], &mut |_| {});
        for (name, reason, checkouts) in [
            ("old", Some("contains_checkout"), Checkouts::Refuse),
            ("recent", None, Checkouts::Allow),
            ("untagged", None, Checkouts::Allow),
            ("marked", None, Checkouts::Allow),
        ] {
            let path = format!("/tmp/union-shared-{name}");
            assert!(
                plan.plan
                    .iter()
                    .any(|entry| entry.candidate == Path::new(&path)
                        && entry.reason == reason
                        && entry.checkouts == checkouts),
                "{selection}, {path}: {plan:#?}"
            );
        }

        let reclaim_host = host(&yaml("", &tree, ""));
        let report = ensure(&reclaim_host, &request);
        let mut deleted = actions(&report);
        deleted.sort();
        assert_eq!(
            deleted,
            [
                "/tmp/union-shared-marked",
                "/tmp/union-shared-recent",
                "/tmp/union-shared-untagged",
            ],
            "{selection}: {report:#?}"
        );
        assert!(exists(&reclaim_host, "/tmp/union-shared-old"));
        assert_eq!(unlinked_below(&reclaim_host, "/tmp/union-shared-old"), 0);
        assert!(
            !reclaim_host.trace().iter().any(|call| call.op == Op::Flock
                && call.path.as_deref() == Some(Path::new("/tmp/union-shared-old"))),
            "the mixed-filter checkout veto must happen before the ranked queue: {selection}"
        );
    }
}

#[test]
fn mixed_filter_union_rechecks_newly_admitted_term_under_candidate_lock() {
    let tree = "  /tmp/union-shared-clear: {.: {mtime: @OLD@}, .git: {.: {mtime: @OLD@}, HEAD: {size: 1K, mtime: @OLD@}}, f: {size: 1M, mtime: @OLD@}}\n".replace("@OLD@", OLD);
    let event = "events:\n  - {at: {op: flock, path: /tmp/union-shared-clear, when: after}, create: /tmp/union-shared-clear/CACHEDIR.TAG}\n  - {at: {op: flock, path: /tmp/union-shared-clear, when: after}, advance: 3d}\n";
    for selection in [
        "profile:union-allow || (profile:union-refuse && filter:tagged-idle-unkept)",
        "(profile:union-refuse && filter:tagged-idle-unkept) || profile:union-allow",
    ] {
        let mut request = tmp(selection);
        request.timeout = Some(Duration::from_secs(4 * 86_400));
        let planned_host = host(&yaml("", &tree, ""));
        let plan = executor::dry_run(&planned_host, &request, &[1000], &mut |_| {});
        assert!(
            plan.plan.iter().any(
                |entry| entry.candidate == Path::new("/tmp/union-shared-clear")
                    && entry.reason.is_none()
                    && entry.checkouts == Checkouts::Allow
            ),
            "{selection}: {plan:#?}"
        );

        let host = host(&yaml("", &tree, event));
        let report = ensure(&host, &request);
        assert!(report.actions.is_empty(), "{selection}: {report:#?}");
        assert!(
            report.skipped.iter().any(|entry| entry.candidate
                == Path::new("/tmp/union-shared-clear")
                && entry.reason == "contains_checkout"),
            "{selection}: {report:#?}"
        );
        assert!(exists(&host, "/tmp/union-shared-clear"));
        assert_eq!(unlinked_below(&host, "/tmp/union-shared-clear"), 0);
        assert!(host.unfired_events().is_empty());
    }
}

#[test]
fn union_checkout_policy_rechecks_filter_under_candidate_lock() {
    let tree = "  /tmp/union-shared-clear: {.: {}, .git: {HEAD: {size: 1K}}, f: {size: 1M}}\n";
    let event = "events:\n  - {at: {op: flock, path: /tmp/union-shared-clear, when: after}, create: /tmp/union-shared-clear/CACHEDIR.TAG}\n";
    for selection in [
        "profile:union-allow || (profile:union-refuse && filter:has-tag)",
        "(profile:union-refuse && filter:has-tag) || profile:union-allow",
    ] {
        let host = host(&yaml("", tree, event));
        let report = ensure(&host, &tmp(selection));
        assert!(report.actions.is_empty(), "{selection}: {report:#?}");
        assert!(
            report.skipped.iter().any(|entry| {
                entry.candidate == Path::new("/tmp/union-shared-clear")
                    && entry.reason == "contains_checkout"
            }),
            "{selection}: {report:#?}"
        );
        assert!(exists(&host, "/tmp/union-shared-clear"));
        assert_eq!(unlinked_below(&host, "/tmp/union-shared-clear"), 0);
        assert!(host.unfired_events().is_empty());
    }
}

#[test]
fn union_checkout_policy_excludes_idle_failed_refusing_term_before_ranking() {
    let tree = "  /tmp/union-shared-recent: {.: {}, .git: {HEAD: {size: 1K}}, f: {size: 1M}}\n";
    for selection in [
        "profile:union-allow || (profile:union-refuse && filter:idle-1d)",
        "(profile:union-refuse && filter:idle-1d) || profile:union-allow",
    ] {
        let host = host(&yaml("", tree, ""));
        let report = ensure(&host, &tmp(selection));
        assert_eq!(
            actions(&report),
            ["/tmp/union-shared-recent"],
            "{selection}: {report:#?}"
        );
        assert!(!exists(&host, "/tmp/union-shared-recent"));
    }
}

#[test]
fn union_checkout_metadata_uses_only_admitted_refusing_source_roots() {
    let tree = "  /tmp/repo/.git/cache-file: {size: 1M}\n";
    for selection in [
        "profile:metadata-root-allow || profile:metadata-root-refuse",
        "profile:metadata-root-refuse || profile:metadata-root-allow",
    ] {
        let request = tmp(selection);
        let planned_host = host(&yaml("", tree, ""));
        let plan = executor::dry_run(&planned_host, &request, &[1000], &mut |_| {});
        assert!(
            plan.plan.iter().any(|entry| {
                entry.candidate == Path::new("/tmp/repo/.git/cache-file")
                    && entry.reason.is_none()
                    && entry.checkouts == Checkouts::Refuse
            }),
            "{selection}: {plan:#?}"
        );

        let reclaim_host = host(&yaml("", tree, ""));
        let report = ensure(&reclaim_host, &request);
        assert_eq!(
            actions(&report),
            ["/tmp/repo/.git/cache-file"],
            "{selection}: {report:#?}"
        );
        assert!(!exists(&reclaim_host, "/tmp/repo/.git/cache-file"));
    }
}

#[test]
fn union_checkout_marker_uses_every_admitted_refusing_hardlink_alias() {
    let tree = "  /tmp/cache-file: {size: 1M}\n  /tmp/.git: {link: /tmp/cache-file}\n";
    for selection in [
        "profile:alias-cache-allow || profile:alias-marker-refuse",
        "profile:alias-marker-refuse || profile:alias-cache-allow",
    ] {
        let request = tmp(selection);
        let planned_host = host(&yaml("", tree, ""));
        let plan = executor::dry_run(&planned_host, &request, &[1000], &mut |_| {});
        assert_eq!(plan.plan.len(), 1, "{selection}: {plan:#?}");
        assert_eq!(
            plan.plan[0].reason,
            Some("contains_checkout"),
            "{selection}: {plan:#?}"
        );
        assert_eq!(plan.plan[0].profile, "alias-marker-refuse");

        let reclaim_host = host(&yaml("", tree, ""));
        let report = ensure(&reclaim_host, &request);
        assert!(report.actions.is_empty(), "{selection}: {report:#?}");
        assert_eq!(report.skipped.len(), 1, "{selection}: {report:#?}");
        assert_eq!(report.skipped[0].reason, "contains_checkout");
        assert_eq!(report.skipped[0].profile, "alias-marker-refuse");
        assert!(exists(&reclaim_host, "/tmp/cache-file"));
        assert!(exists(&reclaim_host, "/tmp/.git"));
        assert!(calls(&reclaim_host, Op::Unlinkat).is_empty());
    }

    // The marker visible only to an allowing source must not veto the
    // refusing source's clean alias of the same inode.
    for selection in [
        "profile:alias-marker-allow || profile:alias-cache-refuse",
        "profile:alias-cache-refuse || profile:alias-marker-allow",
    ] {
        let request = tmp(selection);
        let planned_host = host(&yaml("", tree, ""));
        let plan = executor::dry_run(&planned_host, &request, &[1000], &mut |_| {});
        assert_eq!(plan.plan.len(), 1, "{selection}: {plan:#?}");
        assert!(plan.plan[0].reason.is_none(), "{selection}: {plan:#?}");
        let reclaim_host = host(&yaml("", tree, ""));
        let report = ensure(&reclaim_host, &request);
        assert_eq!(report.actions.len(), 1, "{selection}: {report:#?}");
    }
}

#[test]
fn hardlink_alias_checkout_refusal_survives_lock_time_recheck() {
    let tree = "  /tmp/cache-file: {size: 1M}\n  /tmp/.git: {link: /tmp/cache-file}\n";
    for marker_first in [false, true] {
        let host = host(&yaml("", tree, ""));
        let cache_find = Find::new(
            &["cache-file".to_owned()],
            EntryType::File,
            &Where::default(),
            1000,
        );
        let marker_find = Find::new(
            &[".git".to_owned()],
            EntryType::File,
            &Where::default(),
            1000,
        );
        let roots = [PathBuf::from("/tmp")];
        let (cache_discovered, cache_stop) = fs_kind::discover(&host, &roots, &cache_find, None);
        let (marker_discovered, marker_stop) = fs_kind::discover(&host, &roots, &marker_find, None);
        assert!(cache_stop.is_none() && marker_stop.is_none());
        let cache_found = cache_discovered.candidates[0]
            .1
            .as_ref()
            .expect("cache alias");
        let marker_found = marker_discovered.candidates[0]
            .1
            .as_ref()
            .expect("marker alias");
        let owners = [1000];
        let rules = Rules {
            checkouts: Checkouts::Allow,
            ..Rules::new(&owners)
        };
        let dev = host.stat_path(Path::new("/tmp")).expect("/tmp").dev;
        let idle = diskgc_core::profile::IdleWindow::default();
        let cache = fs_kind::Alternative {
            judge: Judge {
                find: &cache_find,
                rules: &rules,
                idle,
                dev,
            },
            found: cache_found,
            checkouts: Checkouts::Allow,
        };
        let marker = fs_kind::Alternative {
            judge: Judge {
                find: &marker_find,
                rules: &rules,
                idle,
                dev,
            },
            found: marker_found,
            checkouts: Checkouts::Refuse,
        };
        let alternatives = if marker_first {
            [marker, cache]
        } else {
            [cache, marker]
        };
        let mut checker = Checker::new(&rules, REUSE_LIMIT);
        let done = fs_kind::reclaim_alternatives_checked(
            &host,
            &alternatives,
            false,
            &mut checker,
            None,
            &mut |_, _| Ok(None),
        );
        assert!(
            matches!(done.end, ReclaimEnd::Skipped(ref skip) if skip.reason.as_str() == "contains_checkout"),
            "marker_first={marker_first}: {done:?}"
        );
        assert!(exists(&host, "/tmp/cache-file"));
        assert!(exists(&host, "/tmp/.git"));
        assert!(calls(&host, Op::Unlinkat).is_empty());
    }
}

#[test]
fn no_admitted_union_term_reports_filtered_even_inside_checkout_metadata() {
    let host = host(&yaml(
        "",
        "  /tmp/repo/.git/objects: {.: {}, payload: {size: 1M}}\n",
        "",
    ));
    let mut find = Find::new(
        &["repo/.git/objects".to_owned()],
        EntryType::Dir,
        &Where::default(),
        1000,
    );
    find.entry_filters.push(EntryFilter {
        name: "has-tag".to_owned(),
        entry: "CACHEDIR.TAG".to_owned(),
        has: true,
        negated: false,
    });
    let (discovered, stopped) = fs_kind::discover(&host, &[PathBuf::from("/tmp")], &find, None);
    assert!(stopped.is_none(), "{stopped:?}");
    let found = discovered.candidates[0].1.as_ref().expect("candidate");
    let owners = [1000];
    let rules = Rules {
        checkouts: Checkouts::Allow,
        ..Rules::new(&owners)
    };
    let judge = Judge {
        find: &find,
        rules: &rules,
        idle: diskgc_core::profile::IdleWindow::default(),
        dev: host.stat_path(Path::new("/tmp")).expect("/tmp").dev,
    };
    let alternatives = [
        fs_kind::Alternative {
            judge,
            found,
            checkouts: Checkouts::Refuse,
        },
        fs_kind::Alternative {
            judge,
            found,
            checkouts: Checkouts::Refuse,
        },
    ];
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let done = fs_kind::reclaim_alternatives_checked(
        &host,
        &alternatives,
        false,
        &mut checker,
        None,
        &mut |_, _| Ok(None),
    );
    assert!(
        matches!(done.end, ReclaimEnd::Skipped(ref skip) if skip.reason.as_str() == "filtered"),
        "{done:?}"
    );
    assert!(exists(&host, "/tmp/repo/.git/objects"));
    assert_eq!(unlinked_below(&host, "/tmp/repo/.git/objects"), 0);
}

#[test]
fn filesystem_union_dedupes_identity_and_refusal_wins_in_both_orders() {
    let tree = format!(
        "  /tmp/union-a: {{.: {{mtime: {OLD}}}, .git: {{HEAD: {{size: 1K, mtime: {OLD}}}}}}}\n  /tmp/union-b: {}\n  /tmp/union-shared-clear: {}\n  /tmp/union-shared-checkout: {{.: {{mtime: {OLD}}}, .git: {{HEAD: {{size: 1K, mtime: {OLD}}}}}}}\n",
        old_dir("1M"),
        old_dir("1M"),
    );
    for selection in [
        "profile:union-allow || profile:union-refuse",
        "profile:union-refuse || profile:union-allow",
    ] {
        let host = host(&yaml("", &tree, ""));
        let request = tmp(selection);
        let plan = executor::dry_run(&host, &request, &[1000], &mut |_| {});
        let shared: Vec<_> = plan
            .plan
            .iter()
            .filter(|entry| entry.candidate == Path::new("/tmp/union-shared-clear"))
            .collect();
        assert_eq!(shared.len(), 1, "{selection}: {plan:#?}");
        assert_eq!(shared[0].checkouts, diskgc_core::engine::Checkouts::Refuse);
        assert!(
            plan.plan.iter().any(|entry| entry.candidate
                == Path::new("/tmp/union-shared-checkout")
                && entry.reason == Some("contains_checkout")),
            "{selection}: {plan:#?}"
        );
        let report = ensure(&host, &request);
        let mut done = actions(&report);
        done.sort();
        assert_eq!(
            done,
            ["/tmp/union-a", "/tmp/union-b", "/tmp/union-shared-clear"],
            "{selection}: {report:#?}"
        );
        assert!(exists(&host, "/tmp/union-shared-checkout"));
        assert_eq!(unlinked_below(&host, "/tmp/union-shared-checkout"), 0);
    }
}

#[test]
fn filesystem_union_attributes_second_source_root_refusal_and_excludes_tier() {
    let tree = "  /tmp/union-a: {.: {}}\n  /tmp/untrusted: {symlink: /home}\n";
    let selection = "profile:union-allow || profile:union-bad-root";
    let mut request = tmp(selection);
    request.tier.fs_union[0].roots.push(PathBuf::from("/home"));
    let planned_host = host(&yaml("", tree, ""));
    let plan = executor::dry_run(&planned_host, &request, &[1000], &mut |_| {});
    assert_eq!(plan.errors.len(), 1, "{plan:#?}");
    assert_eq!(plan.errors[0].profile, "union-bad-root");
    assert_eq!(plan.plan.len(), 1, "{plan:#?}");
    assert_eq!(plan.plan[0].candidate, Path::new("/home"));
    assert_eq!(plan.plan[0].reason, Some("other_fs"));

    let reclaim_host = host(&yaml("", tree, ""));
    let report = ensure(&reclaim_host, &request);
    assert_eq!(report.errors.len(), 1, "{report:#?}");
    assert_eq!(report.errors[0].profile, "union-bad-root");
    assert!(
        report
            .skipped
            .iter()
            .any(|skip| skip.candidate == Path::new("/home") && skip.reason == "other_fs"),
        "{report:#?}"
    );
    assert!(report.actions.is_empty(), "{report:#?}");
    assert!(exists(&reclaim_host, "/tmp/union-a"));
}

#[test]
fn filesystem_union_skips_foreign_second_source_and_reclaims_same_device_source() {
    let tree = "  /tmp/union-a: {.: {}}\n  /home: {.: {}}\n  /home/union-foreign: {.: {}}\n";
    let mut request = tmp("profile:union-allow || profile:union-bad-root");
    request.tier.fs_union[0].roots = vec![PathBuf::from("/home")];

    let plan_host = host(&yaml("", tree, ""));
    let plan = executor::dry_run(&plan_host, &request, &[1000], &mut |_| {});
    assert!(plan.errors.is_empty(), "{plan:#?}");
    assert_eq!(request.tier.fs_union[0].profile, "union-bad-root");
    assert!(
        plan.plan.iter().any(|entry| {
            entry.profile == "union-bad-root"
                && entry.candidate == Path::new("/home")
                && entry.reason == Some("other_fs")
        }),
        "{plan:#?}"
    );
    assert!(
        plan.plan
            .iter()
            .any(|entry| entry.candidate == Path::new("/tmp/union-a") && entry.reason.is_none()),
        "{plan:#?}"
    );
    assert_eq!(
        plan_host
            .trace()
            .iter()
            .filter(|call| call.path.as_deref() == Some(Path::new("/home/union-foreign")))
            .count(),
        0
    );

    let reclaim_host = host(&yaml("", tree, ""));
    let report = ensure(&reclaim_host, &request);
    assert!(report.errors.is_empty(), "{report:#?}");
    assert_eq!(actions(&report), ["/tmp/union-a"]);
    assert!(
        report
            .skipped
            .iter()
            .any(|skip| skip.profile == "union-bad-root"
                && skip.candidate == Path::new("/home")
                && skip.reason == "other_fs"),
        "{report:#?}"
    );
    assert!(exists(&reclaim_host, "/home/union-foreign"));
    assert_eq!(unlinked_below(&reclaim_host, "/home"), 0);
    assert_eq!(
        reclaim_host
            .trace()
            .iter()
            .filter(|call| call.path.as_deref() == Some(Path::new("/home/union-foreign")))
            .count(),
        0
    );
}

#[test]
fn filesystem_union_rechecks_each_idle_term_under_one_lock() {
    let tree = "  /tmp/union-clock: {.: {mtime: 2026-10-13T03:01:00Z}, f: {size: 1M, mtime: 2026-10-13T03:01:00Z}}\n";
    let selection =
        "(profile:union-recent && !filter:idle-2d) || (profile:union-old && filter:idle-1d)";
    let event =
        "events:\n  - {at: {op: flock, path: /tmp/union-clock, when: after}, advance: 120s}\n";
    let host = host(&yaml("", tree, event));
    let report = ensure(&host, &tmp(selection));
    assert_eq!(actions(&report), ["/tmp/union-clock"], "{report:#?}");
    assert!(!exists(&host, "/tmp/union-clock"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(
        host.trace_count(Op::Flock),
        2,
        "host lock plus one candidate lock"
    );
}

/// Root-mode owner IDs are an explicit input to the same modelled walk:
/// a process uid of zero does not silently admit root-owned or other-user
/// entries when `owners:` contains only the chosen non-system user.
#[test]
fn root_mode_reclaims_only_named_owners_tree() {
    let model = yaml(
        "",
        &format!(
            "  /tmp/pytest-of-allowed: {}\n  /tmp/pytest-of-other: {{.: {{uid: 1001}}, f: {{size: 1M, uid: 1001, mtime: {OLD}}}}}\n",
            old_dir("1M")
        ),
        "own_uid: 0\n",
    );
    let host = host(&model);
    assert_eq!(executor::privileges(&host, true), Ok(0));
    let request = tmp("profile:pytest-of-dir");
    let plan = executor::dry_run(&host, &request, &[1000], &mut |_| {});
    assert!(
        plan.plan
            .iter()
            .any(|item| item.candidate == Path::new("/tmp/pytest-of-allowed")
                && item.reason.is_none()),
        "{plan:?}"
    );
    assert!(
        plan.plan
            .iter()
            .any(|item| item.candidate == Path::new("/tmp/pytest-of-other")
                && item.reason == Some("not_owner")),
        "{plan:?}"
    );
    let report = executor::ensure(&host, &request, &[1000], &mut |_| {});
    assert!(exists(&host, "/tmp/pytest-of-other"));
    assert!(!exists(&host, "/tmp/pytest-of-allowed"), "{report:?}");
}

/// Alias roots are resolved safely, then scanned once by directory identity.
#[test]
fn same_filesystem_root_identity_is_walked_once() {
    let host = host(&yaml("", "  /tmp/x: {}\n", ""));
    let find = Find::new(&["x".to_owned()], EntryType::File, &Where::default(), 1000);
    let roots = [PathBuf::from("/tmp"), PathBuf::from("/tmp")];
    let (discovered, stopped) = fs_kind::discover(&host, &roots, &find, None);
    assert!(stopped.is_none(), "{stopped:?}");
    assert!(discovered.refused.is_empty(), "{:#?}", discovered.refused);
    assert_eq!(discovered.candidates.len(), 1);
    assert!(
        discovered.duplicates.is_empty(),
        "{:#?}",
        discovered.duplicates
    );
}

/// The actual `fs` deletion protocol, on 40,000 distinct top-level `/tmp`
/// candidates. This deliberately enters below the executor: the model's
/// `statvfs` recomputes all reachable inodes at every sample, which would
/// measure the model's quadratic accounting instead of scan cadence. The
/// filesystem omits nlink so its unrelated link-count walk is skipped. An
/// event advances the clock at the halfway unlink: the next candidate
/// must scan again at the exact 500 ms reuse boundary.
#[test]
fn forty_thousand_tmp_candidates_reclaim_once_per_reuse_window() {
    const CANDIDATES: usize = 40_000;
    let mut model = diskgc_model::load::parse_yaml(&yaml(
        "",
        "",
        "events:\n  - {at: {op: unlinkat, nth: 20000, when: after}, advance: 500ms}\n",
    ))
    .expect("base model");
    model
        .mounts
        .iter_mut()
        .find(|mount| mount.at == Path::new("/tmp"))
        .expect("/tmp mount")
        .statx_missing
        .push(StatxField::Nlink);
    for index in 0..CANDIDATES {
        model.tree.insert(
            PathBuf::from(format!("/tmp/entry-{index:05}")),
            NodeSpec::File(FileSpec::default()),
        );
    }
    let host = ModelHost::new(&model).expect("large model");
    let find = Find::new(
        &["entry-*".to_owned()],
        EntryType::File,
        &Where::default(),
        1000,
    );
    let (discovered, stopped) = fs_kind::discover(&host, &[PathBuf::from("/tmp")], &find, None);
    assert!(stopped.is_none(), "discovery stopped: {stopped:?}");
    assert!(discovered.refused.is_empty(), "{:#?}", discovered.refused);
    assert!(discovered.duplicates.is_empty());
    assert_eq!(discovered.candidates.len(), CANDIDATES);

    let owners = [1000];
    let rules = Rules::new(&owners);
    let judge = Judge {
        find: &find,
        rules: &rules,
        idle: diskgc_core::profile::IdleWindow::default(),
        dev: host.stat_path(Path::new("/tmp")).expect("/tmp").dev,
    };
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let started = std::time::Instant::now();
    for (path, candidate) in &discovered.candidates {
        let found = candidate
            .as_ref()
            .unwrap_or_else(|skip| panic!("{path:?}: {skip:?}"));
        let done = fs_kind::reclaim(&host, &judge, found, false, &mut checker, None);
        assert_eq!(done.end, ReclaimEnd::Complete, "{path:?}: {done:?}");
        assert_eq!(done.removed_entries, 1, "{path:?}");
    }
    eprintln!(
        "40,000 model /tmp candidates: {:?}, {} process scans",
        started.elapsed(),
        host.trace_count(Op::OwnPid)
    );
    assert_eq!(
        host.trace_count(Op::OwnPid),
        2,
        "one scan in each reuse window"
    );
    assert_eq!(host.trace_count(Op::Unlinkat), CANDIDATES);
    assert!(
        host.unfired_events().is_empty(),
        "the clock advanced halfway through"
    );
    assert!(!exists(&host, "/tmp/entry-00000"));
    assert!(!exists(&host, "/tmp/entry-20000"));
    assert!(!exists(&host, "/tmp/entry-39999"));
}

fn ensure(host: &ModelHost, request: &Request) -> Report {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::ensure(host, request, &[uid], &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

fn dry_run(host: &ModelHost, request: &Request) -> Report {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::dry_run(host, request, &[uid], &mut |l| lines.push(l.to_owned()));
    eprintln!("dry-run log:\n  {}", lines.join("\n  "));
    report
}

fn calls(host: &ModelHost, op: Op) -> Vec<Call> {
    host.trace().into_iter().filter(|c| c.op == op).collect()
}

fn touching(host: &ModelHost, below: &str) -> Vec<Call> {
    host.trace()
        .into_iter()
        .filter(|c| c.path.as_deref().is_some_and(|p| p.starts_with(below)))
        .collect()
}

fn unlinked_below(host: &ModelHost, below: &str) -> usize {
    calls(host, Op::Unlinkat)
        .iter()
        .filter(|c| c.path.as_deref().is_some_and(|p| p.starts_with(below)))
        .count()
}

fn exists(host: &ModelHost, path: &str) -> bool {
    host.stat_path(Path::new(path)).is_some()
}

#[test]
fn version_reclaim_keeps_linked_and_newest_files() {
    let tree = "  /home/mark/.local/share/claude/versions/1: {size: 100M, mtime: 2026-10-01T00:00:00Z}\n  /home/mark/.local/share/claude/versions/2: {size: 100M, mtime: 2026-10-02T00:00:00Z}\n  /home/mark/.local/share/claude/versions/3: {size: 100M, mtime: 2026-10-03T00:00:00Z}\n  /home/mark/.local/share/claude/versions/4: {size: 100M, mtime: 2026-10-04T00:00:00Z}\n  /home/mark/.local/bin/claude: {symlink: ../share/claude/versions/1}\n  /home/mark/bin/.keep: {size: 1K}\n";
    let host = host(&yaml("", tree, ""));
    let roots = ["/home/mark".to_owned()];
    let library = Library::builtin().expect("built-ins");
    let request = Request {
        path: PathBuf::from("/home/mark"),
        min_free: parse_size("100%").expect("size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select("profile:claude-code-versions", &library, Some(&roots), 1000)
            .expect("version tier"),
    };
    let report = ensure(&host, &request);
    assert_eq!(
        actions(&report),
        ["/home/mark/.local/share/claude/versions/2"]
    );
    assert!(exists(&host, "/home/mark/.local/share/claude/versions/1"));
    assert!(exists(&host, "/home/mark/.local/share/claude/versions/3"));
    assert!(exists(&host, "/home/mark/.local/share/claude/versions/4"));
    assert!(!exists(&host, "/home/mark/.local/share/claude/versions/2"));
    assert_eq!(
        unlinked_below(&host, "/home/mark/.local/share/claude/versions/1"),
        0
    );
    assert!(
        host.trace_count(Op::ReadLink) >= 2,
        "link checked during tally and reclaim"
    );
}

fn actions(report: &Report) -> Vec<String> {
    report
        .actions
        .iter()
        .map(|a| a.candidate.display().to_string())
        .collect()
}

fn skip_of(report: &Report, candidate: &str) -> Option<&'static str> {
    report
        .skipped
        .iter()
        .find(|s| s.candidate == Path::new(candidate))
        .map(|s| s.reason)
}

/// The fixture of the main scenario: three eligible candidates (one a
/// level down), and one of each refusal the protocol makes.
const MIXED: &str = "  /tmp/pytest-of-a: OLD_10K_05
  /tmp/pytest-of-b: OLD_20K
  /tmp/nix-shell.X/pytest-of-c: OLD_10K_03
  /tmp/pytest-of-fresh: {f: {size: 10K}}
  /tmp/pytest-of-kept: {.: {mtime: 2026-10-01T00:00:00Z}, .diskgc-keep: {mtime: 2026-10-01T00:00:00Z}}
  /tmp/nix-shell.K/.diskgc-keep: {mtime: 2026-10-01T00:00:00Z}
  /tmp/nix-shell.K/pytest-of-d: OLD_20K
  /tmp/pytest-of-other: {.: {mtime: 2026-10-01T00:00:00Z}, f: {uid: 1001, mtime: 2026-10-01T00:00:00Z}}
  /tmp/pytest-of-sock: {.: {mtime: 2026-10-01T00:00:00Z}, s: {type: socket, mtime: 2026-10-01T00:00:00Z}}
  /tmp/pytest-of-fifo: {.: {mtime: 2026-10-01T00:00:00Z}, p: {type: fifo, mtime: 2026-10-01T00:00:00Z}}
  /tmp/pytest-of-git: {.: {mtime: 2026-10-01T00:00:00Z}, .git: {HEAD: {mtime: 2026-10-01T00:00:00Z}}}
  /tmp/pytest-of-used: OLD_20K
  /tmp/pytest-of-file: {mtime: 2026-10-01T00:00:00Z}
  /tmp/unrelated: OLD_20K
";

fn mixed() -> String {
    let at = |day: &str| {
        format!(
            "{{.: {{mtime: 2026-10-{day}T00:00:00Z}}, f: {{size: 10K, mtime: 2026-10-{day}T00:00:00Z}}}}"
        )
    };
    yaml(
        "",
        &MIXED
            .replace("OLD_10K_05", &at("05"))
            .replace("OLD_10K_03", &at("03"))
            .replace("OLD_20K", &old_dir("20K")),
        "  - {pid: 8, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{path: /tmp/pytest-of-used/f}], environ: [PATH=/usr/bin]}\n",
    )
}

/// AC #1 in the model: every eligible candidate is deleted, oldest first,
/// and nothing else; each refused one keeps every object, with its reason.
/// The dry run of the same tree predicts it: its eligible entries are the
/// reclaim's actions, in the same order, and its skips the same reasons.
#[test]
fn an_fs_reclaim_deletes_every_eligible_candidate_oldest_first_and_nothing_else() {
    let select = "profile:pytest-of-dir && filter:idle-1d";
    let planned = dry_run(&host(&mixed()), &tmp(select));
    let host = host(&mixed());
    let report = ensure(&host, &tmp(select));
    assert_eq!(report.status, Status::Exhausted, "{report:#?}");
    assert_eq!(report.exit_code(), 1);
    let eligible = [
        "/tmp/pytest-of-b",
        "/tmp/nix-shell.X/pytest-of-c",
        "/tmp/pytest-of-a",
    ];
    assert_eq!(actions(&report), eligible, "{report:#?}");
    for action in &report.actions {
        assert!(
            action.avail_delta_bytes.is_some_and(|d| d > 0),
            "measured, and space came back: {action:?}"
        );
    }
    for gone in eligible {
        assert!(!exists(&host, gone), "{gone} is still there");
    }
    let refused = [
        ("/tmp/pytest-of-fresh", "not_idle"),
        ("/tmp/pytest-of-kept", "kept"),
        ("/tmp/nix-shell.K/pytest-of-d", "kept"),
        ("/tmp/pytest-of-other", "not_owner"),
        ("/tmp/pytest-of-sock", "contains_ipc"),
        ("/tmp/pytest-of-fifo", "contains_ipc"),
        ("/tmp/pytest-of-git", "contains_checkout"),
        ("/tmp/pytest-of-used", "in_use"),
    ];
    for (candidate, reason) in refused {
        assert_eq!(skip_of(&report, candidate), Some(reason), "{candidate}");
        assert_eq!(unlinked_below(&host, candidate), 0, "{candidate}");
    }
    for kept in [
        "/tmp/pytest-of-fresh/f",
        "/tmp/pytest-of-kept/.diskgc-keep",
        "/tmp/nix-shell.K/pytest-of-d/f",
        "/tmp/pytest-of-other/f",
        "/tmp/pytest-of-sock/s",
        "/tmp/pytest-of-fifo/p",
        "/tmp/pytest-of-git/.git/HEAD",
        "/tmp/pytest-of-used/f",
        "/tmp/pytest-of-file",
        "/tmp/unrelated/f",
        "/tmp/nix-shell.X",
    ] {
        assert!(exists(&host, kept), "{kept} was deleted");
    }
    // Every unlink is inside an eligible candidate (or is it).
    for call in calls(&host, Op::Unlinkat) {
        let path = call.path.expect("an unlinkat names its entry");
        assert!(
            eligible.iter().any(|e| path.starts_with(e)),
            "unlinked outside the eligible set: {}",
            path.display()
        );
    }
    // Each eligible directory was flocked before its first unlink.
    let trace = host.trace();
    for candidate in eligible {
        let first_unlink = trace
            .iter()
            .position(|c| {
                c.op == Op::Unlinkat && c.path.as_deref().is_some_and(|p| p.starts_with(candidate))
            })
            .expect("deleted");
        assert!(
            trace[..first_unlink]
                .iter()
                .any(|c| c.op == Op::Flock && c.path.as_deref() == Some(Path::new(candidate))),
            "{candidate} was not flocked before its delete"
        );
    }
    // The plan predicted it.
    let plan_eligible: Vec<String> = planned
        .plan
        .iter()
        .filter(|p| p.reason.is_none())
        .map(|p| p.candidate.display().to_string())
        .collect();
    assert_eq!(plan_eligible, actions(&report));
    for (candidate, reason) in refused {
        let planned_reason = planned
            .plan
            .iter()
            .find(|p| p.candidate == Path::new(candidate))
            .and_then(|p| p.reason);
        assert_eq!(planned_reason, Some(reason), "{candidate} in the plan");
    }
}

/// The reclaim stops once the goal is observed: the oldest candidate frees
/// enough, the others stay. Exit 0, the space measured around the action.
#[test]
fn the_fs_reclaim_stops_once_the_goal_is_observed() {
    let tree = format!(
        "  /tmp/pytest-of-a: {}\n  /tmp/pytest-of-b: {{.: {{mtime: 2026-09-01T00:00:00Z}}, f: \
         {{size: 300M, mtime: 2026-09-01T00:00:00Z}}}}\n  /tmp/pytest-of-c: {}\n",
        old_dir("100M"),
        old_dir("50M")
    );
    let host = host(&yaml("", &tree, ""));
    // 1G - 500M - 450M = 74M (in MiB terms: below 300M).
    let report = ensure(
        &host,
        &request("profile:pytest-of-dir", &[], "/tmp", "300M"),
    );
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(actions(&report), ["/tmp/pytest-of-b"]);
    let delta = report.actions[0].avail_delta_bytes.expect("measured");
    assert!(delta >= 300 << 20, "{delta}");
    assert!(exists(&host, "/tmp/pytest-of-a/f") && exists(&host, "/tmp/pytest-of-c/f"));
}

/// AC #2: a file created in a candidate after its walk is never unlinked;
/// its directory fails with `ENOTEMPTY`, which is an error for that
/// candidate (with an action for what was deleted before), never a silent
/// retry: the directory's `rmdir` is tried once, the candidate itself not
/// at all, and the next candidate is still reclaimed. Exit 3.
#[test]
fn a_file_created_mid_delete_is_an_enotempty_error_never_retried() {
    let tree = format!(
        "  /tmp/pytest-of-b:\n    .: {{mtime: 2026-09-01T00:00:00Z}}\n    sub: {{.: {{mtime: \
         {OLD}}}, x: {{size: 10K, mtime: {OLD}}}}}\n  /tmp/pytest-of-c: {}\n",
        old_dir("10K")
    );
    let events = "events:\n  - {at: {op: unlinkat, path: /tmp/pytest-of-b/sub/x, when: after}, \
                  create: /tmp/pytest-of-b/sub/new}\n";
    let host = host(&yaml("", &tree, events));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    let errors: Vec<_> = report
        .errors
        .iter()
        .filter(|e| e.candidate == "/tmp/pytest-of-b")
        .collect();
    assert_eq!(errors.len(), 1, "{:#?}", report.errors);
    assert!(
        errors[0].message.contains("ENOTEMPTY"),
        "{}",
        errors[0].message
    );
    assert!(
        actions(&report).contains(&"/tmp/pytest-of-b".to_owned()),
        "deleted part-way: an action too"
    );
    assert!(
        exists(&host, "/tmp/pytest-of-b/sub/new"),
        "the new file stays"
    );
    let rmdirs = |path: &str| {
        calls(&host, Op::Unlinkat)
            .iter()
            .filter(|c| c.path.as_deref() == Some(Path::new(path)))
            .count()
    };
    assert_eq!(rmdirs("/tmp/pytest-of-b/sub"), 1, "tried once, never again");
    assert_eq!(rmdirs("/tmp/pytest-of-b"), 0);
    assert!(
        !exists(&host, "/tmp/pytest-of-c"),
        "the next one is reclaimed"
    );
}

/// AC #3: systemd's PrivateTmp entries are never candidates, nor entered:
/// not by a pattern that matches their name, not as a root (a root inside
/// one is a misconfiguration, reported as an error). Nothing at or below
/// them is even `statx`ed; the other old entries go.
#[test]
fn systemd_private_entries_are_never_candidates() {
    let private = "/tmp/systemd-private-abc-cups.service-x1";
    let tree = format!(
        "  {private}:\n    .: {{mtime: {OLD}}}\n    tmp: {{.: {{mtime: {OLD}}}, f: {{size: 10K, \
         mtime: {OLD}}}}}\n  /tmp/old: {}\n",
        old_dir("10K")
    );
    let host = host(&yaml("", &tree, ""));
    let report = ensure(&host, &tmp("profile:tmp-entry && filter:idle-1d"));
    assert_eq!(actions(&report), ["/tmp/old"], "{report:#?}");
    assert!(
        touching(&host, private).is_empty(),
        "{:#?}",
        touching(&host, private)
    );
    assert!(exists(&host, &format!("{private}/tmp/f")));

    let host = common::host(&yaml("", &tree, ""));
    let report = ensure(
        &host,
        &request(
            "profile:anything",
            &[&format!("{private}/tmp")],
            "/tmp",
            "100%",
        ),
    );
    assert!(report.actions.is_empty(), "{report:#?}");
    // Reported as a misconfigured root: an error that excludes the tier
    // (exit 3), never only a log line.
    assert_eq!(report.exit_code(), 3, "{report:#?}");
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.message.contains("PrivateTmp")),
        "{:#?}",
        report.errors
    );
    assert!(
        touching(&host, private).is_empty(),
        "{:#?}",
        touching(&host, private)
    );
}

/// A broad profile may select the parent of PrivateTmp. The eligibility
/// walk must refuse the parent before statting the protected child's name;
/// its owner matching the caller is no excuse to enter it.
#[test]
fn a_plain_delete_candidate_containing_private_tmp_is_kept() {
    let private = "/tmp/old/systemd-private-abc-cups.service-x1";
    let tree = format!(
        "  {private}/tmp/f: {{size: 10K, mtime: {OLD}}}\n  /tmp/good: {}\n",
        old_dir("10K")
    );
    let host = host(&yaml("", &tree, ""));
    let report = ensure(&host, &tmp("profile:tmp-entry"));
    assert_eq!(actions(&report), ["/tmp/good"], "{report:#?}");
    assert_eq!(skip_of(&report, "/tmp/old"), Some("contains_special"));
    assert!(exists(&host, &format!("{private}/tmp/f")));
    assert!(
        touching(&host, private).is_empty(),
        "{:#?}",
        touching(&host, private)
    );
}

/// AC #5 (model): a candidate on a network filesystem is never deleted. A
/// root on one is excluded during preflight from its mountinfo line before
/// anything there is opened or `statx`ed, whatever `PATH` is; a network
/// mount at a candidate's name or inside one is `contains_mount`, never
/// touched.
#[test]
fn a_network_filesystem_is_never_reclaimed_or_touched() {
    for fs in [
        "nfs",
        "nfs4",
        "cifs",
        "smb3",
        "fuse.sshfs",
        "9p",
        "ceph",
        "fuse.ceph-fuse",
        "fuseblk.ntfs-3g",
        "fuse.fuse-overlayfs",
        "virtiofs",
    ] {
        let mounts = format!(
            "  - {{id: 40, at: /mnt/net, fs: {fs}, dev: \"0:50\", size: 10G, other_used: 9G, \
             parent: 1}}\n"
        );
        let tree = format!("  /mnt/net/scratch/old: {}\n", old_dir("1G"));
        let host = host(&yaml(&mounts, &tree, ""));
        let report = ensure(
            &host,
            &request(
                "profile:anything",
                &["/mnt/net/scratch"],
                "/mnt/net",
                "100%",
            ),
        );
        assert_eq!(report.status, Status::Error, "{fs}: {report:#?}");
        assert_eq!(report.exit_code(), 3, "{fs}: {report:#?}");
        assert!(report.actions.is_empty(), "{fs}: {report:#?}");
        assert!(report.skipped.is_empty(), "{fs}: {report:#?}");
        assert!(
            report.errors.iter().any(|error| {
                error.profile == "anything"
                    && error.candidate == "/mnt/net/scratch"
                    && error.message.contains(&format!("on {fs} mount /mnt/net"))
                    && error.message.contains("the tier is excluded")
            }),
            "{fs}: {report:#?}"
        );
        assert!(
            touching(&host, "/mnt/net/scratch").is_empty(),
            "{fs}: {:#?}",
            touching(&host, "/mnt/net/scratch")
        );
        assert!(exists(&host, "/mnt/net/scratch/old/f"), "{fs}");
        assert_eq!(unlinked_below(&host, "/mnt/net/scratch"), 0, "{fs}");
    }
    // In /tmp: a network mount as a candidate, and one inside a candidate.
    let mounts = "  - {id: 41, at: /tmp/pytest-of-net, fs: fuse.sshfs, dev: \"0:51\", size: 1G, \
                  parent: 30}\n  - {id: 42, at: /tmp/pytest-of-holder/mnt, fs: nfs4, dev: \
                  \"0:52\", size: 1G, parent: 30}\n";
    let tree = format!(
        "  /tmp/pytest-of-net/f: {{size: 1M, mtime: {OLD}}}\n  /tmp/pytest-of-holder: {}\n  \
         /tmp/pytest-of-holder/mnt/g: {{size: 1M, mtime: {OLD}}}\n  /tmp/pytest-of-a: {}\n",
        old_dir("1M"),
        old_dir("1M")
    );
    let host = host(&yaml(mounts, &tree, ""));
    let report = ensure(&host, &tmp("profile:pytest-of-dir && filter:idle-1d"));
    assert_eq!(actions(&report), ["/tmp/pytest-of-a"], "{report:#?}");
    assert_eq!(
        skip_of(&report, "/tmp/pytest-of-net"),
        Some("contains_mount")
    );
    assert_eq!(
        skip_of(&report, "/tmp/pytest-of-holder"),
        Some("contains_mount")
    );
    for mount in ["/tmp/pytest-of-net", "/tmp/pytest-of-holder/mnt"] {
        assert!(
            touching(&host, mount).is_empty(),
            "{:#?}",
            touching(&host, mount)
        );
    }
    assert!(exists(&host, "/tmp/pytest-of-holder/f"));
}

/// A FUSE filesystem is refused as possibly networked even when its type
/// names a client often used with local storage: its backing data can
/// still reside on another host.
#[test]
fn every_fuse_filesystem_is_refused() {
    let reclaim_on = |fs: &str| {
        let mounts = format!(
            "  - {{id: 40, at: /mnt/f, fs: {fs}, dev: \"0:50\", size: 10G, other_used: 9G, \
             parent: 1}}\n"
        );
        let tree = format!("  /mnt/f/scratch/old: {}\n", old_dir("1G"));
        let host = host(&yaml(&mounts, &tree, ""));
        let report = ensure(
            &host,
            &request("profile:anything", &["/mnt/f/scratch"], "/mnt/f", "100%"),
        );
        (host, report)
    };
    for fs in [
        "fuse.never-heard-of-it",
        "fuse",
        "fuseblk",
        "fuseblk.ntfs-3g",
        "fuse.portal",
        "fuse.fuse-overlayfs",
    ] {
        let (host, report) = reclaim_on(fs);
        assert_eq!(report.status, Status::Error, "{fs}: {report:#?}");
        assert_eq!(report.exit_code(), 3, "{fs}: {report:#?}");
        assert!(report.actions.is_empty(), "{fs}: {report:#?}");
        assert!(report.skipped.is_empty(), "{fs}: {report:#?}");
        assert!(
            report.errors.iter().any(|error| {
                error.profile == "anything"
                    && error.candidate == "/mnt/f/scratch"
                    && error.message.contains(&format!("on {fs} mount /mnt/f"))
                    && error.message.contains("the tier is excluded")
            }),
            "{fs}: {report:#?}"
        );
        assert!(
            touching(&host, "/mnt/f/scratch").is_empty(),
            "{fs}: {:#?}",
            touching(&host, "/mnt/f/scratch")
        );
        assert!(exists(&host, "/mnt/f/scratch/old/f"), "{fs}");
        assert_eq!(unlinked_below(&host, "/mnt/f/scratch"), 0, "{fs}");
    }
}

/// Every check is made again under the candidate's lock (docs/safety.md,
/// Deletion, step 4): a change made right after the `flock` is seen. A
/// keep marker put between the root and the candidate (`kept`), the
/// source's `has` file removed (`changed`), an entry given to another user
/// (`not_owner`), a new file making it recent (`not_idle`): nothing of it
/// is deleted. (A socket created then is the same walk's `contains_ipc`;
/// the model's events create regular files only.)
#[test]
fn every_check_is_made_again_under_the_candidates_lock() {
    let flocked = |path: &str| format!("{{op: flock, path: {path}, when: after}}");
    let cases: [(&str, String, String, &str, &str); 5] = [
        (
            "profile:pytest-of-dir",
            "/tmp/nix-shell.X/pytest-of-c".to_owned(),
            format!(
                "  - {{at: {}, create: /tmp/nix-shell.X/.diskgc-keep}}\n",
                flocked("/tmp/nix-shell.X/pytest-of-c")
            ),
            "/tmp/nix-shell.X/pytest-of-c",
            "kept",
        ),
        (
            "profile:marker-build-fixture",
            "/home/mark/src/build".to_owned(),
            format!(
                "  - {{at: {}, remove: /home/mark/src/build/CMakeCache.txt}}\n",
                flocked("/home/mark/src/build")
            ),
            "/home/mark/src/build",
            "changed",
        ),
        (
            "profile:marker-build-fixture",
            "/home/mark/src/build".to_owned(),
            format!(
                "  - {{at: {}, create: /home/mark/src/build/CMakeLists.txt}}\n",
                flocked("/home/mark/src/build")
            ),
            "/home/mark/src/build",
            "changed",
        ),
        (
            "profile:pytest-of-dir",
            "/tmp/pytest-of-b".to_owned(),
            format!(
                "  - {{at: {}, chown: {{path: /tmp/pytest-of-b/f, uid: 1001}}}}\n",
                flocked("/tmp/pytest-of-b")
            ),
            "/tmp/pytest-of-b",
            "not_owner",
        ),
        (
            "profile:pytest-of-dir",
            "/tmp/pytest-of-b".to_owned(),
            format!(
                "  - {{at: {}, create: /tmp/pytest-of-b/new}}\n",
                flocked("/tmp/pytest-of-b")
            ),
            "/tmp/pytest-of-b",
            "not_idle",
        ),
    ];
    for (select, candidate, event, skipped, reason) in cases {
        let tree = format!(
            "  /tmp/pytest-of-b: {old}\n  /tmp/nix-shell.X/pytest-of-c: {old}\n  \
             /home/mark/src/build:\n    .: {{mtime: {OLD}}}\n    CMakeCache.txt: {{mtime: \
             {OLD}}}\n    obj: {{size: 1M, mtime: {OLD}}}\n",
            old = old_dir("10K")
        );
        let host = host(&yaml("", &tree, &format!("events:\n{event}")));
        let select = format!("{select} && filter:idle-1d");
        let roots: &[&str] = if select.contains("marker-build-fixture") {
            &["/home/mark/src"]
        } else {
            &[]
        };
        let path = if select.contains("marker-build-fixture") {
            "/home/mark"
        } else {
            "/tmp"
        };
        let report = ensure(&host, &request(&select, roots, path, "100%"));
        assert!(host.unfired_events().is_empty(), "{candidate}: {event}");
        assert_eq!(
            skip_of(&report, skipped),
            Some(reason),
            "{candidate}: {event}: {report:#?}"
        );
        assert_eq!(
            unlinked_below(&host, &candidate),
            0,
            "{candidate}: {event}: {report:#?}"
        );
    }
}

/// A Zephyr marker removed after the candidate lock prevents deletion. The
/// `has_dir` check is part of the source definition, so reclaim must repeat
/// it under the lock just as it repeats the CMakeCache file check.
#[test]
fn anchored_west_build_directory_marker_is_rechecked_under_lock() {
    let candidate = "/home/mark/src/app/build";
    let tree = format!(
        "  /home/mark/src/app/CMakeLists.txt: {{mtime: {OLD}}}\n  {candidate}: \
         {{.: {{mtime: {OLD}}}, CMakeCache.txt: {{content: \
         \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/src/app\\n\", mtime: {OLD}}}, \
         zephyr: {{.: {{mtime: {OLD}}}}}}}\n"
    );
    let event = format!(
        "events:\n  - {{at: {{op: flock, path: {candidate}, when: after}}, remove: {candidate}/zephyr}}\n"
    );
    let host = host(&yaml("", &tree, &event));
    let roots = ["/home/mark/src".to_owned()];
    let library = Library::builtin().expect("built-ins");
    let request = Request {
        path: PathBuf::from("/home/mark"),
        min_free: parse_size("100%").expect("size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select("profile:west-build-anchored", &library, Some(&roots), 1000)
            .expect("west-build-anchored tier"),
    };
    let report = ensure(&host, &request);
    assert!(host.unfired_events().is_empty(), "{report:#?}");
    assert_eq!(skip_of(&report, candidate), Some("changed"), "{report:#?}");
    assert_eq!(unlinked_below(&host, candidate), 0, "{report:#?}");
    assert!(exists(&host, &format!("{candidate}/CMakeCache.txt")));
}

/// Losing or replacing the recorded source file after discovery and tally
/// invalidates the evidence under the candidate lock. No output is unlinked.
#[test]
fn anchored_cmake_source_identity_is_rechecked_under_lock() {
    let candidate = "/home/mark/src/app/arbitrary-name";
    let source_file = "/home/mark/src/app/CMakeLists.txt";
    for action in ["remove", "replace"] {
        let tree = format!(
            "  {source_file}: {{mtime: {OLD}}}\n  {candidate}: {{.: {{mtime: {OLD}}}, \
             CMakeCache.txt: {{content: \
             \"CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/src/app\\n\", mtime: {OLD}}}, \
             CMakeFiles: {{.: {{mtime: {OLD}}}}}, object.o: {{size: 1M, mtime: {OLD}}}}}\n"
        );
        let event = format!(
            "events:\n  - {{at: {{op: flock, path: {candidate}, when: after}}, {action}: {source_file}}}\n"
        );
        let host = host(&yaml("", &tree, &event));
        let roots = ["/home/mark/src".to_owned()];
        let library = Library::builtin().expect("built-ins");
        let request = Request {
            path: PathBuf::from("/home/mark"),
            min_free: parse_size("100%").expect("size"),
            target_free: None,
            timeout: Some(Duration::from_secs(600)),
            tally_budget: None,
            tier: Tier::select("profile:cmake-build-anchored", &library, Some(&roots), 1000)
                .expect("anchored CMake tier"),
        };
        let report = ensure(&host, &request);
        assert!(host.unfired_events().is_empty(), "{action}: {report:#?}");
        assert_eq!(
            skip_of(&report, candidate),
            Some("changed"),
            "{action}: {report:#?}"
        );
        assert_eq!(unlinked_below(&host, candidate), 0, "{action}: {report:#?}");
        assert!(exists(&host, &format!("{candidate}/object.o")));
    }
}

/// The source check reads the locked directory, even if its old path now
/// names a different, equally marked build. The later walk refuses the
/// replacement by identity; neither tree is deleted.
#[test]
fn anchored_cmake_reads_the_locked_candidate_after_its_path_is_swapped() {
    let candidate = "/home/mark/src/app/output";
    let moved = "/home/mark/src/app/moved";
    let replacement = "/home/mark/replacement";
    let cache = "CMAKE_HOME_DIRECTORY:INTERNAL=/home/mark/src/app\\n";
    let tree = format!(
        "  /home/mark/src/app/CMakeLists.txt: {{mtime: {OLD}}}\n  \
         {candidate}: {{.: {{mtime: {OLD}}}, CMakeCache.txt: {{content: \
         \"{cache}\", mtime: {OLD}}}, CMakeFiles: {{.: {{mtime: {OLD}}}}}, \
         original.o: {{mtime: {OLD}}}}}\n  \
         {replacement}: {{.: {{mtime: {OLD}}}, CMakeCache.txt: {{content: \
         \"{cache}\", mtime: {OLD}}}, CMakeFiles: {{.: {{mtime: {OLD}}}}}, \
         precious: {{mtime: {OLD}}}}}\n"
    );
    let events = format!(
        "events:\n  - {{at: {{op: flock, path: {candidate}, when: after}}, \
         rename: {{from: {candidate}, to: {moved}}}}}\n  - \
         {{at: {{op: flock, path: {candidate}, when: after}}, \
         rename: {{from: {replacement}, to: {candidate}}}}}\n"
    );
    let host = host(&yaml("", &tree, &events));
    let roots = ["/home/mark/src".to_owned()];
    let library = Library::builtin().expect("built-ins");
    let request = Request {
        path: PathBuf::from("/home/mark"),
        min_free: parse_size("100%").expect("size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select("profile:cmake-build-anchored", &library, Some(&roots), 1000)
            .expect("anchored CMake tier"),
    };
    let report = ensure(&host, &request);
    assert!(host.unfired_events().is_empty(), "{report:#?}");
    assert_eq!(skip_of(&report, candidate), Some("changed"), "{report:#?}");
    assert!(exists(&host, &format!("{moved}/original.o")));
    assert!(exists(&host, &format!("{candidate}/precious")));
    assert_eq!(unlinked_below(&host, moved), 0);
    assert_eq!(unlinked_below(&host, candidate), 0);

    let trace = host.trace();
    let flock = trace
        .iter()
        .find(|call| call.op == Op::Flock && call.path.as_deref() == Some(Path::new(candidate)))
        .expect("candidate flock");
    let original_cache = trace
        .iter()
        .find(|call| {
            call.op == Op::ReadHead
                && call.index < flock.index
                && call.path.as_deref()
                    == Some(Path::new("/home/mark/src/app/output/CMakeCache.txt"))
        })
        .and_then(|call| call.object)
        .expect("cache read during discovery or tally");
    assert!(
        trace.iter().any(|call| {
            call.op == Op::ReadHead
                && call.index > flock.index
                && call.object == Some(original_cache)
        }),
        "source check did not read CMakeCache.txt through the locked candidate fd: {trace:#?}"
    );
}

/// The candidate must be the object discovery found, at every step that
/// reaches it again: one replaced between the tally and its lock (a new
/// object at the same name, here holding someone's file), before or after
/// the lock's open, is `changed`, and neither the replacement nor what was
/// moved away is deleted. The tier has no idle filter, so only the
/// identity checks stand between them.
#[test]
fn a_candidate_replaced_after_discovery_is_changed_and_nothing_is_deleted() {
    let tree = format!("  /tmp/pytest-of-b: {}\n", old_dir("10K"));
    let at = "{op: flock, path: /tmp/pytest-of-b, when: before}";
    for (name, events) in [
        (
            "renamed away and recreated",
            format!(
                "events:\n  - {{at: {at}, rename: {{from: /tmp/pytest-of-b, to: /tmp/moved}}}}\n  \
                 - {{at: {at}, mkdir: /tmp/pytest-of-b}}\n  - {{at: {at}, create: \
                 /tmp/pytest-of-b/precious}}\n"
            ),
        ),
        (
            "replaced",
            format!("events:\n  - {{at: {at}, replace: /tmp/pytest-of-b}}\n"),
        ),
        // Before the lock's open (the second open of the candidate, after
        // the tally's walk): the lock and the walk then both see the
        // replacement and agree with each other; only the comparison with
        // what discovery found tells.
        (
            "replaced before the lock's open",
            "events:\n  - {at: {op: openat2, path: /tmp/pytest-of-b, nth: 2, when: before}, \
             rename: {from: /tmp/pytest-of-b, to: /tmp/moved}}\n  - {at: {op: openat2, path: \
             /tmp/pytest-of-b, nth: 2, when: before}, mkdir: /tmp/pytest-of-b}\n  - {at: {op: \
             openat2, path: /tmp/pytest-of-b, nth: 2, when: before}, create: \
             /tmp/pytest-of-b/precious}\n"
                .to_owned(),
        ),
    ] {
        let host = host(&yaml("", &tree, &events));
        let report = ensure(&host, &tmp("profile:pytest-of-dir"));
        assert!(host.unfired_events().is_empty(), "{name}");
        assert_eq!(
            skip_of(&report, "/tmp/pytest-of-b"),
            Some("changed"),
            "{name}: {report:#?}"
        );
        assert!(report.actions.is_empty(), "{name}: {report:#?}");
        assert!(exists(&host, "/tmp/pytest-of-b"), "{name}: the replacement");
        assert_eq!(calls(&host, Op::Unlinkat).len(), 0, "{name}");
    }
}

/// The in-use check is fresh at every candidate (docs/safety.md, In use:
/// Freshness): a process that opens a file of the second candidate after
/// the first one's scan, with five seconds passing in between, keeps it
/// (`in_use`). A scan from earlier in the run is never reused for it.
#[test]
fn a_process_that_opens_a_candidate_after_an_earlier_scan_keeps_it() {
    let tree = format!(
        "  /tmp/pytest-of-a: {}\n  /tmp/pytest-of-b: {{.: {{mtime: 2026-09-01T00:00:00Z}}, f: \
         {{size: 10K, mtime: 2026-09-01T00:00:00Z}}}}\n",
        old_dir("10K")
    );
    let at = "{op: flock, path: /tmp/pytest-of-a, when: after}";
    let rest = format!(
        "  - {{pid: 9, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{{path: \
         /usr/bin/tool}}], environ: [PATH=/usr/bin]}}\nevents:\n  - {{at: {at}, advance: 5s}}\n  \
         - {{at: {at}, open: {{pid: 9, path: /tmp/pytest-of-a/f}}}}\n"
    );
    let host = host(&yaml("", &tree, &rest));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(actions(&report), ["/tmp/pytest-of-b"], "{report:#?}");
    assert_eq!(skip_of(&report, "/tmp/pytest-of-a"), Some("in_use"));
    assert!(exists(&host, "/tmp/pytest-of-a/f"));
}

/// A scan that blocks every candidate (an unreadable process of the user)
/// is reused only within the short blocked-scan window. Here pid 9 exits
/// after blocking the first candidate, and the next candidate is judged
/// at the 500 ms boundary against a new scan.
#[test]
fn a_blocking_scan_is_replaced_at_the_short_limit() {
    let tree = format!(
        "  /tmp/pytest-of-a: {}\n  /tmp/pytest-of-b: {{.: {{mtime: 2026-09-01T00:00:00Z}}, f: \
         {{size: 10K, mtime: 2026-09-01T00:00:00Z}}}}\n",
        old_dir("10K")
    );
    let rest = "  - {pid: 9, exe: /usr/bin/tool, proc: failed}\nevents:\n  - {at: {op: flock, \
                path: /tmp/pytest-of-a, when: after}, exit: 9}\n  - {at: {op: flock, \
                path: /tmp/pytest-of-a, when: after}, advance: 500ms}\n";
    let host = host(&yaml("", &tree, rest));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(skip_of(&report, "/tmp/pytest-of-b"), Some("in_use"));
    assert_eq!(actions(&report), ["/tmp/pytest-of-a"], "{report:#?}");
}

#[test]
fn an_unreadable_process_without_a_name_has_null_command_on_the_wire() {
    let tree = format!("  /tmp/pytest-of-a: {}\n", old_dir("10K"));
    let host = host(&yaml(
        "",
        &tree,
        "  - {pid: 9, exe: /usr/bin/tool, name: '', proc: failed}\n",
    ));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert_eq!(skip_of(&report, "/tmp/pytest-of-a"), Some("in_use"));
    let wire: serde_json::Value = serde_json::from_str(&report.to_json()).expect("report JSON");
    assert_eq!(
        wire["skipped"][0]["process_blockers"],
        serde_json::json!([{"pid": 9, "command": null, "evidence": "unreadable"}])
    );
}

/// A blocking scan that runs past the deadline is a stopped run, even when
/// this is the final candidate and there is no next loop iteration to check.
#[test]
fn a_blocking_scan_past_the_deadline_stops_the_reclaim() {
    let tree = format!("  /tmp/pytest-of-a: {}\n", old_dir("10K"));
    let rest = "  - {pid: 9, exe: /usr/bin/tool, proc: failed}\nevents:\n  - {at: {op: own_pid, nth: 1, when: after}, advance: 700s}\n";
    let host = host(&yaml("", &tree, rest));
    let find = Find::new(
        &["pytest-of-*".to_owned()],
        EntryType::Dir,
        &Where::default(),
        1000,
    );
    let (discovered, stopped) = fs_kind::discover(&host, &[PathBuf::from("/tmp")], &find, None);
    assert!(stopped.is_none());
    let found = discovered.candidates[0].1.as_ref().expect("candidate");
    let owners = [1000];
    let rules = Rules::new(&owners);
    let judge = Judge {
        find: &find,
        rules: &rules,
        idle: diskgc_core::profile::IdleWindow::default(),
        dev: host.stat_path(Path::new("/tmp")).expect("/tmp").dev,
    };
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let deadline = host.now().checked_add(Duration::from_secs(600));
    let done = fs_kind::reclaim(&host, &judge, found, false, &mut checker, deadline);
    assert_eq!(
        done.end,
        ReclaimEnd::Stopped(Stop::DeadlinePassed {
            path: PathBuf::from("/tmp/pytest-of-a")
        })
    );
    assert!(host.unfired_events().is_empty());
    assert_eq!(unlinked_below(&host, "/tmp/pytest-of-a"), 0);
}

/// A candidate directory `flock`ed by another process is `locked` and
/// never walked or deleted.
#[test]
fn a_flocked_candidate_is_locked() {
    let tree = format!("  /tmp/pytest-of-b: {}\n", old_dir("10K"));
    let rest = "  - {pid: 9, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{path: \
                /usr/bin/tool}], environ: [PATH=/usr/bin]}\nlocks:\n  - {path: /tmp/pytest-of-b, \
                holder: 9, mode: shared}\n";
    let host = host(&yaml("", &tree, rest));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert_eq!(
        skip_of(&report, "/tmp/pytest-of-b"),
        Some("locked"),
        "{report:#?}"
    );
    assert_eq!(unlinked_below(&host, "/tmp/pytest-of-b"), 0);
}

/// The deadline stops the reclaim inside a candidate's delete (between two
/// unlinks) and before the next candidate: exit 124, the first one an
/// action part-way, the second untouched.
#[test]
fn the_deadline_stops_the_reclaim_inside_and_between_candidates() {
    let tree = format!(
        "  /tmp/pytest-of-a: {{.: {{mtime: 2026-10-05T00:00:00Z}}, f: {{size: 10K, mtime: \
         2026-10-05T00:00:00Z}}}}\n  /tmp/pytest-of-b:\n    .: {{mtime: {OLD}}}\n    f: {{size: \
         10K, mtime: {OLD}}}\n    g: {{size: 10K, mtime: {OLD}}}\n"
    );
    let events = "events:\n  - {at: {op: unlinkat, path: /tmp/pytest-of-b/f, when: after}, \
                  advance: 700s}\n";
    let host = host(&yaml("", &tree, events));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert_eq!(report.exit_code(), 124);
    assert_eq!(actions(&report), ["/tmp/pytest-of-b"]);
    assert!(exists(&host, "/tmp/pytest-of-b/g"));
    assert!(
        touching(&host, "/tmp/pytest-of-a")
            .iter()
            .all(|c| c.op != Op::Flock)
    );
    assert_eq!(unlinked_below(&host, "/tmp/pytest-of-a"), 0);
}

/// The deadline is read right before each change (task-18 round 2, Codex
/// S1): a deadline that passes during the identity `statx` of an entry (it
/// blocked, or the machine slept) stops the delete before that entry's
/// `unlinkat`, and one that passes during the reopening of a read-only
/// directory stops it before its `fchmod`. The model runs twice: once to
/// find the call before the change, once with the clock jumping there.
#[test]
fn the_deadline_is_read_right_before_each_unlink_and_fchmod() {
    let tree = format!(
        "  /tmp/ro-x:\n    .: {{mtime: {OLD}, mode: \"0555\"}}\n    f: {{size: 10K, mtime: \
         {OLD}}}\n"
    );
    for (select, change) in [
        ("profile:plain-tmp", Op::Unlinkat),
        ("profile:writable-tmp", Op::Fchmod),
    ] {
        let first = host(&yaml("", &tree, ""));
        // Permissions are not modelled, so plain-tmp's delete of the
        // read-only directory's entry goes ahead: its first unlinkat.
        ensure(&first, &tmp(select));
        let trace = first.trace();
        let at = trace
            .iter()
            .position(|c| c.op == change)
            .expect("the change");
        let before = trace[..at]
            .iter()
            .rev()
            .find(|c| !matches!(c.op, Op::Now | Op::WallClock))
            .expect("a call before it");
        assert_eq!(before.op, Op::Statx, "{select}: {before:?}");
        let events = format!(
            "events:\n  - {{at: {{call: {}, when: after}}, advance: 700s}}\n",
            before.index
        );
        let host = host(&yaml("", &tree, &events));
        let report = ensure(&host, &tmp(select));
        assert!(host.unfired_events().is_empty(), "{select}");
        assert_eq!(report.status, Status::Deadline, "{select}: {report:#?}");
        assert!(
            calls(&host, change).is_empty(),
            "{select}: {change} after the deadline"
        );
        assert!(exists(&host, "/tmp/ro-x/f"), "{select}");
    }
}

/// The mount table is read again under the lock (task-18 round 2, mped
/// S5): a mount appearing inside the candidate right after its `flock` is
/// `contains_mount`, and nothing at or below it is ever touched (a dead
/// FUSE mount there would hang the walk's `statx`).
#[test]
fn a_mount_made_after_the_flock_is_contains_mount_and_never_touched() {
    let tree = format!(
        "  /tmp/pytest-of-b:\n    .: {{mtime: {OLD}}}\n    sub: {{.: {{mtime: {OLD}}}, f: {{size: \
         10K, mtime: {OLD}}}}}\n"
    );
    let events = "events:\n  - {at: {op: flock, path: /tmp/pytest-of-b, when: after}, mount: {id: \
                  50, at: /tmp/pytest-of-b/sub, fs: fuse, dev: \"0:60\", size: 1M}}\n";
    let host = host(&yaml("", &tree, events));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(
        skip_of(&report, "/tmp/pytest-of-b"),
        Some("contains_mount"),
        "{report:#?}"
    );
    let trace = host.trace();
    let flocked = trace
        .iter()
        .position(|c| c.op == Op::Flock && c.path.as_deref() == Some(Path::new("/tmp/pytest-of-b")))
        .expect("flocked");
    let touched: Vec<_> = trace[flocked..]
        .iter()
        .filter(|c| {
            c.path
                .as_deref()
                .is_some_and(|p| p.starts_with("/tmp/pytest-of-b/sub"))
        })
        .collect();
    assert!(touched.is_empty(), "{touched:#?}");
    assert_eq!(unlinked_below(&host, "/tmp/pytest-of-b"), 0);
}

/// Reached again, the candidate must be the object discovery found
/// (task-18 round 2, QA 1): one replaced right after the reclaim's first
/// read of the mount table is `changed` at its reach, before it is opened
/// for its lock or locked. The model runs twice: once to find that read
/// (the last one before the candidate's `flock`), once replacing it there.
#[test]
fn a_candidate_replaced_before_its_reach_is_never_opened_or_locked() {
    let tree = format!("  /tmp/pytest-of-b: {}\n", old_dir("10K"));
    let candidate = Path::new("/tmp/pytest-of-b");
    let first = host(&yaml("", &tree, ""));
    ensure(&first, &tmp("profile:pytest-of-dir"));
    let trace = first.trace();
    let flock = trace
        .iter()
        .position(|c| c.op == Op::Flock && c.path.as_deref() == Some(candidate))
        .expect("flocked");
    let read = trace[..flock]
        .iter()
        .rev()
        .find(|c| c.op == Op::Mountinfo)
        .expect("the step-1 read")
        .index;
    let events =
        format!("events:\n  - {{at: {{call: {read}, when: after}}, replace: /tmp/pytest-of-b}}\n");
    let host = host(&yaml("", &tree, &events));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(
        skip_of(&report, "/tmp/pytest-of-b"),
        Some("changed"),
        "{report:#?}"
    );
    let after: Vec<_> = host
        .trace()
        .into_iter()
        .filter(|c| c.index > read)
        .filter(|c| matches!(c.op, Op::Openat2 | Op::Flock) && c.path.as_deref() == Some(candidate))
        .collect();
    assert!(after.is_empty(), "{after:#?}");
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

/// The deadline is checked between candidates (task-18 round 2, QA 2): it
/// passes right after the first action is measured, and nothing more of
/// the run reaches for the next candidate: no mount table read, no call on
/// it. Exit 124.
#[test]
fn the_deadline_between_candidates_stops_before_the_next_one_is_touched() {
    let tree = format!(
        "  /tmp/pytest-of-a: {}\n  /tmp/pytest-of-b: {{.: {{mtime: 2026-09-01T00:00:00Z}}, f: \
         {{size: 10K, mtime: 2026-09-01T00:00:00Z}}}}\n",
        old_dir("10K")
    );
    // statvfs: the first sample, after the host lock, the reclaim's start,
    // then the one measuring the first action (pytest-of-b, the oldest).
    let events = "events:\n  - {at: {op: statvfs, nth: 4, when: after}, advance: 700s}\n";
    let host = host(&yaml("", &tree, events));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert_eq!(actions(&report), ["/tmp/pytest-of-b"]);
    let trace = host.trace();
    let measured = trace
        .iter()
        .filter(|c| c.op == Op::Statvfs)
        .nth(3)
        .expect("the fourth sample")
        .index;
    let late: Vec<_> = trace
        .iter()
        .filter(|c| c.index > measured)
        .filter(|c| {
            c.op == Op::Mountinfo
                || c.path
                    .as_deref()
                    .is_some_and(|p| p.starts_with("/tmp/pytest-of-a"))
        })
        .collect();
    assert!(late.is_empty(), "{late:#?}");
    assert!(exists(&host, "/tmp/pytest-of-a/f"));
}

/// An action whose space cannot be measured afterwards has a `null`
/// change, never 0, and the run stops: the goal can no longer be observed.
#[test]
fn an_unmeasured_action_is_null_and_stops_the_run() {
    let tree = format!(
        "  /tmp/pytest-of-a: {}\n  /tmp/pytest-of-b: {{.: {{mtime: 2026-09-01T00:00:00Z}}, f: \
         {{size: 10K, mtime: 2026-09-01T00:00:00Z}}}}\n",
        old_dir("10K")
    );
    // statvfs calls: the first sample, the one after the host lock, the
    // reclaim's start, then the one after the first action.
    let events = "events:\n  - {at: {op: statvfs, nth: 4, when: before}, fail: EIO}\n";
    let host = host(&yaml("", &tree, events));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(actions(&report), ["/tmp/pytest-of-b"], "{report:#?}");
    assert_eq!(report.actions[0].avail_delta_bytes, None);
    assert!(exists(&host, "/tmp/pytest-of-a/f"), "the run stopped");
    assert_eq!(report.exit_code(), 3);
}

/// The checkout guard's path rule on a reclaim (task-71, task-80): a broad
/// pattern below a project root finds `proj/.git/objects`, which a profile
/// that refuses checkouts never deletes (`contains_checkout`, by its path:
/// its own tree holds no marker); one that says `checkouts: allow` does.
#[test]
fn a_reclaiming_fs_run_never_deletes_proj_git_objects_unless_the_profile_allows_checkouts() {
    let tree = format!(
        "  /home/mark/src/proj/.git/objects: {}\n  /home/mark/src/proj/a/b: {}\n",
        old_dir("1M"),
        old_dir("1M")
    );
    let host = host(&yaml("", &tree, ""));
    let report = ensure(
        &host,
        &request(
            "profile:deep-dirs",
            &["/home/mark/src"],
            "/home/mark",
            "100%",
        ),
    );
    assert_eq!(
        skip_of(&report, "/home/mark/src/proj/.git/objects"),
        Some("contains_checkout"),
        "{report:#?}"
    );
    assert!(exists(&host, "/home/mark/src/proj/.git/objects/f"));
    assert_eq!(actions(&report), ["/home/mark/src/proj/a/b"]);

    let host = common::host(&yaml("", &tree, ""));
    let report = ensure(
        &host,
        &request(
            "profile:deep-dirs-checkouts",
            &["/home/mark/src"],
            "/home/mark",
            "100%",
        ),
    );
    assert!(
        !exists(&host, "/home/mark/src/proj/.git/objects"),
        "{report:#?}"
    );
}

/// `make_writable` (docs/reference.md, Actions) gives a read-only
/// directory owner write permission before anything in it is unlinked;
/// without it, no permission is changed.
#[test]
fn make_writable_is_the_profiles_option() {
    let tree = format!(
        "  /tmp/ro-x:\n    .: {{mtime: {OLD}, mode: \"0555\"}}\n    f: {{size: 10K, mtime: {OLD}}}\n"
    );
    let host = host(&yaml("", &tree, ""));
    let report = ensure(&host, &tmp("profile:writable-tmp"));
    assert_eq!(actions(&report), ["/tmp/ro-x"], "{report:#?}");
    assert_eq!(calls(&host, Op::Fchmod).len(), 1);

    let host = common::host(&yaml("", &tree, ""));
    ensure(&host, &tmp("profile:plain-tmp"));
    assert!(calls(&host, Op::Fchmod).is_empty());
}

/// The product flow reaches a command only after the same fresh in-use
/// check as deletion. A held file makes the candidate a skip, with no
/// RunCommand call at all.
#[test]
fn an_in_use_command_candidate_never_runs_the_cleaner() {
    let tree = format!("  /tmp/pytest-of-b: {}\n", old_dir("900M"));
    let held = "  - {pid: 8, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{path: /tmp/pytest-of-b/f}], environ: [PATH=/usr/bin]}\n";
    let host = host(&yaml("", &tree, held));
    let request = tmp("profile:cleaner");
    let report = ensure(&host, &request);
    assert_eq!(skip_of(&report, "/tmp/pytest-of-b"), Some("in_use"));
    assert_eq!(calls(&host, Op::RunCommand).len(), 0);
    assert!(exists(&host, "/tmp/pytest-of-b/f"));
    let plan = dry_run(&host, &request);
    assert_eq!(plan.plan.len(), 1, "{plan:#?}");
}

/// A browser lock can appear after planning and the general process scan.
/// The command action must recheck it at its final execution boundary.
#[test]
fn a_browser_lock_created_before_command_start_blocks_the_cleaner() {
    let tree = format!(
        "  /tmp/pytest-of-b: {}\n  /tmp/browser-config: {{.: {{}}}}\n",
        old_dir("900M")
    );
    let events = "events:\n  - {at: {op: pids, nth: 2, when: after}, create: /tmp/browser-config/SingletonLock}\n";
    let host = host(&yaml("", &tree, events));
    let report = ensure(&host, &tmp("profile:guarded-cleaner"));
    assert_eq!(
        skip_of(&report, "/tmp/pytest-of-b"),
        Some("in_use"),
        "{report:#?}\n{:#?}",
        host.trace()
    );
    assert!(
        calls(&host, Op::RunCommand).is_empty(),
        "{:#?}",
        host.trace()
    );
    assert!(exists(&host, "/tmp/pytest-of-b/f"));
}

#[test]
fn a_command_candidate_with_a_checkout_is_refused_before_exec() {
    let tree =
        format!("  /tmp/pytest-of-b: {{.: {{mtime: {OLD}}}, .git: {{HEAD: {{mtime: {OLD}}}}}}}\n");
    let host = host(&yaml("", &tree, ""));
    let report = ensure(&host, &tmp("profile:cleaner"));
    assert_eq!(
        skip_of(&report, "/tmp/pytest-of-b"),
        Some("contains_checkout"),
        "{report:#?}"
    );
    assert!(calls(&host, Op::RunCommand).is_empty());
    assert!(exists(&host, "/tmp/pytest-of-b/.git/HEAD"));
}

#[test]
fn a_missing_command_tool_skips_the_profile_without_starting_a_child() {
    let tree = format!("  /tmp/pytest-of-b: {}\n", old_dir("900M"));
    let host = host(&yaml("", &tree, ""));
    let report = ensure(&host, &tmp("profile:missing-cleaner"));
    assert_eq!(report.status, Status::Exhausted, "{report:#?}");
    assert_eq!(report.skipped.len(), 1, "{report:#?}");
    assert_eq!(report.skipped[0].reason, "missing");
    assert!(report.errors.is_empty(), "{report:#?}");
    assert!(calls(&host, Op::RunCommand).is_empty());
    assert!(exists(&host, "/tmp/pytest-of-b/f"));
    let plan = dry_run(
        &common::host(&yaml("", &tree, "")),
        &tmp("profile:missing-cleaner"),
    );
    assert_eq!(plan.plan.len(), 1, "{plan:#?}");
    assert_eq!(plan.plan[0].reason, Some("missing"));
}

#[test]
fn a_missing_command_cannot_bypass_the_reclaim_gate() {
    let tree = format!("  /tmp/pytest-of-b: {}\n", old_dir("900M"));
    let host = host(&yaml("", &tree, "sandbox_gate: closed\n"));
    let report = ensure(&host, &tmp("profile:missing-cleaner"));
    assert_eq!(report.status, Status::UsageError, "{report:#?}");
    assert_eq!(report.exit_code(), 2);
    assert!(report.errors[0].message.contains("sandbox gate"));
    let ops: Vec<Op> = host.trace().iter().map(|call| call.op).collect();
    assert!(ops.contains(&Op::MayDelete));
    for op in [Op::Statvfs, Op::Openat2, Op::Flock, Op::RunCommand] {
        assert!(!ops.contains(&op), "{op} after a closed gate: {ops:?}");
    }
}

#[test]
fn a_nonzero_command_exit_is_reported_with_exit_three() {
    let tree = format!("  /tmp/pytest-of-b: {}\n", old_dir("900M"));
    let host = host(&yaml("", &tree, "command_exit: 7\n"));
    let report = ensure(&host, &tmp("profile:cleaner"));
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.actions.len(), 1, "{report:#?}");
    assert!(report.errors[0].message.contains("status 7"), "{report:#?}");
    let calls = calls(&host, Op::RunCommand);
    assert_eq!(calls.len(), 1);
    let run = calls[0].command.as_ref().expect("traced command request");
    assert_eq!(run.executable, Path::new("/usr/bin/rm"));
    assert_eq!(run.arguments[1], "/tmp/pytest-of-b");
    assert_eq!(
        calls[0].path.as_deref(),
        Some(Path::new("/tmp/pytest-of-b"))
    );
}

#[test]
fn env_invokes_the_inner_executable_resolved_during_preflight() {
    let tree = format!("  /tmp/pytest-of-b: {}\n", old_dir("900M"));
    let host = host(&yaml("", &tree, "command_exit: 7\n"));
    let report = ensure(&host, &tmp("profile:env-cleaner"));
    assert_eq!(report.status, Status::Error, "{report:#?}");
    let calls = calls(&host, Op::RunCommand);
    assert_eq!(calls.len(), 1);
    let run = calls[0].command.as_ref().expect("traced command request");
    assert_eq!(run.executable, Path::new("/usr/bin/env"));
    assert_eq!(run.arguments[0], "PATH=/usr/bin");
    assert_eq!(run.arguments[1], "/usr/bin/rm");
    assert_eq!(run.arguments[3], "/tmp/pytest-of-b");
}

#[test]
fn env_repeated_terminator_is_refused_in_preflight() {
    let host = host(&yaml("", "", ""));
    let argv = ["env", "--", "--", "rm"].map(str::to_owned);
    let error = diskgc_core::command_action::preflight(&host, &argv).unwrap_err();
    assert!(error.detail.contains("repeated `--`"), "{error}");
}

#[test]
fn an_output_failure_after_spawn_is_an_action_with_an_observed_delta() {
    let tree = format!("  /tmp/pytest-of-b: {}\n", old_dir("900M"));
    let host = host(&yaml("", &tree, "command_error_after_start: true\n"));
    let report = ensure(&host, &tmp("profile:cleaner"));
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.actions.len(), 1, "{report:#?}");
    assert_eq!(report.actions[0].avail_delta_bytes, Some(0));
    assert!(
        report.errors[0].message.contains("after spawn"),
        "{report:#?}"
    );
    assert_eq!(calls(&host, Op::RunCommand).len(), 1);
}

#[test]
fn a_deadline_expired_at_the_port_starts_no_reported_action() {
    let tree = format!("  /tmp/pytest-of-b: {}\n", old_dir("900M"));
    let event = "events:\n  - {at: {op: run_command, nth: 1, when: before}, advance: 700s}\n";
    let host = host(&yaml("", &tree, event));
    let report = ensure(&host, &tmp("profile:cleaner"));
    assert!(report.actions.is_empty(), "{report:#?}");
    assert_eq!(calls(&host, Op::RunCommand).len(), 1);
    assert_eq!(
        calls(&host, Op::RunCommand)[0].result,
        "DeadlineBeforeStart"
    );
}

/// Each kind runs its own protocol only (both directions). An fs tier
/// whose roots hold a Cargo target reclaims its own candidates through the
/// fs protocol and never takes a Cargo lock or touches the target; a
/// Cargo tier over the same roots reclaims the target's profile and never
/// an fs candidate.
#[test]
fn each_kind_reaches_only_its_own_protocol() {
    let at = OLD;
    let tree = format!(
        "  /home/mark/src/proj/Cargo.toml: {{}}\n  /home/mark/src/proj/.pytest_cache: {}\n  \
         /home/mark/src/proj/target:\n    CACHEDIR.TAG: {{cachedir_tag: true}}\n    debug: {{.: \
         {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}, .fingerprint: {{.: {{mtime: {at}}}, p-1: \
         {{.: {{mtime: {at}}}}}}}, deps: {{.: {{mtime: {at}}}, p-1: {{size: 5G, mtime: \
         {at}}}}}}}\n",
        old_dir("1M")
    );
    let host = host(&yaml("", &tree, ""));
    let report = ensure(
        &host,
        &request(
            "profile:pytest-cache-fixture",
            &["/home/mark/src"],
            "/home/mark",
            "100%",
        ),
    );
    assert_eq!(
        actions(&report),
        ["/home/mark/src/proj/.pytest_cache"],
        "{report:#?}"
    );
    assert!(
        calls(&host, Op::Flock).iter().all(|c| !c
            .path
            .as_deref()
            .is_some_and(|p| p.starts_with("/home/mark/src/proj/target"))),
        "a Cargo lock was taken by an fs tier"
    );
    assert_eq!(unlinked_below(&host, "/home/mark/src/proj/target"), 0);

    let library = Library::builtin().unwrap_or_else(|e| panic!("{e}"));
    let host = common::host(&yaml("", &tree, ""));
    let request = Request {
        path: PathBuf::from("/home/mark"),
        min_free: parse_size("100%").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select(
            "profile:cargo-target",
            &library,
            Some(&["/home/mark/src".to_owned()]),
            1000,
        )
        .unwrap_or_else(|e| panic!("{e}")),
    };
    let report = ensure(&host, &request);
    assert_eq!(
        actions(&report),
        ["/home/mark/src/proj/target"],
        "{report:#?}"
    );
    assert!(exists(&host, "/home/mark/src/proj/.pytest_cache/f"));
    assert_eq!(
        unlinked_below(&host, "/home/mark/src/proj/.pytest_cache"),
        0
    );
}

#[test]
fn cargo_report_json_keeps_multiple_attributed_processes() {
    let at = OLD;
    let tree = format!(
        "  /home/mark/src/proj/Cargo.toml: {{}}\n  /home/mark/src/proj/target:\n    \
         CACHEDIR.TAG: {{cachedir_tag: true}}\n    debug: {{.: {{mtime: {at}}}, \
         .cargo-lock: {{mtime: {at}}}, .fingerprint: {{.: {{mtime: {at}}}, p-1: \
         {{.: {{mtime: {at}}}}}}}, deps: {{.: {{mtime: {at}}}, p-1: {{size: 5G, mtime: \
         {at}}}}}}}\n"
    );
    let rest = "  - {pid: 42, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], \
                fds: [{path: /usr/bin/tool}], environ: \
                [CARGO_TARGET_DIR=/home/mark/src/proj/target]}\n  - {pid: 43, \
                exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], \
                fds: [{path: /usr/bin/tool}], \
                environ: [TMPDIR=/home/mark/src/proj/target/debug]}\n";
    let host = host(&yaml("", &tree, rest));
    let report = ensure(
        &host,
        &request(
            "profile:cargo-target",
            &["/home/mark/src"],
            "/home/mark",
            "100%",
        ),
    );
    assert_eq!(
        skip_of(&report, "/home/mark/src/proj/target"),
        Some("in_use"),
        "{report:#?}"
    );
    let wire: serde_json::Value = serde_json::from_str(&report.to_json()).expect("report JSON");
    let blockers = wire["skipped"][0]["process_blockers"]
        .as_array()
        .expect("attributed process array");
    assert_eq!(
        blockers,
        &vec![
            serde_json::json!({"pid": 42, "command": "tool", "evidence": "environment"}),
            serde_json::json!({"pid": 43, "command": "tool", "evidence": "environment"}),
        ]
    );
}

/// Mounts first, on a reclaim too: a mount at a candidate's name or inside
/// it, of any type, is never `statx`ed, opened or locked (a dead FUSE
/// mount would hang the call); the candidate is `contains_mount`.
#[test]
fn a_mount_at_or_inside_a_candidate_is_never_touched() {
    let mounts = "  - {id: 41, at: /tmp/pytest-of-fuse, fs: fuse, dev: \"0:51\", size: 1G, \
                  parent: 30}\n  - {id: 42, at: /tmp/pytest-of-holder/m, fs: fuse, dev: \"0:52\", \
                  size: 1G, parent: 30}\n";
    let tree = format!(
        "  /tmp/pytest-of-fuse/f: {{mtime: {OLD}}}\n  /tmp/pytest-of-holder: {}\n  \
         /tmp/pytest-of-holder/m/g: {{mtime: {OLD}}}\n",
        old_dir("1M")
    );
    let host = host(&yaml(mounts, &tree, ""));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert!(report.actions.is_empty(), "{report:#?}");
    for mount in ["/tmp/pytest-of-fuse", "/tmp/pytest-of-holder/m"] {
        assert!(
            touching(&host, mount).is_empty(),
            "{:#?}",
            touching(&host, mount)
        );
    }
    assert!(
        touching(&host, "/tmp/pytest-of-holder")
            .iter()
            .all(|c| c.op != Op::Flock && c.op != Op::ReadDir),
        "a candidate with a mount inside was locked or listed"
    );
}
