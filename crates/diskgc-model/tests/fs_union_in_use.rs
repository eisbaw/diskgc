//! TASK-337: in a filesystem union, the in-use check judges the candidate,
//! not a source term (docs/reference.md, the filesystem union rules). The
//! reclaim judges each term's own evidence under the candidate lock,
//! selects the first term it admits, and then checks the candidate once
//! against a process scan; an in-use refusal there is the candidate's
//! verdict under the selected term. The plan must predict that: a term
//! whose in-use check refuses (`in_use`, or `in_use_unknown` because the
//! process list never settled) does not hand the candidate to the next
//! term, which would scan again and, on a host whose fork storm has just
//! ended, plan it eligible under another profile.
//!
//! Seen on the real kernel under load (read_ahead_plan, seeds 1 and 6):
//! `/tmp/a-0` planned eligible under `gen-ab` in one run and under `gen-a`
//! in the other. Here the storm is deterministic: process 7 forks a child
//! after each of the first `2 * MAX_LISTINGS` process listings, so the
//! first candidate's scan and its one retry are both unsettled
//! (`unknown_cause: busy`); every later scan settles.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::engine::{Reason, UnknownCause};
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::inuse::MAX_LISTINGS;
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::Report;
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

/// `gen-ab` finds everything `gen-a` finds, and more: in
/// `profile:gen-a || profile:gen-ab`, `gen-a` is the first term of `a-*`.
const PROFILES: &str = r#"
- name: gen-a
  find: {roots: [/tmp], paths: ["a-*"], type: dir}
  action: delete
  examples: {match: [/tmp/a-1]}
- name: gen-ab
  find: {roots: [/tmp], paths: ["a-*", "b-*"], type: dir}
  action: delete
  examples: {match: [/tmp/a-1, /tmp/b-1]}
- name: has-tag
  where: {has: CACHEDIR.TAG}
"#;

const UNION: &str = "profile:gen-a || profile:gen-ab";

/// `gen-a` filtered out (no candidate has a tag): `gen-ab` is the first
/// term whose own judgement admits `a-0`.
const FILTERED_UNION: &str = "(profile:gen-a && filter:has-tag) || profile:gen-ab";

/// `/tmp` (tmpfs) with two old candidates, `a-0` (both terms) and `b-1`
/// (`gen-ab` only), judged in that order; process 7 is a readable
/// bystander that forks a child after each of the first `storm` process
/// listings.
fn model(storm: usize) -> ModelHost {
    let mut events: String = (1..=storm)
        .map(|listing| {
            format!(
                "  - {{at: {{op: pids, nth: {listing}, when: after}}, fork: {{pid: 7, child: {}}}}}\n",
                1000 + listing
            )
        })
        .collect();
    if !events.is_empty() {
        events.insert_str(0, "events:\n");
    }
    host(&format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, other_used: 500M, parent: 1}}
tree:
  /usr/bin/tool: {{}}
  /run/diskgc/lock: {{uid: 0, gid: 0}}
  /tmp/a-0: {{.: {{mtime: 2026-10-01T00:00:00Z}}, f: {{size: 8K, mtime: 2026-10-01T00:00:00Z}}}}
  /tmp/b-1: {{.: {{mtime: 2026-10-01T00:00:00Z}}, f: {{size: 4K, mtime: 2026-10-01T00:00:00Z}}}}
processes:
  - {{pid: 7, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{{path: /usr/bin/tool}}], environ: [PATH=/usr/bin]}}
{events}"#
    ))
}

fn request(select: &str) -> Request {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    let library = Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"));
    Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(select, &library, None, 1000).unwrap_or_else(|e| panic!("{e}")),
    }
}

/// A plan row as compared: profile, reason, cause of an unknown.
type Row = (String, Option<Reason>, Option<UnknownCause>);

fn plan_row(report: &Report, candidate: &str) -> Row {
    let rows: Vec<_> = report
        .plan
        .iter()
        .filter(|row| row.candidate.as_deref() == Some(Path::new(candidate)))
        .collect();
    assert_eq!(rows.len(), 1, "{candidate}: {:#?}", report.plan);
    (
        rows[0].profile.clone(),
        rows[0].reason,
        rows[0].unknown_cause,
    )
}

/// The reclaim's skips of `candidate`: profile, reason, cause of an
/// unknown.
fn skips(report: &Report, candidate: &str) -> Vec<(String, Reason, Option<UnknownCause>)> {
    report
        .skipped
        .iter()
        .filter(|skip| skip.candidate.as_deref() == Some(Path::new(candidate)))
        .map(|skip| (skip.profile.clone(), skip.reason, skip.unknown_cause))
        .collect()
}

/// Every fork event fired, and the run listed processes more than the
/// storm's `2 * MAX_LISTINGS` times: a scan after the storm was made, and
/// settled.
fn storm_spent(host: &ModelHost) {
    assert!(
        host.unfired_events().is_empty(),
        "the storm outlasted the run"
    );
    assert!(
        host.trace_count(Op::Pids) > 2 * MAX_LISTINGS,
        "a settled scan followed the storm"
    );
}

/// The bite: the first term's scan and its retry are unsettled, so `a-0`
/// is `in_use_unknown` under `gen-a`. Before TASK-337 the plan judged
/// `gen-ab` next, which scanned again after the storm and planned `a-0`
/// eligible under `gen-ab`; the reclaim never does that (below).
#[test]
fn an_unsettled_scan_keeps_a_union_candidate_under_its_first_admitting_term() {
    let host = model(2 * MAX_LISTINGS);
    let report = executor::dry_run(&host, &request(UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert_eq!(
        plan_row(&report, "/tmp/a-0"),
        (
            "gen-a".to_owned(),
            Some(Reason::InUseUnknown),
            Some(UnknownCause::Busy)
        ),
        "{:#?}",
        report.plan
    );
    assert_eq!(
        plan_row(&report, "/tmp/b-1"),
        ("gen-ab".to_owned(), None, None)
    );
    storm_spent(&host);
}

/// The reclaim under the same storm: `gen-a` is selected by its own
/// evidence, its one in-use check is unsettled twice, and `a-0` is
/// skipped `in_use_unknown` under `gen-a`, as the plan says. `b-1` is
/// deleted once the storm is over.
#[test]
fn the_reclaim_skips_the_same_candidate_under_the_same_term() {
    let host = model(2 * MAX_LISTINGS);
    let report = executor::ensure(&host, &request(UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert_eq!(
        skips(&report, "/tmp/a-0"),
        [(
            "gen-a".to_owned(),
            Reason::InUseUnknown,
            Some(UnknownCause::Busy)
        )],
        "{report:#?}"
    );
    let acted: Vec<_> = report
        .actions
        .iter()
        .map(|action| (action.profile.as_str(), action.candidate.as_path()))
        .collect();
    assert_eq!(acted, [("gen-ab", Path::new("/tmp/b-1"))], "{report:#?}");
    assert!(
        host.stat_path(Path::new("/tmp/a-0/f")).is_some(),
        "a-0 was not deleted"
    );
    storm_spent(&host);
}

/// Without a storm the union plans as before: `a-0` eligible under its
/// first term, `b-1` under the only term that finds it.
#[test]
fn a_settled_scan_plans_the_union_by_its_first_term() {
    let host = model(0);
    let report = executor::dry_run(&host, &request(UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert_eq!(
        plan_row(&report, "/tmp/a-0"),
        ("gen-a".to_owned(), None, None)
    );
    assert_eq!(
        plan_row(&report, "/tmp/b-1"),
        ("gen-ab".to_owned(), None, None)
    );
}

/// A term its own judgement refuses (here a filter) hands the candidate on
/// as before; the in-use verdict then belongs to the term that admits it.
/// Before TASK-337 the plan reported the first refusal it met (`filtered`
/// under `gen-a`) while the reclaim skipped `a-0` `in_use_unknown` under
/// `gen-ab`.
#[test]
fn the_in_use_verdict_belongs_to_the_term_that_admits_the_candidate() {
    let host = model(2 * MAX_LISTINGS);
    let report = executor::dry_run(&host, &request(FILTERED_UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    let unknown = (
        "gen-ab".to_owned(),
        Some(Reason::InUseUnknown),
        Some(UnknownCause::Busy),
    );
    assert_eq!(plan_row(&report, "/tmp/a-0"), unknown, "{:#?}", report.plan);
    storm_spent(&host);

    let host = model(2 * MAX_LISTINGS);
    let report = executor::ensure(&host, &request(FILTERED_UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert_eq!(
        skips(&report, "/tmp/a-0"),
        [(
            "gen-ab".to_owned(),
            Reason::InUseUnknown,
            Some(UnknownCause::Busy)
        )],
        "{report:#?}"
    );
    storm_spent(&host);
}
