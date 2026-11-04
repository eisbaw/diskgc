//! The survey's pure parts; its composition is tested on ModelHost
//! (crates/diskgc-model/tests/survey.rs).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{at_or_below, matched_names, worst};
use crate::engine::Checkouts;
use crate::report::{PlanEntry, Report, Signal, Status};

fn row(candidate: Option<&str>) -> PlanEntry {
    PlanEntry {
        group: "g".into(),
        tier: "t".into(),
        profile: "p".into(),
        candidate: candidate.map(PathBuf::from),
        reason: None,
        estimated_bytes: None,
        operation: String::new(),
        checkouts: Checkouts::Refuse,
        build_evidence: None,
        idle_since: None,
        detail: None,
        process_blockers: None,
        unknown_cause: None,
    }
}

#[test]
fn a_top_level_entry_is_matched_when_it_is_or_holds_a_candidate() {
    let mut plan = Report::new(Status::DryRun, Some(Path::new("/tmp")), true);
    plan.plan = vec![
        row(Some("/tmp/nix-shell.A1/pytest-of-m/pytest-0")),
        row(Some("/tmp/tmp.x")),
        // A root row of the PATH itself, a row without a candidate, and
        // one outside PATH match no entry.
        row(Some("/tmp")),
        row(None),
        row(Some("/var/tmp/other")),
        // Component-wise: /tmpx is not below /tmp.
        row(Some("/tmpx/y")),
    ];
    let names = matched_names(&plan, Path::new("/tmp"));
    let want: Vec<OsString> = vec!["nix-shell.A1".into(), "tmp.x".into()];
    assert_eq!(names.into_iter().collect::<Vec<_>>(), want);
}

#[test]
fn the_worst_status_of_several_plans_decides() {
    use Status::*;
    let int = Interrupted(Signal::Int);
    for (statuses, want) in [
        (vec![], DryRun),
        (vec![DryRun, DryRun], DryRun),
        (vec![DryRun, Deadline], Deadline),
        (vec![Deadline, Error], Error),
        (vec![Error, int], int),
        (vec![int, UsageError, DryRun], UsageError),
    ] {
        assert_eq!(worst(statuses.clone().into_iter()), want, "{statuses:?}");
    }
}

#[test]
fn roots_are_held_to_path_component_by_component() {
    assert!(at_or_below(Path::new("/tmp"), Path::new("/tmp")));
    assert!(at_or_below(Path::new("/tmp/a"), Path::new("/tmp")));
    assert!(at_or_below(Path::new("/var/tmp"), Path::new("/")));
    assert!(!at_or_below(Path::new("/tmpx"), Path::new("/tmp")));
    assert!(!at_or_below(Path::new("/tmp"), Path::new("/tmp/a")));
}
