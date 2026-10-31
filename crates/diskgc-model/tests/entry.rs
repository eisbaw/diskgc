//! The one reclaiming entry point (TASK-270; docs/architecture.md, The
//! reclaim walk): `--select` ([`executor::ensure_with_index`]) is a group
//! of one and runs exactly as a configuration of that one tier
//! ([`executor::ensure_grouped_with_index`]). Each scenario runs on both
//! entry points and pins what they share:
//!
//! - every return after the synchronous tally goes through one exit that
//!   writes the snapshot, or says in one line why not (PRD: each `ensure`
//!   that tallies leaves a snapshot): met under the host lock, a root
//!   that refuses its tier, a `protect` that cannot be enumerated, a host
//!   lock held to the deadline;
//! - the same log lines (taking the host lock, its timeout, met after it);
//! - a command's executable is resolved under the host lock, never on a
//!   run whose goal is already met (docs/reference.md, `ensure`:
//!   "Before the first deletion, it checks all configured roots, command
//!   executables and Docker bindings under the host lock");
//! - one allowlist of reclaimable actions, with one message.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::config;
use diskgc_core::engine::Reason;
use diskgc_core::executor::{self, Request, SourceKind, Tier};
use diskgc_core::profile::{Action, Library, parse_profiles};
use diskgc_core::report::{Report, Status};
use diskgc_core::snapshot::RunIndex;
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

const PROFILES: &str = r#"
- name: tmp-a
  find: {roots: [/tmp], paths: ["a*"], type: dir}
  action: delete
  examples: {match: [/tmp/a0]}
- name: tmp-a-linked
  find: {roots: [/tmp, /tmp/link], paths: ["a*"], type: dir}
  action: delete
  examples: {match: [/tmp/a0]}
- name: missing-cleaner
  find: {roots: [/tmp], paths: ["a*"], type: dir}
  action: {command: [diskgc-no-such-tool, "{path}"]}
  examples: {match: [/tmp/a0]}
- name: guard
  find: {roots: required, paths: ['*'], type: dir}
  action: delete
  examples: {root: /tmp/protected, match: [/tmp/protected/inside]}
"#;

/// `protect` of `guard` below a symlinked root: it cannot be enumerated.
const UNSAFE_PROTECT: &str = r#"version: 1
profiles:
  - name: guard
    find: {roots: required, paths: ['*'], type: dir}
    action: delete
    examples: {root: /tmp/protected, match: [/tmp/protected/inside]}
protect: [{profile: guard, roots: [/tmp/guard-link]}]
"#;

const STATE: &str = "/home/u/.local/state/diskgc";
const SNAPSHOT: &str = "/home/u/.local/state/diskgc/index-dev-0-42";
const HOST_LOCK: &str = "/run/diskgc/lock";
const WRITTEN: &str = "index: snapshot index-dev-0-42 written: ";

/// `/` (with the state directory), `/run` with the host lock, and a 1 GiB
/// `/tmp` (tmpfs, dev 0:42) with 500 MiB used elsewhere and three fresh
/// candidates of `tmp-a`; `rest` appended to the tree.
fn yaml(rest: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", uuid: 4fca2e4a-9b1e-4989-a4c7-d05957e51910, size: 100G}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, other_used: 500M, parent: 1}}
state_dir: {STATE}
tree:
  /usr/bin/tool: {{}}
  /usr/bin/rm: {{mode: "0755"}}
  {HOST_LOCK}: {{uid: 0, gid: 0}}
  /home/u: {{.: {{mode: "0700"}}}}
  /tmp/a1: {{f: {{size: 10K}}}}
  /tmp/a2: {{f: {{size: 10K}}}}
  /tmp/a3: {{f: {{size: 10K}}}}
{rest}"#
    )
}

fn library() -> Library {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"))
}

fn tier(select: &str) -> Tier {
    Tier::select(select, &library(), None, 1000).unwrap_or_else(|e| panic!("{e}"))
}

fn request(tier: Tier, min_free: &str) -> Request {
    Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size(min_free).expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier,
    }
}

/// The two public ways into a reclaim.
#[derive(Debug, Clone, Copy)]
enum Entry {
    /// `ensure --select`.
    Select,
    /// `ensure` of a configuration with the request's tier alone.
    Grouped,
}

const ENTRIES: [Entry; 2] = [Entry::Select, Entry::Grouped];

/// One reclaim of `request` through `entry`, with a snapshot index; the
/// report and its log.
fn run(host: &ModelHost, entry: Entry, request: &Request) -> (Report, Vec<String>) {
    let mut lines = Vec::new();
    let mut index = RunIndex::snapshot();
    let mut log = |l: &str| lines.push(l.to_owned());
    let report = match entry {
        Entry::Select => executor::ensure_with_index(host, request, &[1000], &mut index, &mut log),
        Entry::Grouped => executor::ensure_grouped_with_index(
            host,
            request,
            std::slice::from_ref(&request.tier),
            &[Duration::ZERO],
            &[1000],
            &mut index,
            &mut log,
        ),
    };
    eprintln!("{entry:?} log:\n  {}", lines.join("\n  "));
    (report, lines)
}

fn has(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

fn host_lock_taken(host: &ModelHost) -> bool {
    host.trace().iter().any(|c| {
        c.op == Op::Flock
            && c.path.as_deref() == Some(Path::new(HOST_LOCK))
            && c.result == "acquired"
    })
}

/// The tally ran, then the snapshot was written with its units.
fn assert_snapshot_written(host: &ModelHost, entry: Entry, lines: &[String]) {
    assert!(
        has(lines, "index: synchronous tally of 1 tier(s)"),
        "{entry:?}: {lines:#?}"
    );
    assert!(has(lines, WRITTEN), "{entry:?}: no snapshot: {lines:#?}");
    assert!(
        host.stat_path(Path::new(SNAPSHOT)).is_some(),
        "{entry:?}: {lines:#?}"
    );
}

/// Another reclaim frees the space while this run waits for the host
/// lock: met under the lock, nothing reclaimed, the snapshot written, and
/// the same line on both entry points.
#[test]
fn met_under_the_host_lock_writes_the_snapshot_and_says_so() {
    for entry in ENTRIES {
        let freed = format!(
            "events:\n  - {{at: {{op: flock, path: {HOST_LOCK}, when: after}}, other_used: \
             {{mount: /tmp, bytes: 0}}}}\n"
        );
        let host = host(&yaml(&freed));
        let (report, lines) = run(&host, entry, &request(tier("profile:tmp-a"), "700M"));
        assert!(host.unfired_events().is_empty(), "{entry:?}");
        assert_eq!(report.status, Status::Met, "{entry:?}: {report:#?}");
        assert!(report.actions.is_empty(), "{entry:?}: {report:#?}");
        assert!(
            has(&lines, "below --min-free 700.0 MiB: taking the reclaim lock"),
            "{entry:?}: {lines:#?}"
        );
        assert!(
            has(
                &lines,
                "available now: --min-free is met, nothing to reclaim"
            ),
            "{entry:?}: {lines:#?}"
        );
        assert_snapshot_written(&host, entry, &lines);
    }
}

/// A root that cannot be reached safely (a symlink) excludes the tier, an
/// error of the run (exit 3); the tally of its other root is kept.
#[test]
fn a_root_that_excludes_the_tier_still_writes_the_snapshot() {
    for entry in ENTRIES {
        let host = host(&yaml(
            "  /tmp/link: {symlink: /tmp/real}\n  /tmp/real: {.: {}}\n",
        ));
        let (report, lines) = run(&host, entry, &request(tier("profile:tmp-a-linked"), "100%"));
        assert_eq!(report.status, Status::Error, "{entry:?}: {report:#?}");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.candidate.as_deref().is_some_and(|c| c == "/tmp/link")),
            "{entry:?}: {report:#?}"
        );
        assert!(report.actions.is_empty(), "{entry:?}: {report:#?}");
        assert_snapshot_written(&host, entry, &lines);
    }
}

/// A `protect` that cannot be enumerated stops the run before any tier
/// (exit 3); the snapshot of the tally is written all the same.
#[test]
fn a_protect_that_cannot_be_enumerated_still_writes_the_snapshot() {
    let config = config::parse(Path::new("/tmp/config.yaml"), UNSAFE_PROTECT.as_bytes())
        .unwrap_or_else(|e| panic!("{e}"));
    let protections = executor::bind_protections(&config, 1000, false).expect("bound");
    for entry in ENTRIES {
        let host = host(&yaml(
            "  /tmp/guard-link: {symlink: /tmp/protected}\n  /tmp/protected: {inside: {}}\n",
        ));
        let mut tier = tier("profile:tmp-a");
        tier.protections = protections.clone();
        let (report, lines) = run(&host, entry, &request(tier, "100%"));
        assert_eq!(report.status, Status::Error, "{entry:?}: {report:#?}");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.message.contains("guard-link")),
            "{entry:?}: {report:#?}"
        );
        assert!(report.actions.is_empty(), "{entry:?}: {report:#?}");
        assert_snapshot_written(&host, entry, &lines);
    }
}

/// The host lock held by another reclaim to the deadline: exit 75, the
/// timeout logged, and the snapshot not written because the run's
/// deadline has passed (docs/architecture.md, Snapshot: never past it),
/// said in one line instead of nothing.
#[test]
fn a_host_lock_timeout_logs_it_and_why_no_snapshot_is_written() {
    let holder = format!(
        "processes:\n  - {{pid: 7, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], \
         environ: [PATH=/usr/bin]}}\nlocks:\n  - {{path: {HOST_LOCK}, holder: 7, mode: \
         exclusive}}\n"
    );
    for entry in ENTRIES {
        let host = host(&yaml(&holder));
        let mut request = request(tier("profile:tmp-a"), "100%");
        request.timeout = Some(Duration::from_secs(2));
        let (report, lines) = run(&host, entry, &request);
        assert_eq!(report.status, Status::LockTimeout, "{entry:?}: {report:#?}");
        assert!(
            has(
                &lines,
                "the host lock /run/diskgc/lock was still held by another reclaim at the deadline"
            ),
            "{entry:?}: {lines:#?}"
        );
        assert!(
            has(&lines, "index: synchronous tally of 1 tier(s)"),
            "{entry:?}: {lines:#?}"
        );
        assert!(
            has(
                &lines,
                "index: snapshot index-dev-0-42 not written: the run's deadline has passed"
            ),
            "{entry:?}: {lines:#?}"
        );
        assert_eq!(host.trace_count(Op::StateWrite), 0, "{entry:?}");
    }
}

/// A command whose executable is not installed: looked up under the host
/// lock, after the tally (a `missing` skip of the tier, exit 1), and never
/// on a run whose goal is met (no lookup, no skip row: an instant no-op).
#[test]
fn a_missing_executable_is_found_under_the_host_lock_only() {
    for entry in ENTRIES {
        let host = host(&yaml(""));
        let (report, lines) = run(
            &host,
            entry,
            &request(tier("profile:missing-cleaner"), "1M"),
        );
        assert_eq!(report.status, Status::Met, "{entry:?}: {report:#?}");
        assert!(report.skipped.is_empty(), "{entry:?}: {report:#?}");
        assert!(
            !has(&lines, "missing executable") && !has(&lines, "command executables"),
            "{entry:?}: {lines:#?}"
        );
        assert_eq!(host.trace_count(Op::ExecutableFile), 0, "{entry:?}");

        let host = common::host(&yaml(""));
        let (report, lines) = run(
            &host,
            entry,
            &request(tier("profile:missing-cleaner"), "100%"),
        );
        assert_eq!(report.status, Status::Exhausted, "{entry:?}: {report:#?}");
        assert!(report.errors.is_empty(), "{entry:?}: {report:#?}");
        assert_eq!(report.skipped.len(), 1, "{entry:?}: {report:#?}");
        assert_eq!(report.skipped[0].reason, Reason::Missing);
        assert_eq!(report.skipped[0].candidate, None);
        assert!(host_lock_taken(&host), "{entry:?}: {lines:#?}");
        let missing = lines
            .iter()
            .position(|l| l.contains("is missing executable"))
            .unwrap_or_else(|| panic!("{entry:?}: {lines:#?}"));
        let locked = lines
            .iter()
            .position(|l| l.starts_with("host lock /run/diskgc/lock taken"))
            .unwrap_or_else(|| panic!("{entry:?}: {lines:#?}"));
        assert!(locked < missing, "{entry:?}: {lines:#?}");
        assert_eq!(host.trace_count(Op::RunCommand), 0, "{entry:?}");
    }
}

/// A tier whose action this build does not reclaim is refused before the
/// gate is asked, with the same message on both entry points.
#[test]
fn an_action_without_a_reclaim_is_refused_alike_before_the_gate() {
    let mut odd = tier("profile:tmp-a");
    odd.source = SourceKind::CargoTarget;
    odd.action = Action::Delete {
        make_writable: false,
    };
    let mut messages = Vec::new();
    for entry in ENTRIES {
        let host = host(&yaml(""));
        let (report, _) = run(&host, entry, &request(odd.clone(), "100%"));
        assert_eq!(report.status, Status::UsageError, "{entry:?}: {report:#?}");
        assert_eq!(host.trace_count(Op::MayDelete), 0, "{entry:?}");
        assert_eq!(host.trace_count(Op::Statvfs), 0, "{entry:?}");
        messages.push(report.errors[0].message.clone());
    }
    assert_eq!(messages[0], messages[1]);
    assert!(
        messages[0].contains("`profile:tmp-a`") && messages[0].contains("--dry-run"),
        "{messages:#?}"
    );
}
