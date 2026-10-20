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
use crate::engine::{Checkouts, Reason};

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
    let mut r = Report::new(Status::Met, Some(&PathBuf::from("/home/mark/src/app")));
    r.mount = Some("/".into());
    r.min_free_bytes = Some(50);
    r.target_free_bytes = Some(80);
    r.avail_before_bytes = Some(10);
    r.avail_after_bytes = Some(90);
    let plan = |reason, bytes| PlanEntry {
        group: "--select".into(),
        tier: "--select".into(),
        profile: "cargo-target".into(),
        candidate: "/home/mark/src/app/target".into(),
        reason,
        estimated_bytes: bytes,
        operation: "cargo lock protocol".into(),
        checkouts: Checkouts::Allow,
        build_evidence: None,
    };
    r.plan = vec![plan(None, Some(7)), plan(Some("not_idle"), None)];
    r.actions = vec![ActionEntry {
        tier: "--select".into(),
        profile: "cargo-target".into(),
        candidate: "/t".into(),
        avail_delta_bytes: Some(-3),
        duration_ms: 4,
        count: 1,
    }];
    r.skipped = vec![SkipEntry {
        tier: "--select".into(),
        profile: "cargo-target".into(),
        candidate: "/u".into(),
        count: 1,
        examples: vec!["/u".into()],
        reason: "locked",
    }];
    r.held_by_deleted_files = vec![HeldEntry {
        pid: 42,
        command: "cat".into(),
        bytes: 5,
    }];
    r.overruns = vec![OverrunEntry {
        operation: "unlinkat".into(),
        path: "/x".into(),
        over_ms: 6,
    }];
    r.errors = vec![ErrorEntry {
        tier: "--select".into(),
        profile: "cargo-target".into(),
        candidate: "/v".into(),
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
    for reason in Reason::ALL {
        assert!(
            reasons.contains(reason.as_str()),
            "skip reason {} is not documented",
            reason.as_str()
        );
    }
    // Every status the report can carry shows up as that value.
    for status in Status::ALL {
        let mut r = full();
        r.status = status;
        assert_eq!(emitted(&r)["status"], status.as_str());
    }
}

/// On a usage error, fields that could not be determined are `null`; every
/// field is still there and `errors` says why.
#[test]
fn a_usage_error_report_keeps_every_field() {
    let r = Report::new(Status::UsageError, None).usage_error("--min-free: `50`: a bare number");
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
    assert_eq!(got["errors"][0]["candidate"], "");
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
        "The first sample, of `PATH`'s filesystem before anything is done, is the exception: \
         when it cannot be taken nothing was measured or done, and it is exit 2,",
        "A dry run keeps its exits (0, 2, 124, 130/143) and reports a final sample it could not \
         take as `avail_after_bytes: null` with an `errors` entry.",
        "or the final sample could not be taken.",
    ] {
        assert!(codes.contains(sentence), "{sentence:?} not in: {codes}");
    }
}

#[test]
fn the_summary_is_silent_only_when_nothing_needed_doing() {
    let mut quiet = Report::new(Status::Met, Some(&PathBuf::from("/p")));
    quiet.mount = Some("/".into());
    assert_eq!(quiet.summary(), None);
    let mut freed = full();
    freed.errors.clear();
    let text = freed.summary().expect("something was done");
    assert!(text.starts_with("met: 90 B available on / "), "{text}");
    assert!(text.contains("1 action(s)"), "{text}");
    let mut dry = full();
    dry.status = Status::DryRun;
    let text = dry.summary().expect("a dry run always prints");
    assert!(text.starts_with("dry run: 10 B available on / "), "{text}");
    assert!(text.contains("eligible"), "{text}");
    assert!(
        text.contains("skip not_idle  /home/mark/src/app/target"),
        "{text}"
    );
    let mut exhausted = full();
    exhausted.status = Status::Exhausted;
    assert!(
        exhausted
            .summary()
            .expect("prints")
            .starts_with("exhausted: ")
    );
}
