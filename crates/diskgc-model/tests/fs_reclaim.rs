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
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::{Report, Status};
use diskgc_core::units::parse_size;
use diskgc_model::spec::Op;
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
- name: anything
  find: {roots: required, paths: ["*"], type: any}
  action: delete
  examples: {match: [/srv/x]}
- name: pytest-cache-fixture
  find: {roots: required, paths: [".pytest_cache", "*/.pytest_cache"], type: dir}
  action: delete
  examples: {match: [/home/mark/src/app/.pytest_cache]}
- name: cmake-build
  find: {roots: required, paths: ["*"], type: dir}
  where: {has: CMakeCache.txt, has_not: CMakeLists.txt}
  action: delete
  examples: {match: [/home/mark/src/build]}
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

fn ensure(host: &ModelHost, request: &Request) -> Report {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::ensure(host, request, uid, &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

fn dry_run(host: &ModelHost, request: &Request) -> Report {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::dry_run(host, request, uid, &mut |l| lines.push(l.to_owned()));
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
/// root on one is refused by its mountinfo line before anything there is
/// opened or `statx`ed (`unsupported_fs`), whatever `PATH` is; a network
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
        assert_ne!(report.exit_code(), 0, "{fs}: {report:#?}");
        assert!(report.actions.is_empty(), "{fs}: {report:#?}");
        assert_eq!(
            skip_of(&report, "/mnt/net/scratch"),
            Some("unsupported_fs"),
            "{fs}: {report:#?}"
        );
        assert!(
            touching(&host, "/mnt/net/scratch").is_empty(),
            "{fs}: {:#?}",
            touching(&host, "/mnt/net/scratch")
        );
        assert!(exists(&host, "/mnt/net/scratch/old/f"), "{fs}");
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
        assert!(report.actions.is_empty(), "{fs}: {report:#?}");
        assert_eq!(
            skip_of(&report, "/mnt/f/scratch"),
            Some("unsupported_fs"),
            "{fs}: {report:#?}"
        );
        assert!(
            touching(&host, "/mnt/f/scratch").is_empty(),
            "{fs}: {:#?}",
            touching(&host, "/mnt/f/scratch")
        );
        assert!(exists(&host, "/mnt/f/scratch/old/f"), "{fs}");
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
    let cases: [(&str, String, String, &str, &str); 4] = [
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
            "profile:cmake-build",
            "/home/mark/src/build".to_owned(),
            format!(
                "  - {{at: {}, remove: /home/mark/src/build/CMakeCache.txt}}\n",
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
        let roots: &[&str] = if select.contains("cmake") {
            &["/home/mark/src"]
        } else {
            &[]
        };
        let path = if select.contains("cmake") {
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
/// is never reused: the next candidate gets a new scan, also within the
/// reuse window. Here pid 9 exits after blocking the first candidate.
#[test]
fn a_blocking_scan_is_not_reused_for_the_next_candidate() {
    let tree = format!(
        "  /tmp/pytest-of-a: {}\n  /tmp/pytest-of-b: {{.: {{mtime: 2026-09-01T00:00:00Z}}, f: \
         {{size: 10K, mtime: 2026-09-01T00:00:00Z}}}}\n",
        old_dir("10K")
    );
    let rest = "  - {pid: 9, exe: /usr/bin/tool, proc: failed}\nevents:\n  - {at: {op: flock, \
                path: /tmp/pytest-of-a, when: after}, exit: 9}\n";
    let host = host(&yaml("", &tree, rest));
    let report = ensure(&host, &tmp("profile:pytest-of-dir"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(skip_of(&report, "/tmp/pytest-of-b"), Some("in_use"));
    assert_eq!(actions(&report), ["/tmp/pytest-of-a"], "{report:#?}");
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

/// A reclaiming `ensure` refuses an fs profile whose action it does not
/// run (a command, task-37) before anything else: no gate question, no
/// sample, no lock, no call on the host at all (exit 2). Its dry run
/// still plans it.
#[test]
fn a_command_action_is_refused_before_anything() {
    let tree = format!("  /tmp/pytest-of-b: {}\n", old_dir("900M"));
    let host = host(&yaml("", &tree, ""));
    let request = tmp("profile:cleaner");
    let uid = executor::privileges(&host, false).unwrap_or_else(|e| panic!("{e}"));
    let calls_before = host.trace().len();
    let report = executor::ensure(&host, &request, uid, &mut |_| {});
    assert_eq!(report.status, Status::UsageError, "{report:#?}");
    assert_eq!(report.exit_code(), 2);
    assert!(
        report.errors[0].message.contains("task-37"),
        "{}",
        report.errors[0].message
    );
    assert_eq!(
        host.trace().len(),
        calls_before,
        "{:#?}",
        &host.trace()[calls_before..]
    );
    assert!(exists(&host, "/tmp/pytest-of-b/f"));
    let plan = dry_run(&host, &request);
    assert_eq!(plan.plan.len(), 1, "{plan:#?}");
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
