//! Kind `fs` in `ensure --dry-run` (task-68's spike, diskgc-core
//! `fs_kind`): segment-wise find patterns plan candidates, each judged by
//! the engine's eligibility walk and the in-use check, as the reclaim
//! judges them (tests/fs_reclaim.rs). Profiles are user profiles in the
//! fixtures.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{dir, host};
use diskgc_core::engine::{Candidate, Checkouts, DeleteOptions, End, Rules, delete, walk};
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::inuse::MAX_LISTINGS;
use diskgc_core::profile::{Action, Body, Library, parse_profiles};
use diskgc_core::report::{Report, Status};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

/// The user profiles the tests select from. `pytest-of-dir` is a stand-in
/// with whole `pytest-of-*` dirs as candidates (simple fixtures), not the
/// library's `pytest-tmp` (one test dir per candidate, tested against the
/// spike's profile file below). `pytest-cache-fixture` pins an older depth
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
    let outcome = delete(
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
    let report = executor::dry_run(host, request, uid, &mut |l| lines.push(l.to_owned()));
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
        &request("profile:cmake-build", &["/home/mark/src"], "/home/mark"),
    );
    assert_eq!(
        plan(&report),
        [("/home/mark/src/build".to_owned(), None)],
        "{:#?}",
        report.plan
    );
}

/// A `has` / `has_not` filter in a term is refused (task-16), never
/// silently ignored; `!filter:` and unions stay refused too.
#[test]
fn has_filters_in_a_term_are_refused_not_ignored() {
    let library = library();
    for (expr, needle) in [
        ("profile:tmp-entry && filter:has-tag", "`filter:has-tag`"),
        ("profile:tmp-entry && !filter:idle-1d", "`!filter:idle-1d`"),
        ("profile:tmp-entry || profile:pytest-of-dir", "a union"),
    ] {
        let error = Tier::select(expr, &library, None, 1000).expect_err(expr);
        assert!(
            error.contains(needle) && error.contains("task-16"),
            "{expr}: {error}"
        );
    }
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
    assert_eq!(report.status, Status::DryRun);
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
            "profile:cmake-build",
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

/// task-71 AC #2 (QA's `insrc2`): an in-source build inside a checkout,
/// `.git` and `CMakeCache.txt` but no `CMakeLists.txt`, passes
/// `cmake-build`'s own `where` and is still never eligible: the walk
/// refuses it as `contains_checkout`, at the `.git`. An out-of-source build
/// next to it stays eligible, so the refusal is the checkout's; it lies
/// inside a checkout itself (`/home/mark/.git`), which does not count: only
/// what is in the candidate does.
#[test]
fn an_in_source_build_in_a_checkout_is_never_eligible_for_cmake_build() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let host = host(&yaml(&format!(
        "  /home/mark/src/insrc2:\n    .: {at}\n    CMakeCache.txt: {at}\n    main.c: {at}\n    \
         .git: {{.: {at}, HEAD: {at}, objects: {{.: {at}}}}}\n  /home/mark/src/build:\n    .: \
         {at}\n    CMakeCache.txt: {at}\n  /home/mark/.git: {{HEAD: {at}}}\n"
    )));
    let report = dry_run(
        &host,
        &request("profile:cmake-build", &["/home/mark/src"], "/home/mark"),
    );
    assert_eq!(
        plan(&report),
        [
            ("/home/mark/src/build".to_owned(), None),
            (
                "/home/mark/src/insrc2".to_owned(),
                Some("contains_checkout")
            ),
        ],
        "{:#?}",
        report.plan
    );
    assert_eq!(report.status, Status::DryRun);
    nothing_locked_or_deleted(&host);
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
    assert_eq!(allows(&lines), 1, "{lines:#?}");
    // And in the plan (`--json`): every entry was judged under `allow`.
    assert!(
        report.plan.iter().all(|p| p.checkouts == Checkouts::Allow),
        "{:#?}",
        report.plan
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("profile `tmp-entry-checkouts` says `checkouts: allow`")),
        "{lines:#?}"
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

/// The spike's profile file (task-68; diskgc-core
/// `fs_kind/spike-profiles.yaml`) as a library: what the real-machine dry
/// runs use, so a test of its profiles tests the file itself.
fn spike_library() -> Library {
    let text = include_str!("../../diskgc-core/src/fs_kind/spike-profiles.yaml");
    diskgc_core::config::parse(Path::new("spike-profiles.yaml"), text.as_bytes())
        .unwrap_or_else(|e| panic!("{e}"))
        .library
}

/// task-71 (Mark, 2026-10-18): `pytest-tmp` takes one pytest test dir
/// per candidate (`pytest-of-<user>/pytest-<N>/<test>`), the test dirs of
/// two sessions and one inside a nix-shell temp dir. task-80 (Mark,
/// 2026-10-19): it says `checkouts: allow`, so the test dir where a test
/// ran `git init` is eligible too. The session dirs, `pytest-of-mark`,
/// pytest's `*current` symlinks and a plain file in a session dir
/// (pytest's `.lock`) are never candidates.
#[test]
fn pytest_tmp_takes_one_test_dir_per_candidate_checkouts_included() {
    let at = "{mtime: 2026-10-01T00:00:00Z}";
    let link = |to: &str| format!("{{symlink: {to}, mtime: 2026-10-01T00:00:00Z}}");
    let host = host(&yaml(&format!(
        "  /tmp/pytest-of-mark:\n    .: {at}\n    pytest-1:\n      .: {at}\n      .lock: {at}\n      \
         test_repo0: {{.: {at}, .git: {{.: {at}, HEAD: {at}}}, f: {at}}}\n      test_repocurrent: \
         {}\n      test_other0: {{.: {at}, g: {at}}}\n    pytest-2: {{.: {at}, test_x0: {{.: {at}, \
         h: {at}}}}}\n    pytest-current: {}\n  \
         /tmp/nix-shell.Ab1/pytest-of-mark/pytest-0/test_y0: {{.: {at}, i: {at}}}\n",
        link("/tmp/pytest-of-mark/pytest-1/test_repo0"),
        link("/tmp/pytest-of-mark/pytest-2"),
    )));
    let request = Request {
        tier: Tier::select("profile:pytest-tmp", &spike_library(), None, 1000)
            .unwrap_or_else(|e| panic!("{e}")),
        ..request("profile:tmp-entry", &[], "/tmp")
    };
    let report = dry_run(&host, &request);
    let mut found = plan(&report);
    found.sort();
    assert_eq!(
        found,
        [
            (
                "/tmp/nix-shell.Ab1/pytest-of-mark/pytest-0/test_y0".to_owned(),
                None
            ),
            ("/tmp/pytest-of-mark/pytest-1/test_other0".to_owned(), None),
            ("/tmp/pytest-of-mark/pytest-1/test_repo0".to_owned(), None),
            ("/tmp/pytest-of-mark/pytest-2/test_x0".to_owned(), None),
            ("/var/tmp".to_owned(), Some("missing")),
        ],
        "{:#?}",
        report.plan
    );
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
