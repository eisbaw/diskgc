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
//! in the other. A storm (process 7 forks a child after each of the first
//! `2 * MAX_LISTINGS` process listings) made the first candidate's scan and
//! its one retry unsettled. Since TASK-364 such a scan is waited out under
//! the term that asked for it, in the plan and the reclaim, so a storm no
//! longer makes an unknown verdict; a cause that stays does: process 12, a
//! `newgrp`-like shell the kernel refuses to show (`unreadable_process`),
//! keeps every candidate, and the verdict must stay with the term that
//! met it.

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
    model_with(storm, "")
}

/// [`model`] with the refused shell, process 12.
fn refused_shell_model() -> ModelHost {
    model_with(
        0,
        "  - {pid: 12, exe: /usr/bin/tool, name: bash, gid: 1001, proc: denied}\n",
    )
}

/// [`refused_shell_model`] whose shell exits right after the dry run's
/// first scan (its second PID namespace read ends it), with the clock then
/// a second on: past the half second a blocking scan is reused, so a term
/// judged after that one scans again and finds the shell gone. The cause
/// lasted for the first term's verdict and ended before the next term's,
/// as a storm that ended between them did before TASK-364.
fn shell_exiting_after_the_first_scan_model() -> ModelHost {
    model_with(
        0,
        "  - {pid: 12, exe: /usr/bin/tool, name: bash, gid: 1001, proc: denied}\n\
         events:\n  - {at: {op: pid_namespace, nth: 2, when: after}, exit: 12}\n  - {at: \
         {op: pid_namespace, nth: 2, when: after}, advance: 1s}\n",
    )
}

/// [`model`] with more process items `procs`.
fn model_with(storm: usize, procs: &str) -> ModelHost {
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
{procs}{events}"#
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

/// The bite: the first term's in-use check cannot tell (`in_use_unknown`,
/// the refused shell), so `a-0` is kept under `gen-a`. Before TASK-337 the
/// plan judged `gen-ab` next, which scanned again (after a storm that had
/// just ended, eligible) and planned `a-0` under `gen-ab`; the reclaim
/// never does that (below).
#[test]
fn an_unknown_verdict_keeps_a_union_candidate_under_its_first_admitting_term() {
    let host = refused_shell_model();
    let report = executor::dry_run(&host, &request(UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert_eq!(
        plan_row(&report, "/tmp/a-0"),
        (
            "gen-a".to_owned(),
            Some(Reason::InUseUnknown),
            Some(UnknownCause::UnreadableProcess)
        ),
        "{:#?}",
        report.plan
    );
}

/// The bite with a cause that ends between the terms: `a-0` is kept under
/// `gen-a`, whose check met the shell. A plan that handed it on to
/// `gen-ab` would scan again, find the shell gone, and plan `a-0` eligible
/// under `gen-ab` (TASK-337's real-kernel case).
#[test]
fn a_verdict_stays_with_its_term_when_the_cause_ends_before_the_next() {
    let host = shell_exiting_after_the_first_scan_model();
    let report = executor::dry_run(&host, &request(UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert!(host.unfired_events().is_empty(), "the shell did not exit");
    assert_eq!(
        plan_row(&report, "/tmp/a-0"),
        (
            "gen-a".to_owned(),
            Some(Reason::InUseUnknown),
            Some(UnknownCause::UnreadableProcess)
        ),
        "{:#?}",
        report.plan
    );
    assert_eq!(
        plan_row(&report, "/tmp/b-1"),
        ("gen-ab".to_owned(), None, None),
        "the shell was gone for the next candidate: {:#?}",
        report.plan
    );
}

/// The reclaim with the same shell: `gen-a` is selected by its own
/// evidence, its one in-use check cannot tell, and `a-0` is skipped
/// `in_use_unknown` under `gen-a`, as the plan says.
#[test]
fn the_reclaim_skips_the_same_candidate_under_the_same_term() {
    let host = refused_shell_model();
    let report = executor::ensure(&host, &request(UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert_eq!(
        skips(&report, "/tmp/a-0"),
        [(
            "gen-a".to_owned(),
            Reason::InUseUnknown,
            Some(UnknownCause::UnreadableProcess)
        )],
        "{report:#?}"
    );
    assert!(report.actions.is_empty(), "{report:#?}");
    assert!(
        host.stat_path(Path::new("/tmp/a-0/f")).is_some(),
        "a-0 was not deleted"
    );
}

/// The storm (TASK-364): the first term's scan and its retry are
/// unsettled; the plan waits it out under `gen-a` and plans `a-0` eligible
/// there, and the reclaim waits it out under `gen-a` and deletes `a-0`
/// under `gen-a`: plan and reclaim agree on the term, and neither hands the
/// candidate on.
#[test]
fn a_storm_is_waited_out_under_the_first_admitting_term_in_plan_and_reclaim() {
    let host = model(2 * MAX_LISTINGS);
    let report = executor::dry_run(&host, &request(UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert_eq!(
        plan_row(&report, "/tmp/a-0"),
        ("gen-a".to_owned(), None, None),
        "{:#?}",
        report.plan
    );
    assert_eq!(
        plan_row(&report, "/tmp/b-1"),
        ("gen-ab".to_owned(), None, None)
    );
    storm_spent(&host);

    let host = model(2 * MAX_LISTINGS);
    let report = executor::ensure(&host, &request(UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert!(report.skipped.is_empty(), "{report:#?}");
    let acted: Vec<_> = report
        .actions
        .iter()
        .map(|action| (action.profile.as_str(), action.candidate.as_path()))
        .collect();
    assert_eq!(
        acted,
        [
            ("gen-a", Path::new("/tmp/a-0")),
            ("gen-ab", Path::new("/tmp/b-1"))
        ],
        "{report:#?}"
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
    let host = refused_shell_model();
    let report = executor::dry_run(&host, &request(FILTERED_UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    let unknown = (
        "gen-ab".to_owned(),
        Some(Reason::InUseUnknown),
        Some(UnknownCause::UnreadableProcess),
    );
    assert_eq!(plan_row(&report, "/tmp/a-0"), unknown, "{:#?}", report.plan);

    let host = refused_shell_model();
    let report = executor::ensure(&host, &request(FILTERED_UNION), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert_eq!(
        skips(&report, "/tmp/a-0"),
        [(
            "gen-ab".to_owned(),
            Reason::InUseUnknown,
            Some(UnknownCause::UnreadableProcess)
        )],
        "{report:#?}"
    );
}
