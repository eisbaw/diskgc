//! Kind `fs` in `ensure --dry-run` (task-68's spike, diskgc-core
//! `fs_kind`): segment-wise find patterns plan candidates, each judged by
//! the engine's eligibility walk and the in-use check; and a reclaiming
//! `ensure` over `fs` candidates is refused before it touches anything
//! (their delete is task-18). Profiles are user profiles in the fixtures.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::{Report, Status};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

/// The user profiles the tests select from.
const PROFILES: &str = r#"
- name: pytest-tmp
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
- name: pytest-cache
  find:
    roots: required
    paths: [".pytest_cache", "*/.pytest_cache", "*/*/.pytest_cache"]
    type: dir
  action: delete
  examples: {match: [/home/mark/src/app/.pytest_cache]}
- name: cmake-build
  find: {roots: required, paths: ["*", "*/*"], type: dir}
  where: {has: CMakeCache.txt, has_not: CMakeLists.txt}
  action: delete
  examples: {match: [/home/mark/src/app/build]}
- name: has-tag
  where: {has: CACHEDIR.TAG}
- name: unmounted
  find: {roots: [/tmp], paths: ["pytest-of-*"], type: dir}
  where: {has_not: mounted}
  action: delete
  examples: {match: [/tmp/pytest-of-mark]}
- name: anything
  find: {roots: required, paths: ["*"], type: any}
  action: delete
  examples: {match: [/srv/x]}
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

fn dry_run(host: &ModelHost, request: &Request) -> Report {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::dry_run(host, request, uid, &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
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

/// AC #1: the patterns find exactly what they say, one level per
/// segment: top-level and one level down, directories only (`type: dir`:
/// a file, a symlink or a FIFO of a matching name is no candidate), not two
/// levels down, not the candidate's own insides, and not `/home`.
/// Eligible first, oldest newest-change first.
#[test]
fn pytest_tmp_plans_exactly_what_its_patterns_find() {
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
    let report = dry_run(&host, &request("profile:pytest-tmp", &[], "/tmp"));
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
        &request("profile:pytest-tmp && filter:idle-3d", &[], "/tmp"),
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
/// entered); a candidate on another filesystem than PATH's is `other_fs`.
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
    // The same candidates for a request on /home: on another filesystem.
    let host = common::host(&yaml("  /tmp/f: {}\n  /home/mark/x: {}\n"));
    let report = dry_run(&host, &request("profile:tmp-entry", &[], "/home/mark"));
    assert_eq!(plan(&report), [("/tmp/f".to_owned(), Some("other_fs"))]);
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
            "profile:pytest-cache",
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
        &request("profile:cmake-build", &["/home/mark/src"], "/home/mark"),
    );
    assert_eq!(
        plan(&report),
        [("/home/mark/src/build".to_owned(), None)],
        "{:#?}",
        report.plan
    );
}

/// The safety requirement of task-68: kind `fs` has no delete yet
/// (task-18), so a reclaiming `ensure` over `fs` candidates is refused as a
/// usage error (exit 2), before anything else: no gate question, no
/// sample, no host lock, no call on the host at all. Even with eligible
/// candidates and the goal unmet.
#[test]
fn a_reclaiming_ensure_over_fs_candidates_is_refused_before_anything() {
    let host = host(&yaml(
        "  /tmp/pytest-of-mark: {.: {mtime: 2026-10-01T00:00:00Z}, f: {size: 900M}}\n  \
         /run/diskgc/lock: {uid: 0, gid: 0}\n",
    ));
    let mut req = request("profile:pytest-tmp", &[], "/tmp");
    req.min_free = parse_size("200M").expect("a size");
    let uid = executor::privileges(&host, false).unwrap_or_else(|e| panic!("{e}"));
    let calls_before = host.trace().len();
    let report = executor::ensure(&host, &req, uid, &mut |_| {});
    assert_eq!(report.status, Status::UsageError, "{report:#?}");
    assert_eq!(report.exit_code(), 2);
    let message = &report.errors[0].message;
    assert!(
        message.contains("task-18") && message.contains("not implemented"),
        "{message}"
    );
    assert_eq!(
        host.trace().len(),
        calls_before,
        "ensure touched the host: {:#?}",
        &host.trace()[calls_before..]
    );
    assert!(host.stat_path(Path::new("/tmp/pytest-of-mark/f")).is_some());
}

/// A `has` / `has_not` filter in a term is refused (task-16), never
/// silently ignored; `!filter:` and unions stay refused too.
#[test]
fn has_filters_in_a_term_are_refused_not_ignored() {
    let library = library();
    for (expr, needle) in [
        ("profile:tmp-entry && filter:has-tag", "`filter:has-tag`"),
        ("profile:tmp-entry && !filter:idle-1d", "`!filter:idle-1d`"),
        ("profile:tmp-entry || profile:pytest-tmp", "a union"),
    ] {
        let error = Tier::select(expr, &library, None, 1000).expect_err(expr);
        assert!(
            error.contains(needle) && error.contains("task-16"),
            "{expr}: {error}"
        );
    }
}

/// A dry run reuses one process scan for later candidates for up to
/// `DRY_RUN_SCAN_REUSE` (it deletes nothing: the deletion's freshness
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
        let report = dry_run(&host, &request("profile:pytest-tmp", &[], "/tmp"));
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
    let report = dry_run(&host, &request("profile:pytest-tmp", &[], "/tmp"));
    assert!(host.unfired_events().is_empty());
    assert_eq!(reason_of(&report, "/tmp/pytest-of-a"), Some(Some("in_use")));
    assert_eq!(
        reason_of(&report, "/tmp/pytest-of-b"),
        Some(None),
        "{:#?}",
        report.plan
    );
}

/// Defence in depth behind the refusal above (task-68 review, F1): an fs
/// tier never reaches the Cargo reclaim. QA's scenario: a `roots: required`
/// fs profile whose roots hold an old, attached Cargo target, below
/// `--min-free` with the host lock provisioned. With the allowlist guard in
/// `ensure`, nothing is called; were it removed, the reclaim's discovery
/// refuses the fs source (an error of the run) instead of reclaiming the
/// target its roots happen to hold. Either way: nothing unlinked, the
/// target intact.
#[test]
fn an_fs_tier_never_reaches_the_cargo_reclaim_over_its_roots() {
    let at = "2026-10-01T00:00:00Z";
    let host = host(&yaml(&format!(
        "  /run/diskgc/lock: {{uid: 0, gid: 0}}\n  /home/mark/src/proj/Cargo.toml: {{}}\n  \
         /home/mark/src/proj/.pytest_cache: {{.: {{mtime: {at}}}}}\n  \
         /home/mark/src/proj/target:\n    CACHEDIR.TAG: {{cachedir_tag: true}}\n    debug: \
         {{.: {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}, .fingerprint: {{.: {{mtime: {at}}}, \
         p-1: {{.: {{mtime: {at}}}}}}}, deps: {{.: {{mtime: {at}}}, p-1: {{size: 5G, mtime: \
         {at}}}}}}}\n"
    )));
    let mut req = request("profile:pytest-cache", &["/home/mark/src"], "/home/mark");
    req.min_free = parse_size("99G").expect("a size");
    let uid = executor::privileges(&host, false).unwrap_or_else(|e| panic!("{e}"));
    let report = executor::ensure(&host, &req, uid, &mut |l| eprintln!("{l}"));
    assert_ne!(report.exit_code(), 0, "{report:#?}");
    assert_eq!(
        host.trace().iter().filter(|c| c.op == Op::Unlinkat).count(),
        0,
        "{report:#?}"
    );
    assert!(report.actions.is_empty(), "{:#?}", report.actions);
    assert!(
        host.stat_path(Path::new("/home/mark/src/proj/target/debug/deps/p-1"))
            .is_some()
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
/// searched: checked on the root's own names, before it is opened.
#[test]
fn a_root_inside_private_tmp_is_never_searched() {
    let private = "/tmp/systemd-private-abc-cups.service-x1/tmp";
    let host = host(&yaml(&format!("  {private}/f: {{}}\n  /tmp/g: {{}}\n")));
    let report = dry_run(&host, &request("profile:anything", &[private], "/tmp"));
    assert_eq!(plan(&report), [], "{:#?}", report.plan);
    assert_eq!(report.status, Status::DryRun);
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
        let report = dry_run(&host, &request("profile:pytest-tmp", &[], "/tmp"));
        assert!(host.unfired_events().is_empty(), "{event}");
        assert_eq!(report.status, Status::Deadline, "{event}: {report:#?}");
        assert_eq!(report.exit_code(), 124, "{event}");
    }
}
