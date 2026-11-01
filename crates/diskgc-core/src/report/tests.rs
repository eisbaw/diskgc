//! The report against docs/reference.md (Exit codes, Output): field names
//! and types, enum values, and the exit-code precedence are
//! read from the doc and compared, so the code and the contract cannot
//! drift (task-10 AC #5). JSON snapshots come with the contract freeze
//! (task-26).

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

use super::*;
use crate::doc::{code_blocks, reference_md, section, table};
use crate::engine::{Checkouts, RESERVED_REASONS, Reason};

/// Documented object fields this build always emits as `null`, and why.
const NULL_FOR_NOW: &[(&str, &str)] = &[("index", "no index before task-28/29")];

/// The JSON block of docs/reference.md, Output.
fn documented() -> Value {
    let output = section(&reference_md(), "## Output");
    let blocks = code_blocks(&output, "json");
    assert_eq!(blocks.len(), 1, "one json block in Output");
    serde_json::from_str(&blocks[0]).expect("the documented JSON object parses")
}

/// A report with every optional field set and every list holding an entry
/// (two plan entries: one eligible, one skipped).
fn full() -> Report {
    let mut r = Report::new(
        Status::Met,
        Some(&PathBuf::from("/home/mark/src/app")),
        false,
    );
    r.started_at = Some("2026-10-21T12:00:00Z".into());
    r.duration_ms = Some(123);
    r.seed = Some(42);
    r.mount = Some("/".into());
    r.min_free_bytes = Some(50);
    r.target_free_bytes = Some(80);
    r.avail_before_bytes = Some(10);
    r.avail_after_bytes = Some(90);
    let plan = |reason, bytes| PlanEntry {
        group: "select".into(),
        tier: "select".into(),
        profile: "cargo-target".into(),
        candidate: Some("/home/mark/src/app/target".into()),
        reason,
        estimated_bytes: bytes,
        operation: "cargo lock protocol".into(),
        checkouts: Checkouts::Allow,
        build_evidence: Some(BuildEvidence {
            marker: "CMakeCache.txt",
            source: "/home/mark/src/app".into(),
            idle_age_seconds: Some(3600),
        }),
        detail: None,
        process_blockers: None,
        unknown_cause: None,
    };
    r.plan = vec![plan(None, Some(7)), plan(Some(Reason::NotIdle), None)];
    r.actions = vec![ActionEntry {
        tier: "select".into(),
        profile: "cargo-target".into(),
        candidate: "/t".into(),
        avail_delta_bytes: Some(-3),
        duration_ms: 4,
        idle_age_seconds: Some(9),
        count: 1,
    }];
    r.skipped = vec![SkipEntry {
        tier: "select".into(),
        profile: "cargo-target".into(),
        candidate: Some("/u".into()),
        count: 1,
        examples: vec!["/u".into()],
        reason: Reason::InUse,
        detail: Some("pid 42 uses it".into()),
        process_blockers: Some(vec![crate::engine::ProcessBlocker {
            pid: 42,
            command: None,
            evidence: crate::engine::ProcessEvidence::Fd,
        }]),
        unknown_cause: None,
        estimated_bytes: Some(4096),
    }];
    r.held_by_deleted_files = Some(vec![HeldEntry {
        pid: 42,
        command: Some("cat".into()),
        bytes: 5,
    }]);
    r.overruns = vec![OverrunEntry {
        operation: "unlinkat".into(),
        path: "/x".into(),
        over_ms: 6,
    }];
    r.errors = vec![ErrorEntry {
        tier: Some("select".into()),
        profile: Some("cargo-target".into()),
        candidate: Some("/v".into()),
        cause: ErrorCause::Host,
        message: "EACCES".into(),
    }];
    r
}

fn emitted(report: &Report) -> Value {
    serde_json::from_str(&report.to_json()).expect("the report is valid JSON")
}

fn keys(v: &Value) -> Vec<String> {
    v.as_object()
        .unwrap_or_else(|| panic!("not an object: {v}"))
        .keys()
        .cloned()
        .collect()
}

/// The JSON type of `v`, for comparing the documented example's.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `want` and `got` have the same fields (names; serde_json's map does not
/// keep their order), each of the
/// documented type (a documented `null` takes any type: `seed`,
/// `reason`), recursively into the first element of every list.
fn same_shape(want: &Value, got: &Value, at: &str) {
    assert_eq!(keys(want), keys(got), "{at}: field names");
    for (name, w) in want.as_object().expect("object") {
        let g = &got[name];
        let here = format!("{at}.{name}");
        match (w, g) {
            (Value::Null, _) => {}
            (Value::Array(ws), Value::Array(gs)) => match (ws.first(), gs.first()) {
                (Some(w0 @ Value::Object(_)), Some(g0)) => same_shape(w0, g0, &here),
                (Some(w0), Some(g0)) => assert_eq!(kind(w0), kind(g0), "{here}[0]"),
                (Some(_), None) => panic!("{here}: the test report must hold an entry"),
                (None, _) => {}
            },
            (Value::Object(_), _) => same_shape(w, g, &here),
            _ => assert_eq!(kind(w), kind(g), "{here}: type"),
        }
    }
}

/// The values of a documented enum (`"a | b | c"`).
fn alternatives(v: &Value) -> BTreeSet<String> {
    v.as_str()
        .expect("an enum is documented as a string")
        .split('|')
        .map(|s| s.trim().to_owned())
        .collect()
}

#[test]
fn the_json_fields_are_the_docs() {
    let mut doc = documented();
    let mut got = emitted(&full());
    for (field, why) in NULL_FOR_NOW {
        assert!(
            doc.as_object_mut()
                .expect("object")
                .remove(*field)
                .is_some(),
            "{field} is listed as null for now ({why}) but is not in the doc"
        );
        let value = got.as_object_mut().expect("object").remove(*field);
        assert_eq!(
            value,
            Some(Value::Null),
            "{field}: emitted, and null ({why})"
        );
    }
    same_shape(&doc, &got, "report");
}

#[test]
fn every_status_and_reason_is_a_documented_value() {
    let doc = documented();
    let causes: BTreeSet<String> = ErrorCause::ALL
        .iter()
        .map(|cause| cause.as_str().to_owned())
        .collect();
    assert_eq!(causes, alternatives(&doc["errors"][0]["cause"]));
    let statuses: BTreeSet<String> = Status::ALL.iter().map(|s| s.as_str().to_owned()).collect();
    assert_eq!(statuses, alternatives(&doc["status"]), "status values");
    let checkouts: BTreeSet<String> = Checkouts::ALL
        .iter()
        .map(|c| c.as_str().to_owned())
        .collect();
    assert_eq!(
        checkouts,
        alternatives(&doc["plan"][0]["checkouts"]),
        "plan checkouts values"
    );
    let reasons = alternatives(&doc["skipped"][0]["reason"]);
    // Every reason the report can carry, and the reserved ones no code
    // produces yet: exactly the documented enum.
    let all_reasons: BTreeSet<String> = Reason::ALL
        .iter()
        .map(|reason| reason.as_str())
        .chain(RESERVED_REASONS.iter().map(|(reason, _)| *reason))
        .map(str::to_owned)
        .collect();
    assert_eq!(all_reasons, reasons, "the full documented skip enum");
    let evidence_values = alternatives(&doc["skipped"][0]["process_blockers"][0]["evidence"]);
    let expected_evidence: BTreeSet<String> = crate::engine::ProcessEvidence::ALL
        .iter()
        .map(|e| e.as_str().to_owned())
        .collect();
    assert_eq!(evidence_values, expected_evidence);
    for reason in Reason::ALL {
        let mut r = full();
        r.skipped[0].reason = reason;
        r.plan[1].reason = Some(reason);
        let got = emitted(&r);
        assert_eq!(got["skipped"][0]["reason"], reason.as_str());
        assert_eq!(got["plan"][1]["reason"], reason.as_str());
    }
    // Every status the report can carry shows up as that value.
    for status in Status::ALL {
        let mut r = full();
        r.status = status;
        assert_eq!(emitted(&r)["status"], status.as_str());
    }
}

fn skip(candidate: impl Into<PathBuf>, tier: &str, profile: &str, reason: Reason) -> SkipEntry {
    let candidate = candidate.into();
    SkipEntry {
        tier: tier.to_owned(),
        profile: profile.to_owned(),
        examples: vec![candidate.clone()],
        candidate: Some(candidate),
        count: 1,
        reason,
        detail: None,
        process_blockers: None,
        unknown_cause: None,
        estimated_bytes: None,
    }
}

#[test]
fn skips_aggregate_at_21_per_tier_profile_and_reason() {
    let mut r = Report::new(Status::Exhausted, Some(Path::new("/p")), false);
    r.skipped = (0..20)
        .map(|n| skip(format!("/p/{n}"), "tier-a", "profile-a", Reason::Locked))
        .collect();
    assert_eq!(emitted(&r)["skipped"].as_array().unwrap().len(), 20);
    assert_eq!(emitted(&r)["skipped"][0]["candidate"], "/p/0");

    let mut last = skip("/p/20", "tier-a", "profile-a", Reason::Locked);
    last.detail = Some("a particular holder".into());
    last.estimated_bytes = Some(4096);
    r.skipped.push(last);
    let compact = emitted(&r);
    assert_eq!(compact["skipped"].as_array().unwrap().len(), 1);
    assert!(compact["skipped"][0]["candidate"].is_null());
    assert_eq!(compact["skipped"][0]["count"], 21);
    assert!(compact["skipped"][0]["detail"].is_null());
    assert!(compact["skipped"][0]["process_blockers"].is_null());
    assert!(compact["skipped"][0]["estimated_bytes"].is_null());
    assert_eq!(
        compact["skipped"][0]["examples"].as_array().unwrap().len(),
        20
    );
    assert_eq!(compact["skipped"][0]["examples"][0], "/p/0");
    assert_eq!(compact["skipped"][0]["examples"][19], "/p/19");

    // Matching profiles in different tiers, and different reasons in the
    // same tier, retain their own rows and context.
    r.skipped
        .push(skip("/q", "tier-b", "profile-a", Reason::Locked));
    r.skipped
        .push(skip("/r", "tier-a", "profile-a", Reason::InUse));
    let got = emitted(&r);
    assert_eq!(got["skipped"].as_array().unwrap().len(), 3);
    assert_eq!(got["skipped"][1]["tier"], "tier-b");
    assert_eq!(got["skipped"][1]["candidate"], "/q");
    assert_eq!(got["skipped"][2]["reason"], "in_use");
}

#[test]
fn actions_keep_1000_details_then_one_counted_summary() {
    let mut r = Report::new(Status::Exhausted, Some(Path::new("/p")), false);
    r.actions = (0..1000)
        .map(|n| ActionEntry {
            tier: "tier".to_owned(),
            profile: "profile".to_owned(),
            candidate: format!("/p/{n}").into(),
            avail_delta_bytes: Some(1),
            duration_ms: 2,
            idle_age_seconds: None,
            count: 1,
        })
        .collect();
    let at_limit = emitted(&r);
    assert_eq!(at_limit["actions"].as_array().unwrap().len(), 1000);
    assert_eq!(at_limit["actions"][999]["candidate"], "/p/999");

    r.actions.push(ActionEntry {
        tier: "tier".to_owned(),
        profile: "profile".to_owned(),
        candidate: "/p/1000".into(),
        avail_delta_bytes: Some(-3),
        duration_ms: 4,
        idle_age_seconds: Some(25),
        count: 1,
    });
    let over_limit = emitted(&r);
    assert_eq!(over_limit["actions"].as_array().unwrap().len(), 1001);
    assert!(over_limit["actions"][1000]["candidate"].is_null());
    assert_eq!(over_limit["actions"][1000]["count"], 1);
    assert_eq!(over_limit["actions"][1000]["avail_delta_bytes"], -3);
    assert_eq!(over_limit["actions"][1000]["duration_ms"], 4);
    assert!(over_limit["actions"][1000]["idle_age_seconds"].is_null());

    r.actions.push(ActionEntry {
        tier: "other-tier".to_owned(),
        profile: "other-profile".to_owned(),
        candidate: "/q".into(),
        avail_delta_bytes: Some(5),
        duration_ms: 6,
        idle_age_seconds: None,
        count: 1,
    });
    let over_limit = emitted(&r);
    assert_eq!(over_limit["actions"].as_array().unwrap().len(), 1001);
    assert_eq!(over_limit["actions"][1000]["count"], 2);
    assert_eq!(over_limit["actions"][1000]["avail_delta_bytes"], 2);
    assert_eq!(over_limit["actions"][1000]["duration_ms"], 10);
    assert!(over_limit["actions"][1000]["tier"].is_null());
    assert!(over_limit["actions"][1000]["profile"].is_null());

    r.actions[1001].avail_delta_bytes = None;
    assert!(emitted(&r)["actions"][1000]["avail_delta_bytes"].is_null());
    assert_eq!(r.actions.len(), 1002, "details remain available internally");
}

/// On a usage error, fields that could not be determined are `null`; every
/// field is still there and `errors` says why.
#[test]
fn a_usage_error_report_keeps_every_field() {
    let r =
        Report::new(Status::UsageError, None, false).usage_error("--min-free: `50`: a bare number");
    let got = emitted(&r);
    let doc = documented();
    assert_eq!(keys(&doc), keys(&got));
    assert_eq!(got["status"], "usage_error");
    for field in [
        "mount",
        "path",
        "seed",
        "min_free_bytes",
        "target_free_bytes",
        "target_met",
        "avail_before_bytes",
        "avail_after_bytes",
        "index",
    ] {
        assert!(got[field].is_null(), "{field}: {}", got[field]);
    }
    assert_eq!(
        got["errors"][0]["message"],
        "--min-free: `50`: a bare number"
    );
    for field in ["tier", "profile", "candidate"] {
        assert!(got["errors"][0][field].is_null(), "errors[0].{field}");
    }
    assert_eq!(got["errors"][0]["cause"], "usage");
    assert_eq!(got["exit_code"], 2);
    assert_eq!(got["dry_run"], false);
    assert_eq!(got["tally_cut_short"], false);
    assert_eq!(r.exit_code(), 2);
    assert_eq!(r.summary(), None, "a usage error goes to stderr only");
}

#[test]
fn target_met_and_eligible_are_derived() {
    let mut r = full();
    assert_eq!(emitted(&r)["target_met"], true);
    r.avail_after_bytes = Some(79);
    assert_eq!(emitted(&r)["target_met"], false);
    let got = emitted(&r);
    assert_eq!(got["plan"][0]["eligible"], true);
    assert!(got["plan"][0]["reason"].is_null());
    assert_eq!(got["plan"][1]["eligible"], false);
    assert_eq!(got["plan"][1]["reason"], "not_idle");
    assert!(got["plan"][1]["estimated_bytes"].is_null());
    assert_eq!(got["actions"][0]["avail_delta_bytes"], -3);
    // Not measured: null, never 0; and an unknown final sample claims no
    // target met.
    r.actions[0].avail_delta_bytes = None;
    r.avail_after_bytes = None;
    let got = emitted(&r);
    assert!(got["actions"][0]["avail_delta_bytes"].is_null());
    assert!(got["avail_after_bytes"].is_null());
    assert!(got["target_met"].is_null());
    r.status = Status::Error;
    r.plan.clear();
    let text = r.summary().expect("prints");
    assert!(
        text.starts_with("error: unknown available") && text.contains("observed change unknown"),
        "{text}"
    );
}

/// The exit-code table's rows, code column, in order.
fn exit_rows() -> Vec<String> {
    let codes = section(&reference_md(), "## Exit codes (`ensure`)");
    table(&codes, "| Code |")
        .into_iter()
        .map(|row| row[0].clone())
        .collect()
}

/// The row of the table that holds `code`.
fn row_of(rows: &[String], code: u8) -> usize {
    rows.iter()
        .position(|cell| cell.split('/').any(|c| c.trim() == code.to_string()))
        .unwrap_or_else(|| panic!("exit code {code} is not in the table {rows:?}"))
}

#[test]
fn the_exit_codes_are_the_docs_table() {
    let rows = exit_rows();
    assert_eq!(rows, ["2", "130 / 143", "75", "0", "124", "3", "1"]);
    for status in Status::ALL {
        row_of(&rows, status.exit_code());
    }
    assert_eq!(Status::Interrupted(Signal::Int).exit_code(), 130);
    assert_eq!(Status::Interrupted(Signal::Term).exit_code(), 143);
    assert_eq!(Status::DryRun.exit_code(), 0);
}

/// "The first matching row wins": for every combination of what a reclaim
/// that ran can end with, [`conclude`] picks the status of the earliest
/// row of the doc's table among those that match.
#[test]
fn conclude_takes_the_first_matching_row_of_the_table() {
    let rows = exit_rows();
    for last in [Final::Met, Final::Below] {
        let met = last == Final::Met;
        for deadline in [false, true] {
            for errors in [false, true] {
                let mut matching = vec![Status::Exhausted];
                if met {
                    matching.push(Status::Met);
                }
                if deadline {
                    matching.push(Status::Deadline);
                }
                if errors {
                    matching.push(Status::Error);
                }
                let want = matching
                    .into_iter()
                    .min_by_key(|s| row_of(&rows, s.exit_code()))
                    .expect("at least exhausted");
                assert_eq!(
                    conclude(last, deadline, errors),
                    want,
                    "met {met}, deadline {deadline}, errors {errors}"
                );
            }
        }
    }
}

/// U1: a final sample that could not be taken is a run error (3) whatever
/// else happened, after a reclaim and after a lock timeout alike; 124 and
/// 75 need a fresh sample below `--min-free`. The doc says so next to the
/// table.
#[test]
fn an_unknown_final_sample_is_a_run_error_whatever_else_happened() {
    for deadline in [false, true] {
        for errors in [false, true] {
            assert_eq!(conclude(Final::Unknown, deadline, errors), Status::Error);
        }
    }
    assert_eq!(after_lock_timeout(Final::Unknown), Status::Error);
    assert_eq!(after_lock_timeout(Final::Below), Status::LockTimeout);
    assert_eq!(after_lock_timeout(Final::Met), Status::Met);
    assert_eq!(Final::of(None, 0), Final::Unknown);
    assert_eq!(Final::of(Some(5), 5), Final::Met);
    assert_eq!(Final::of(Some(4), 5), Final::Below);
    let codes = section(&reference_md(), "## Exit codes (`ensure`)")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for sentence in [
        "In a reclaiming run, available space decides 0, 75, 124, 3 and 1 by a fresh sample: a \
         sample that could not be taken is a run error (3); 75 and 124 require a fresh sample \
         below `--min-free`.",
        "An I/O failure in the first sample or startup host check is exit 3; an invalid `PATH` or unsupported kernel remains exit 2.",
        "A dry run with a host or planning error also exits 3, reports its partial plan and \
         errors, and uses `avail_after_bytes: null` when its final sample failed.",
    ] {
        assert!(codes.contains(sentence), "{sentence:?} not in: {codes}");
    }
}

#[test]
fn the_summary_is_silent_only_when_nothing_needed_doing() {
    let mut quiet = Report::new(Status::Met, Some(&PathBuf::from("/p")), false);
    quiet.mount = Some("/".into());
    assert_eq!(quiet.summary(), None);
    let mut freed = full();
    freed.errors.clear();
    freed.plan.clear();
    let text = freed.summary().expect("something was done");
    assert!(text.starts_with("met: 90 B available on / "), "{text}");
    assert!(
        text.contains("1 reclaimed (observed change -3 B)"),
        "{text}"
    );
    assert!(text.contains("\n  reclaimed /t: -3 B"), "{text}");
    let mut dry = full();
    dry.status = Status::DryRun;
    dry.dry_run = true;
    let text = dry.summary().expect("a dry run always prints");
    assert!(text.starts_with("dry run: 10 B available on / "), "{text}");
    // One candidate, eligible in its row: the not_idle row of the same
    // candidate does not make it read as kept.
    assert!(
        text.contains("would reclaim /home/mark/src/app/target: est. 7 B"),
        "{text}"
    );
    assert!(!text.contains("kept /home/mark/src/app/target"), "{text}");
    assert!(text.contains("took 123 ms"), "{text}");
    assert!(
        text.contains("held by deleted files: 1 process(es), 5 B"),
        "{text}"
    );
    let mut exhausted = full();
    exhausted.status = Status::Exhausted;
    exhausted.plan.clear();
    assert!(
        exhausted
            .summary()
            .expect("prints")
            .starts_with("nothing more could be reclaimed: ")
    );
}

#[test]
fn human_failure_names_kept_targets_and_held_deleted_files() {
    let mut report = full();
    report.status = Status::Exhausted;
    report.plan.clear();
    report.actions.clear();
    report.errors.clear();
    report.duration_ms = Some(1_234);
    report.skipped.push(SkipEntry {
        tier: "select".into(),
        profile: "cargo-target".into(),
        candidate: Some("/busy-build".into()),
        count: 1,
        examples: Vec::new(),
        reason: Reason::Locked,
        detail: None,
        process_blockers: None,
        unknown_cause: None,
        estimated_bytes: None,
    });
    report.skipped.push(SkipEntry {
        tier: "select".into(),
        profile: "cargo-target".into(),
        candidate: Some("/already-empty".into()),
        count: 1,
        examples: Vec::new(),
        reason: Reason::Empty,
        detail: None,
        process_blockers: None,
        unknown_cause: None,
        estimated_bytes: None,
    });
    let text = report.summary().expect("summary");
    assert!(text.contains("2 kept, 0 error(s); took 1.234 s"), "{text}");
    assert!(text.contains("kept /u: in_use"), "{text}");
    assert!(text.contains("kept /busy-build: locked\n"), "{text}");
    assert!(!text.contains("/already-empty"), "{text}");
    assert!(
        text.contains("held by deleted files: 1 process(es), 5 B; top: pid 42 (cat, 5 B)"),
        "{text}"
    );
}

#[test]
fn human_kept_reasons_are_bounded_and_paths_cannot_inject_lines() {
    let mut report = Report::new(Status::Exhausted, Some(Path::new("/tmp")), false);
    report.mount = Some("/tmp".into());
    for index in 0..7 {
        report.skipped.push(SkipEntry {
            tier: "t".into(),
            profile: "p".into(),
            candidate: Some(format!("/tmp/held-{index}").into()),
            count: 1,
            examples: Vec::new(),
            reason: Reason::InUse,
            detail: None,
            process_blockers: None,
            unknown_cause: None,
            estimated_bytes: None,
        });
    }
    report.skipped[0].candidate = Some("/tmp/held-0\nspoofed".into());
    let text = report.summary().expect("summary");
    assert!(text.contains("7 kept: in_use; examples: "), "{text}");
    assert!(text.contains("(2 more)"), "{text}");
    assert!(text.contains("held-0\\nspoofed"), "{text}");
    assert!(!text.lines().any(|line| line == "spoofed"), "{text}");
    assert!(!text.contains("held-6"), "{text}");
}

/// Another user's entries on a shared host (TASK-140): two or more
/// `unreadable` or `not_owner` candidates read as one line each per reason,
/// while one stays a line of its own and other reasons are unchanged.
#[test]
fn human_groups_other_users_entries_from_two() {
    let mut report = Report::new(Status::Exhausted, Some(Path::new("/tmp")), false);
    report.mount = Some("/tmp".into());
    let kept = |name: &str, reason, detail: &str| SkipEntry {
        tier: "t".into(),
        profile: "p".into(),
        candidate: Some(format!("/tmp/{name}").into()),
        count: 1,
        examples: Vec::new(),
        reason,
        detail: Some(detail.into()),
        process_blockers: None,
        unknown_cause: None,
        estimated_bytes: None,
    };
    let denied = "cannot list it (fail closed): open /tmp/x: EACCES (13)";
    let foreign = "owned by uid 1000, not by uid 1001";
    report.skipped = vec![
        kept("a", Reason::Unreadable, denied),
        kept("b", Reason::Unreadable, denied),
        kept("c", Reason::Unreadable, denied),
        kept("d", Reason::NotOwner, foreign),
        kept("e", Reason::NotOwner, foreign),
        kept("f", Reason::KeepMarker, "inside the candidate"),
        kept("g", Reason::KeepMarker, "inside the candidate"),
    ];
    let text = report.summary().expect("summary");
    let lines: Vec<&str> = text.lines().skip(1).collect();
    assert_eq!(
        lines,
        [
            format!("  3 kept: unreadable; examples: /tmp/a, /tmp/b, /tmp/c; e.g. {denied}"),
            format!("  2 kept: not_owner; examples: /tmp/d, /tmp/e; e.g. {foreign}"),
            "  kept /tmp/f: keep marker, inside the candidate".to_owned(),
            "  kept /tmp/g: keep marker, inside the candidate".to_owned(),
        ],
        "{text}"
    );

    report.skipped.truncate(1);
    let text = report.summary().expect("summary");
    assert!(
        text.contains(&format!("  kept /tmp/a: unreadable, {denied}")),
        "{text}"
    );
}

#[test]
fn human_preflight_refusal_does_not_invent_a_kept_candidate() {
    let mut report = Report::new(Status::Exhausted, Some(Path::new("/tmp")), false);
    report.skipped.push(SkipEntry {
        tier: "select".into(),
        profile: "cargo-target".into(),
        candidate: None,
        count: 1,
        examples: Vec::new(),
        reason: Reason::Missing,
        detail: None,
        process_blockers: None,
        unknown_cause: None,
        estimated_bytes: None,
    });
    let text = report.summary().expect("preflight refusal must be visible");
    assert!(
        text.contains("0 kept, 1 profile(s) skipped without a candidate"),
        "{text}"
    );
    assert!(
        text.contains("skipped profile cargo-target [select]: missing"),
        "{text}"
    );
    assert!(
        !text.contains("1 kept") && !text.contains("examples: "),
        "{text}"
    );
    report.status = Status::Met;
    assert!(
        report.summary().is_some(),
        "a preflight refusal should not make a met result silent"
    );
}

#[test]
fn human_held_deleted_summary_orders_top_pids_and_counts_bytes_once() {
    let mut report = Report::new(Status::Exhausted, Some(Path::new("/tmp")), false);
    report.held_by_deleted_files = Some(vec![
        HeldEntry {
            pid: 10,
            command: Some("a".into()),
            bytes: 0,
        },
        HeldEntry {
            pid: 11,
            command: Some("b".into()),
            bytes: 5,
        },
        HeldEntry {
            pid: 12,
            command: Some("c".into()),
            bytes: 2,
        },
        HeldEntry {
            pid: 13,
            command: Some("d".into()),
            bytes: 7,
        },
    ]);
    let text = report.summary().expect("summary");
    // pid 10 holds 0 B: it pins nothing and is left out.
    assert!(text.contains("3 process(es), 14 B"), "{text}");
    assert!(
        text.contains("pid 13 (d, 7 B), pid 11 (b, 5 B), pid 12 (c, 2 B)"),
        "{text}"
    );
    assert!(!text.contains("pid 10"), "{text}");
    report.status = Status::Met;
    assert!(
        report
            .summary()
            .expect("a held deleted file is actionable even when the goal is met")
            .contains("held by deleted files:")
    );
}

#[test]
fn rounded_space_comparison_includes_exact_bytes_for_both_values() {
    let mut report = Report::new(Status::Exhausted, Some(Path::new("/tmp")), false);
    report.mount = Some("/tmp".into());
    report.avail_after_bytes = Some((15 << 30) + (1 << 20));
    report.min_free_bytes = Some((15 << 30) + (2 << 20));
    report.target_free_bytes = report.min_free_bytes;
    let text = report.summary().expect("summary");
    assert!(
        text.contains("15.0 GiB (16107175936 B) available"),
        "{text}"
    );
    assert!(text.contains("min-free 15.0 GiB (16108224512 B)"), "{text}");
    report.status = Status::DryRun;
    report.dry_run = true;
    report.avail_before_bytes = report.avail_after_bytes;
    let text = report.summary().expect("dry run");
    assert!(
        text.contains("15.0 GiB (16107175936 B) available"),
        "{text}"
    );
}

/// TASK-144: `in_use_unknown` skips get a plain line saying the host was
/// too busy and to run again; other reasons do not.
#[test]
fn human_summary_says_a_busy_host_kept_candidates_and_to_run_again() {
    let mut report = Report::new(Status::Exhausted, Some(Path::new("/tmp")), false);
    report.mount = Some("/tmp".into());
    report.skipped.push(skip("/tmp/a", "t", "p", Reason::InUse));
    let text = report.summary().expect("summary");
    assert!(!text.contains("too busy"), "{text}");
    report
        .skipped
        .push(skip("/tmp/b", "t", "p", Reason::InUseUnknown));
    report
        .skipped
        .push(skip("/tmp/c", "t", "p", Reason::InUseUnknown));
    let text = report.summary().expect("summary");
    assert!(text.contains("kept /tmp/b: in_use_unknown"), "{text}");
    assert!(
        text.contains(
            "the host was too busy to check 2 candidate(s) for use (in_use_unknown): kept; \
             run again when the host is less busy"
        ),
        "{text}"
    );
}

/// TASK-263, TASK-169 (mped S2): an `in_use_unknown` whose cause stays
/// (`incomplete_process_list`, `loop_device`) says its detail and is not
/// counted as the host's load, whatever the detail's words; a `busy` one,
/// or one without a cause, is. The JSON carries the cause.
#[test]
fn human_summary_names_a_standing_cause_instead_of_a_busy_host() {
    use crate::engine::UnknownCause;
    let with = |candidate: &str, cause: UnknownCause, detail: &str| SkipEntry {
        unknown_cause: Some(cause),
        ..tier_skip("t", candidate, Reason::InUseUnknown, detail)
    };
    let mut report = Report::new(Status::Exhausted, Some(Path::new("/tmp")), false);
    report.mount = Some("/tmp".into());
    report.skipped.push(with(
        "/tmp/a",
        UnknownCause::LoopDevice,
        "loop device 7:4 is backed by /tmp/old.img (deleted)",
    ));
    report.skipped.push(with(
        "/tmp/b",
        UnknownCause::IncompleteProcessList,
        "the held /proc is mounted with hidepid",
    ));
    let text = report.summary().expect("summary");
    assert!(!text.contains("too busy"), "{text}");
    assert!(
        text.contains("kept /tmp/a: in_use_unknown, loop device 7:4 is backed by"),
        "{text}"
    );
    assert!(
        text.contains("kept /tmp/b: in_use_unknown, the held /proc is mounted with hidepid"),
        "{text}"
    );
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("JSON");
    assert_eq!(json["skipped"][0]["unknown_cause"], "loop_device");
    assert_eq!(
        json["skipped"][1]["unknown_cause"],
        "incomplete_process_list"
    );
    report.skipped.push(with(
        "/tmp/c",
        UnknownCause::Busy,
        "loop device 7:9 in words only: the cause decides",
    ));
    report
        .skipped
        .push(skip("/tmp/d", "t", "p", Reason::InUseUnknown));
    let text = report.summary().expect("summary");
    assert!(
        text.contains("the host was too busy to check 2 candidate(s)"),
        "{text}"
    );
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("JSON");
    assert_eq!(json["skipped"][3]["unknown_cause"], serde_json::Value::Null);
    // Every cause's name is documented (docs/reference.md, Output).
    let docs = section(&reference_md(), "## Output");
    for cause in UnknownCause::ALL {
        assert!(
            docs.contains(&format!("`{}`", cause.as_str())),
            "{} undocumented",
            cause.as_str()
        );
    }
}

fn tier_skip(tier: &str, candidate: &str, reason: Reason, detail: &str) -> SkipEntry {
    SkipEntry {
        detail: Some(detail.into()),
        ..skip(candidate, tier, "cargo-target", reason)
    }
}

fn tier_action(tier: &str, candidate: &str, delta: i64) -> ActionEntry {
    ActionEntry {
        tier: tier.into(),
        profile: "cargo-target".into(),
        candidate: candidate.into(),
        avail_delta_bytes: Some(delta),
        duration_ms: 1,
        idle_age_seconds: None,
        count: 1,
    }
}

/// TASK-155 (walk dumps 25, 26): a two-tier policy judges a target once per
/// tier. The summary has one line per candidate with its final outcome: a
/// target a later tier reclaimed is not also kept, and a kept one carries
/// the reason of the last tier that judged it, once.
#[test]
fn human_summary_has_one_final_outcome_per_candidate_across_tiers() {
    let mut report = Report::new(Status::Met, Some(Path::new("/home/t")), false);
    report.mount = Some("/home/t".into());
    report.skipped = vec![
        tier_skip(
            "stale",
            "/s/lib/target",
            Reason::Protected,
            "overlaps a protected match",
        ),
        tier_skip(
            "stale",
            "/s/fw/target",
            Reason::NotIdle,
            "last build 1 h ago; idle_for 7d required",
        ),
        tier_skip(
            "stale",
            "/s/web/target",
            Reason::NotIdle,
            "last build 1 h ago; idle_for 7d required",
        ),
        tier_skip(
            "stale",
            "/s/slow/target",
            Reason::NotIdle,
            "last build 1 h ago; idle_for 7d required",
        ),
        tier_skip(
            "recent",
            "/s/lib/target",
            Reason::Protected,
            "overlaps a protected match",
        ),
        SkipEntry {
            process_blockers: Some(vec![
                crate::engine::ProcessBlocker {
                    pid: 4269,
                    command: Some("webapp".into()),
                    evidence: crate::engine::ProcessEvidence::Exe,
                },
                crate::engine::ProcessBlocker {
                    pid: 4269,
                    command: Some("webapp".into()),
                    evidence: crate::engine::ProcessEvidence::TargetExe,
                },
            ]),
            ..tier_skip(
                "recent",
                "/s/web/target",
                Reason::InUse,
                "a very long evidence list",
            )
        },
    ];
    report.actions = vec![tier_action("recent", "/s/fw/target", 1 << 20)];
    let text = report.summary().expect("summary");
    assert!(
        text.contains("1 reclaimed (observed change +1.0 MiB), 3 kept, 0 error(s)"),
        "{text}"
    );
    assert!(
        text.contains("\n  reclaimed /s/fw/target: +1.0 MiB [recent]"),
        "{text}"
    );
    assert!(!text.contains("kept /s/fw/target"), "{text}");
    assert_eq!(text.matches("/s/lib/target").count(), 1, "{text}");
    assert!(
        text.contains("kept /s/web/target: in_use, used by pid 4269 (webapp) [recent]"),
        "{text}"
    );
    assert!(!text.contains("web/target: not_idle"), "{text}");
    assert!(
        text.contains(
            "kept /s/slow/target: not_idle, last build 1 h ago; idle_for 7d required [stale]"
        ),
        "{text}"
    );
}

/// A reclaim that deleted entries and then stopped is an action and a skip
/// of the same candidate: one line saying both.
#[test]
fn a_part_way_reclaim_is_one_line_naming_why_it_stopped() {
    let mut report = Report::new(Status::Exhausted, Some(Path::new("/w")), false);
    report.actions = vec![tier_action("t", "/w/a/target", 4096)];
    report.skipped = vec![tier_skip(
        "t",
        "/w/a/target",
        Reason::Changed,
        "a profile changed",
    )];
    let text = report.summary().expect("summary");
    assert!(
        text.contains("1 reclaimed (observed change +4.0 KiB), 0 kept"),
        "{text}"
    );
    assert!(
        text.contains(
            "reclaimed part-way /w/a/target: +4.0 KiB, then kept: changed, a profile changed"
        ),
        "{text}"
    );
}

fn plan_row(tier: &str, candidate: &str, reason: Option<Reason>, bytes: Option<u64>) -> PlanEntry {
    PlanEntry {
        group: "g".into(),
        tier: tier.into(),
        profile: "cargo-target".into(),
        candidate: Some(candidate.into()),
        reason,
        estimated_bytes: bytes,
        operation: "cargo lock protocol".into(),
        checkouts: Checkouts::Allow,
        build_evidence: None,
        detail: reason.map(|_| "last build 1 h ago; idle_for 7d required".into()),
        process_blockers: None,
        unknown_cause: None,
    }
}

/// TASK-147: a dry run says what a reclaim would take, by the estimates,
/// and whether that reaches the goal; each candidate once, the earliest
/// tier that would take it deciding.
#[test]
fn a_dry_run_shows_what_would_be_reclaimed_against_the_goal() {
    let mut report = Report::new(Status::DryRun, Some(Path::new("/w")), true);
    report.mount = Some("/w".into());
    report.duration_ms = Some(5);
    report.avail_before_bytes = Some(100);
    report.min_free_bytes = Some(200);
    report.target_free_bytes = Some(250);
    report.plan = vec![
        plan_row("stale", "/w/a", Some(Reason::NotIdle), None),
        plan_row("stale", "/w/b", Some(Reason::NotIdle), None),
        plan_row("stale", "/w/c", Some(Reason::NotIdle), None),
        plan_row("recent", "/w/a", None, Some(100)),
        plan_row("recent", "/w/b", None, Some(60)),
        plan_row("recent", "/w/c", Some(Reason::NotIdle), None),
    ];
    let text = report.summary().expect("a dry run prints");
    assert!(
        text.starts_with(
            "dry run: 100 B available on /w (min-free 200 B, target-free 250 B); would reclaim 2 \
             of 2 eligible, est. 160 B of 150 B needed: the goal is reachable by estimate; 1 kept \
             (took 5 ms):"
        ),
        "{text}"
    );
    assert!(
        text.contains("\n  would reclaim /w/a: est. 100 B [recent]"),
        "{text}"
    );
    assert!(
        text.contains("\n  would reclaim /w/b: est. 60 B [recent]"),
        "{text}"
    );
    assert!(
        text.contains("\n  kept /w/c: not_idle, last build 1 h ago; idle_for 7d required [recent]"),
        "{text}"
    );
    assert_eq!(text.matches("/w/a").count(), 1, "{text}");
    assert!(
        text.contains("operations: cargo-target: cargo lock protocol"),
        "{text}"
    );

    // Less needed: the second eligible one is not needed.
    report.target_free_bytes = Some(200);
    let text = report.summary().expect("a dry run prints");
    assert!(
        text.contains("\n  eligible, not needed /w/b: est. 60 B"),
        "{text}"
    );
    // More needed than all estimates: short, by how much.
    report.target_free_bytes = Some(400);
    let text = report.summary().expect("a dry run prints");
    assert!(
        text.contains("est. 160 B of 300 B needed: short of the goal by 140 B by estimate"),
        "{text}"
    );
    // Already at min-free: a reclaim takes nothing.
    report.avail_before_bytes = Some(300);
    let text = report.summary().expect("a dry run prints");
    assert!(
        text.contains("a reclaim would take nothing (2 eligible, 1 kept)"),
        "{text}"
    );
    assert!(text.contains("eligible, not needed /w/a"), "{text}");
}

/// TASK-229: a dry run the deadline cut lists only what it judged
/// (TASK-227); its counts must not read as a complete plan, and the text
/// says what to do. The report does not count the candidates found but not
/// judged, so the text names no number.
#[test]
fn a_dry_run_cut_by_the_deadline_says_its_plan_is_partial() {
    let mut report = Report::new(Status::Deadline, Some(Path::new("/tmp")), true);
    report.mount = Some("/tmp".into());
    report.duration_ms = Some(30_001);
    report.avail_before_bytes = Some(100);
    report.min_free_bytes = Some(200);
    report.target_free_bytes = Some(250);
    report.plan = vec![
        plan_row("t", "/tmp/a", None, Some(100)),
        plan_row("t", "/tmp/b", Some(Reason::NotIdle), None),
    ];
    let text = report.summary().expect("a dry run prints");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "dry run, deadline: 100 B available on /tmp (min-free 200 B, target-free 250 B); \
         judged before the deadline: would reclaim 1 of 1 eligible, est. 100 B of 150 B needed: \
         short of the goal by 50 B by estimate; 1 kept (took 30.001 s):",
        "{text}"
    );
    assert_eq!(
        lines[1],
        "  the deadline passed: only the candidates judged before it are listed, not any found \
         but not yet judged; run again with a larger --timeout for a complete plan",
        "{text}"
    );
    // Nothing eligible among those judged is no verdict on the rest.
    report.plan = vec![plan_row("t", "/tmp/b", Some(Reason::NotIdle), None)];
    let text = report.summary().expect("a dry run prints");
    assert!(
        text.contains(
            "judged before the deadline: nothing is eligible, 150 B needed: the goal is not \
             reachable by these; 1 kept"
        ),
        "{text}"
    );
    // A complete dry run says none of it.
    report.status = Status::DryRun;
    let text = report.summary().expect("a dry run prints");
    assert!(!text.contains("deadline"), "{text}");
    assert!(text.contains("the goal is not reachable; 1 kept"), "{text}");
}

/// TASK-229: a reclaim the deadline stopped counts only the candidates it
/// reached; the summary says so and what to do.
#[test]
fn a_reclaim_cut_by_the_deadline_says_its_counts_are_partial() {
    let mut report = Report::new(Status::Deadline, Some(Path::new("/w")), false);
    report.actions = vec![tier_action("t", "/w/a/target", 4096)];
    let text = report.summary().expect("summary");
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("deadline: "), "{text}");
    assert_eq!(
        lines[1],
        "  the deadline passed: the counts cover only the candidates reached before it; run \
         again, with a larger --timeout, to reclaim more",
        "{text}"
    );
    report.status = Status::Exhausted;
    let text = report.summary().expect("summary");
    assert!(!text.contains("deadline"), "{text}");
}

/// TASK-193: whether a run was a dry run is the report's `dry_run`, not a
/// guess from its status or plan. A dry run that failed before its first
/// plan row (a protect scan failure) still reads as a dry run, never as a
/// reclaim that freed nothing; a live run that failed reads as live.
#[test]
fn a_failed_dry_run_without_a_plan_reads_as_a_dry_run() {
    let mut report = Report::new(Status::Error, Some(Path::new("/w")), true);
    report.mount = Some("/w".into());
    report
        .errors
        .push(ErrorEntry::run(ErrorCause::Host, "protect scan: EIO"));
    let text = report.summary().expect("a failed dry run prints");
    assert!(text.starts_with("dry run, error: "), "{text}");
    assert!(!text.contains("reclaimed"), "{text}");
    assert!(text.contains("1 error(s): see stderr"), "{text}");
    assert_eq!(emitted(&report)["dry_run"], true);
    assert_eq!(emitted(&report)["status"], "error");

    report.dry_run = false;
    let text = report.summary().expect("a failed run prints");
    assert!(text.starts_with("error: "), "{text}");
    assert!(text.contains("0 reclaimed"), "{text}");
}

/// TASK-193: a dry run over thousands of candidates is as short as the live
/// summary: kept candidates grouped by reason with examples above
/// KEPT_EXAMPLES_MAX, eligible ones listed up to RECLAIMED_LINES_MAX per
/// verdict and the rest counted with their estimate.
#[test]
fn a_dry_run_over_thousands_of_candidates_is_bounded() {
    let mut report = Report::new(Status::DryRun, Some(Path::new("/tmp")), true);
    report.mount = Some("/tmp".into());
    report.avail_before_bytes = Some(0);
    report.min_free_bytes = Some(1);
    report.target_free_bytes = Some(1_000);
    report.plan = (0..6_000)
        .map(|n| plan_row("t", &format!("/tmp/kept-{n}"), Some(Reason::InUse), None))
        .chain((0..400).map(|n| plan_row("t", &format!("/tmp/old-{n}"), None, Some(10))))
        .chain((0..3).map(|n| {
            plan_row(
                "t",
                &format!("/tmp/marked-{n}"),
                Some(Reason::KeepMarker),
                None,
            )
        }))
        .collect();
    report.plan[6_400].detail = Some("inside the candidate".into());
    let text = report.summary().expect("a dry run prints");
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() < 40, "{} lines:\n{text}", lines.len());
    assert!(
        text.contains("would reclaim 100 of 400 eligible, est. 1000 B of 1000 B needed"),
        "{text}"
    );
    assert_eq!(
        text.matches("\n  would reclaim /tmp/old-").count(),
        10,
        "{text}"
    );
    assert!(
        text.contains("\n  90 more would reclaim: est. 900 B (--json lists every candidate)"),
        "{text}"
    );
    assert_eq!(
        text.matches("\n  eligible, not needed /tmp/old-").count(),
        10,
        "{text}"
    );
    assert!(
        text.contains("\n  290 more eligible, not needed: est. 2.8 KiB"),
        "{text}"
    );
    assert!(
        text.contains(
            "\n  6000 kept: in_use; examples: /tmp/kept-0, /tmp/kept-1, /tmp/kept-2, \
             /tmp/kept-3, /tmp/kept-4 (5995 more); e.g. last build"
        ),
        "{text}"
    );
    // TASK-192: the keep-marker reason reads as words, not as its JSON name.
    assert!(
        text.contains("\n  kept /tmp/marked-0: keep marker, inside the candidate"),
        "{text}"
    );
    assert_eq!(
        emitted(&report)["plan"]
            .as_array()
            .expect("the JSON plan")
            .len(),
        6_403,
        "the JSON keeps every row"
    );
    assert_eq!(emitted(&report)["plan"][6_400]["reason"], "keep_marker");
}

/// A deleted file of 0 B pins no space: no held line, and a met run with
/// nothing else to say stays silent.
#[test]
fn zero_byte_holders_of_deleted_files_are_not_reported() {
    let mut report = Report::new(Status::Met, Some(Path::new("/tmp")), false);
    report.held_by_deleted_files = Some(vec![HeldEntry {
        pid: 4274,
        command: Some("python3.14".into()),
        bytes: 0,
    }]);
    assert_eq!(report.summary(), None);
    report.status = Status::DryRun;
    report.dry_run = true;
    let text = report.summary().expect("a dry run prints");
    assert!(!text.contains("held by deleted files"), "{text}");
}

#[test]
fn a_dry_run_with_nothing_eligible_says_the_goal_is_not_reachable() {
    let mut report = Report::new(Status::DryRun, Some(Path::new("/w")), true);
    report.avail_before_bytes = Some(100);
    report.min_free_bytes = Some(200);
    report.target_free_bytes = Some(200);
    report.plan = vec![plan_row("t", "/w/a", Some(Reason::NotIdle), None)];
    let text = report.summary().expect("a dry run prints");
    assert!(
        text.contains("; nothing is eligible, 100 B needed: the goal is not reachable; 1 kept"),
        "{text}"
    );
}

/// TASK-144 with the per-candidate fold: an `in_use_unknown` kept line says
/// plainly that the host was too busy (not the attempt diagnostics), and
/// the busy count is of final verdicts: a later tier that reclaimed the
/// candidate, or refused it for another reason, removes it from the count.
#[test]
fn busy_host_count_and_wording_follow_the_final_verdict() {
    let mut report = Report::new(Status::Exhausted, Some(Path::new("/w")), false);
    let long = "the host was too busy to check whether it is in use: 3 attempt(s) found no \
                in-use scan within the freshness bound of 1000 ms; kept, run again when the \
                host is less busy";
    report.skipped = vec![
        tier_skip("a", "/w/x", Reason::InUseUnknown, long),
        tier_skip("a", "/w/y", Reason::InUseUnknown, long),
        tier_skip("a", "/w/z", Reason::InUseUnknown, long),
        tier_skip(
            "b",
            "/w/y",
            Reason::NotIdle,
            "last build 1 m ago; idle_for 1h required",
        ),
    ];
    report.actions = vec![tier_action("b", "/w/z", 1)];
    let text = report.summary().expect("summary");
    assert!(
        text.contains("\n  kept /w/x: in_use_unknown, the host was too busy to check whether it is in use [a]\n"),
        "{text}"
    );
    assert!(!text.contains("attempt(s)"), "{text}");
    assert!(
        text.contains("the host was too busy to check 1 candidate(s) for use (in_use_unknown): kept; run again when the host is less busy"),
        "{text}"
    );
    assert!(
        text.contains("1 reclaimed (observed change +1 B), 2 kept"),
        "{text}"
    );

    let mut dry = Report::new(Status::DryRun, Some(Path::new("/w")), true);
    dry.plan = vec![PlanEntry {
        detail: Some(long.into()),
        ..plan_row("a", "/w/x", Some(Reason::InUseUnknown), None)
    }];
    let text = dry.summary().expect("a dry run prints");
    assert!(
        text.contains(
            "kept /w/x: in_use_unknown, the host was too busy to check whether it is in use"
        ) && text.contains("too busy to check 1 candidate(s) for use"),
        "{text}"
    );
}

/// TASK-190: a candidate kept `in_use_unknown` because a process of the
/// user cannot be inspected names that process ("cannot be inspected"),
/// not the host's load, and never says "used by": nobody saw a use.
#[test]
fn an_uninspectable_process_is_named_not_blamed() {
    let mut report = Report::new(Status::Exhausted, Some(Path::new("/w")), false);
    let mut skip = tier_skip(
        "a",
        "/w/x",
        Reason::InUseUnknown,
        "pid 9 (sh) cannot be inspected, so it may be using any candidate: EIO",
    );
    skip.process_blockers = Some(vec![crate::engine::ProcessBlocker {
        pid: 9,
        command: Some("sh".into()),
        evidence: crate::engine::ProcessEvidence::Unreadable,
    }]);
    report.skipped = vec![skip];
    let text = report.summary().expect("summary");
    assert!(
        text.contains(
            "kept /w/x: in_use_unknown, pid 9 (sh) cannot be inspected, so whether it is in use \
             cannot be told"
        ),
        "{text}"
    );
    assert!(!text.contains("used by"), "{text}");
}

/// TASK-221 (Debian walk dumps 14-16): in a container diskgc has its own
/// PID namespace, and every `/tmp` candidate whose contract needs the
/// process list is kept `producer_unknown`. The summary says why in words
/// (not the contract's row) and what to do, once per cause, in live and
/// dry runs; a `producer_unknown` of another cause gets no such line.
#[test]
fn producer_unknown_from_a_hidden_process_list_says_what_to_do() {
    let row8 = "liveness_contract pytest row 8 (named): the process list is not complete: \
                diskgc is not in the initial PID namespace";
    let rc = "liveness_contract nix-shell: whether owner pid 41234 ended cannot be decided: \
              diskgc is not in the initial PID namespace";
    let marker = "liveness_contract nix-shell: the pid marker is malformed";
    let mut report = Report::new(Status::Exhausted, Some(Path::new("/tmp")), false);
    report.skipped = vec![
        tier_skip(
            "t",
            "/tmp/pytest-of-u/pytest-0",
            Reason::ProducerUnknown,
            row8,
        ),
        tier_skip("t", "/tmp/nix-shell-41234-1", Reason::ProducerUnknown, rc),
        tier_skip("t", "/tmp/nix-shell-9-1", Reason::ProducerUnknown, marker),
    ];
    let want = "\n  2 candidate(s) kept producer_unknown: diskgc runs in its own PID namespace (a \
                container) and cannot see every process, so whether their producer ended cannot \
                be told; run ensure on the host (outside any container) to reclaim them";
    let text = report.summary().expect("summary");
    assert!(text.contains(want), "{text}");
    assert!(!text.contains("row 8"), "{text}");
    assert!(
        text.contains(
            "kept /tmp/pytest-of-u/pytest-0: producer_unknown, diskgc runs in its own PID \
             namespace (a container) and cannot see every process"
        ),
        "{text}"
    );
    assert!(
        text.contains("kept /tmp/nix-shell-9-1: producer_unknown, liveness_contract nix-shell: the pid marker is malformed"),
        "{text}"
    );

    let mut dry = Report::new(Status::DryRun, Some(Path::new("/tmp")), true);
    dry.plan = [row8, rc, marker]
        .iter()
        .enumerate()
        .map(|(n, detail)| PlanEntry {
            detail: Some((*detail).into()),
            ..plan_row(
                "t",
                &format!("/tmp/c{n}"),
                Some(Reason::ProducerUnknown),
                None,
            )
        })
        .collect();
    let text = dry.summary().expect("a dry run prints");
    assert!(text.contains(want), "{text}");

    let hidepid = "liveness_contract nix-shell: whether owner pid 7 ended cannot be decided: \
                   the held /proc is mounted with hidepid";
    report.skipped = vec![tier_skip("t", "/tmp/x", Reason::ProducerUnknown, hidepid)];
    let text = report.summary().expect("summary");
    assert!(
        text.contains(
            "\n  1 candidate(s) kept producer_unknown: /proc is mounted with hidepid, which hides \
             other processes, so whether their producer ended cannot be told; run ensure where \
             /proc is mounted without hidepid to reclaim them"
        ),
        "{text}"
    );
    report.skipped = vec![tier_skip("t", "/tmp/x", Reason::ProducerUnknown, marker)];
    let text = report.summary().expect("summary");
    assert!(!text.contains("to reclaim them"), "{text}");
}

/// TASK-216: the first line of a dry run ends with a colon only when lines
/// follow it.
#[test]
fn a_dry_run_with_nothing_to_list_ends_without_a_colon() {
    let mut dry = Report::new(Status::DryRun, Some(Path::new("/w")), true);
    dry.duration_ms = Some(1);
    dry.avail_before_bytes = Some(300);
    dry.min_free_bytes = Some(200);
    dry.target_free_bytes = Some(200);
    let text = dry.summary().expect("a dry run prints");
    assert!(
        text.ends_with("a reclaim would take nothing (0 eligible, 0 kept) (took 1 ms)"),
        "{text}"
    );
    assert!(!text.contains('\n'), "{text}");
    dry.plan = vec![plan_row("t", "/w/a", Some(Reason::NotIdle), None)];
    let text = dry.summary().expect("a dry run prints");
    assert!(text.contains("(took 1 ms):\n  kept /w/a"), "{text}");
}
