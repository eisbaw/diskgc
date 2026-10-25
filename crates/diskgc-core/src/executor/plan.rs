//! `--dry-run`: what [`super::ensure`] would do, deleting nothing and
//! taking no lock (docs/reference.md, `--dry-run`), per kind: Cargo
//! targets, `fs` candidates, a union of `fs` profiles, Docker.

use crate::cargo_target::{self, Stop};
use crate::command_action::{self, Preflight};
use crate::config::Order;
use crate::docker_backend::{self, Binding, Storage};
use crate::engine::{Checkouts, Identity, Reason, Rules, Skip, checkout_metadata_below};
use crate::fs_kind::{self, Find};
use crate::host::{Dev, DevIno, Host, MonoTime, Timestamp};
use crate::inuse::Checker;
use crate::profile::{Action, IdleWindow};
use crate::report::{BuildEvidence, PlanEntry, Report, Status};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    CandidateQueue, FsUnionSourceRef, Measured, NegatedKind, NegatedSource, NotesOncePerRun,
    Request, SourceKind, Tier, bounded_tier_timeout, deadline_of, deadline_passed, discover,
    excluded, excluded_by, final_sample, held, is_misconfigured, is_misconfigured_source,
    measure_failure, measure_until, misconfigured_roots, nothing_searched, oldest_first, other_fs,
    overlaps_checked, preflight_roots, record_deadline_overrun, record_error, roots_text, rules,
    run_error, safety_error, scan_protections, stop_error, timed_report, usage,
};

/// Plans what [`ensure`] would do, deleting nothing and taking no lock
/// (docs/reference.md, `--dry-run`). Given only a [`Host`]. Its caller must
/// satisfy the same root policy and `owners` precondition as [`ensure`].
pub fn dry_run<H: Host>(
    host: &H,
    request: &Request,
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    timed_report(host, || {
        let mut report = dry_run_inner(host, request, owners, log);
        if report.status == Status::DryRun && !report.errors.is_empty() {
            report.status = Status::Error;
        }
        report
    })
}

pub(super) fn dry_run_inner<H: Host>(
    host: &H,
    request: &Request,
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    let deadline = deadline_of(host, request.timeout);
    let mut report = request.report(Status::DryRun, true);
    if host.interrupted().is_some() {
        return report;
    }
    let measured = match measure_until(host, request, &mut report, deadline) {
        Ok(m) => m,
        Err(_) if host.interrupted().is_some() => return report,
        Err(e) => return measure_failure(report, log, e),
    };
    if host.interrupted().is_some() {
        return report;
    }
    let tier = &request.tier;
    if deadline_passed(host, deadline) {
        return deadline_before_plan(host, &measured, deadline, report, log, tier);
    }
    let started = host.now();
    let roots = preflight_roots(host, tier, measured.dev);
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "preflight",
        &request.path,
    );
    if deadline_passed(host, deadline) {
        return deadline_before_plan(host, &measured, deadline, report, log, tier);
    }
    report
        .plan
        .extend(roots.skipped.iter().map(|skip| PlanEntry {
            candidate: skip.candidate.clone(),
            profile: skip.profile.clone(),
            detail: skip.detail.clone(),
            ..plan_tier_row(tier, Some(skip.reason))
        }));
    if !roots.errors.is_empty() {
        for error in roots.errors.iter().cloned() {
            record_error(&mut report, log, error);
        }
        report.avail_after_bytes = final_sample(host, &measured, deadline, &mut report, log, tier);
        return report;
    }
    let tier = roots.filtered_tier(tier);
    let tier = &tier;
    let started = host.now();
    let protections = scan_protections(host, &tier.protections, deadline);
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "protect",
        &request.path,
    );
    if deadline_passed(host, deadline) {
        return deadline_before_plan(host, &measured, deadline, report, log, tier);
    }
    let protection_matches = match protections {
        Ok(matches) => matches,
        Err(error) => {
            record_error(&mut report, log, run_error(tier, error));
            report.status = Status::Error;
            return report;
        }
    };
    if matches!(
        tier.source,
        SourceKind::DockerBuildCache | SourceKind::DockerImages
    ) {
        let started = host.now();
        let inspected = docker_backend::inspect_backend(host, measured.dev, docker_storage(tier));
        record_deadline_overrun(
            host,
            &mut report,
            deadline,
            started,
            "preflight",
            &request.path,
        );
        if deadline_passed(host, deadline) {
            return deadline_before_plan(host, &measured, deadline, report, log, tier);
        }
        match inspected {
            Ok(Binding::Bound(bound)) => {
                let candidate = docker_candidate(tier, &bound);
                log(&format!(
                    "{}: Docker backend bound to {}",
                    tier.name,
                    candidate.display()
                ));
                let protected = match overlaps_checked(host, &candidate, &protection_matches) {
                    Ok(protected) => protected,
                    Err(error) => {
                        record_error(&mut report, log, run_error(tier, error));
                        report.status = Status::Error;
                        return report;
                    }
                };
                report.plan.push(plan_entry(
                    tier,
                    &candidate,
                    protected.then_some(Reason::Protected),
                    None,
                ));
            }
            Ok(Binding::Skipped(skip)) => {
                log(&format!(
                    "{}: skip Docker backend: {}: {}",
                    tier.name, skip.reason, skip.detail
                ));
                report.plan.push(plan_tier_row(tier, Some(skip.reason)));
            }
            Err(error) => record_error(&mut report, log, run_error(tier, error.to_string())),
        }
        report.avail_after_bytes = final_sample(host, &measured, deadline, &mut report, log, tier);
        return report;
    }
    if let Action::Command { argv, .. } = &tier.action {
        let started = host.now();
        let preflight = command_action::preflight(host, argv);
        record_deadline_overrun(
            host,
            &mut report,
            deadline,
            started,
            "preflight",
            &request.path,
        );
        if deadline_passed(host, deadline) {
            return deadline_before_plan(host, &measured, deadline, report, log, tier);
        }
        match preflight {
            Ok(Preflight::Ready(ready)) => log(&format!(
                "{}: command executables: {}",
                tier.name,
                ready
                    .executables
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Ok(Preflight::Missing(program)) => {
                log(&format!(
                    "{}: profile `{}` is missing executable {program:?}",
                    tier.name, tier.profile
                ));
                report.plan.push(plan_tier_row(tier, Some(Reason::Missing)));
                report.avail_after_bytes =
                    final_sample(host, &measured, deadline, &mut report, log, tier);
                return report;
            }
            Err(error) => {
                record_error(&mut report, log, run_error(tier, error.to_string()));
                report.avail_after_bytes =
                    final_sample(host, &measured, deadline, &mut report, log, tier);
                return report;
            }
        }
    }
    let rules = rules(tier, owners);
    let mut checker = Checker::for_plan(&rules)
        .with_pid_lookup(tier.pid_lookup())
        .with_pytest_facts(tier.pytest_facts());
    let started = host.now();
    let planned = match &tier.source {
        SourceKind::CargoTarget => plan_cargo(
            host,
            tier,
            measured.dev,
            &rules,
            &mut checker,
            deadline,
            &mut report,
            log,
        ),
        SourceKind::Fs(find) => {
            if tier.fs_union.is_empty() && tier.negated.is_empty() {
                plan_fs(
                    host,
                    tier,
                    find,
                    measured.dev,
                    &rules,
                    deadline,
                    &mut report,
                    log,
                )
            } else {
                plan_fs_union(host, tier, measured.dev, &rules, deadline, &mut report, log)
            }
        }
        SourceKind::DockerBuildCache | SourceKind::DockerImages => unreachable!("handled above"),
    };
    record_deadline_overrun(host, &mut report, deadline, started, "plan", &request.path);
    // A deadline here keeps what was judged (TASK-227): each planner stops
    // at it and returns the candidates it finished judging; a candidate it
    // was judging when the deadline passed is not among them.
    let Planned {
        mut eligible,
        skipped,
        stopped,
    } = planned;
    if host.interrupted().is_some() {
        // A partial judgement is not a trustworthy plan. Keep the root
        // refusals already recorded, but publish no unverified candidate.
        return report;
    }
    if tier.order == Order::Oldest {
        oldest_first(&mut eligible, |(last, _)| *last);
    } else {
        // A dry run cannot observe what reclamation actually frees. Order
        // the complete plan with estimated bytes and the initial sample.
        log(&format!(
            "{}: best-fit dry-run order uses estimates; actual reclaim order may differ",
            tier.name
        ));
        let mut remaining = measured
            .target
            .saturating_sub(report.avail_before_bytes.expect("measured"));
        let mut ordered = Vec::with_capacity(eligible.len());
        let mut queue =
            CandidateQueue::new(Order::BestFit, eligible, |(_, entry): &(_, PlanEntry)| {
                entry.estimated_bytes.map(|bytes| (bytes, 1))
            });
        while let Some(item) = queue.pop(remaining) {
            remaining = remaining.saturating_sub(item.1.estimated_bytes.unwrap_or(0));
            ordered.push(item);
        }
        eligible = ordered;
    }
    let root_skips = std::mem::take(&mut report.plan);
    report.plan = eligible
        .into_iter()
        .map(|(_, e)| e)
        .chain(skipped)
        .collect();
    // Without protect matches nothing can overlap one: no check, no I/O.
    let checks = if protection_matches.is_empty() {
        0
    } else {
        report.plan.len()
    };
    let mut unchecked_from = None;
    for index in 0..checks {
        let Some(candidate) = report.plan[index].candidate.clone() else {
            continue;
        };
        if deadline_passed(host, deadline) {
            unchecked_from = Some(index);
            break;
        }
        let started = host.now();
        let overlap = overlaps_checked(host, &candidate, &protection_matches);
        record_deadline_overrun(host, &mut report, deadline, started, "protect", &candidate);
        let protected = match overlap {
            Ok(protected) => protected,
            Err(error) => {
                record_error(&mut report, log, run_error(tier, error));
                report.status = Status::Error;
                report.plan.extend(root_skips);
                return report;
            }
        };
        if protected {
            let entry = &mut report.plan[index];
            entry.reason = Some(Reason::Protected);
            entry.estimated_bytes = None;
            entry.detail = Some("overlaps a protected source match".to_owned());
        }
    }
    if let Some(index) = unchecked_from {
        // An eligible row is judged only once it is checked against the
        // protect matches; past the deadline it is dropped, not guessed.
        // A skipped row keeps its own reason, which is true either way.
        let unchecked = report.plan.split_off(index);
        let dropped = unchecked.iter().filter(|e| e.reason.is_none()).count();
        report
            .plan
            .extend(unchecked.into_iter().filter(|e| e.reason.is_some()));
        log(&format!(
            "{}: the deadline passed before {dropped} eligible candidate(s) were checked \
             against the protect matches; they are not in the plan",
            tier.name
        ));
    }
    report.plan.extend(root_skips);
    match stopped {
        Some(stop) => {
            if matches!(stop, Stop::DeadlinePassed { .. }) {
                report.status = Status::Deadline;
            }
            record_error(
                &mut report,
                log,
                stop_error(tier, &stop, format!("stopped: {stop}")),
            );
        }
        None if unchecked_from.is_some() => {
            report.status = Status::Deadline;
            record_error(
                &mut report,
                log,
                run_error(tier, "deadline passed before the plan was complete"),
            );
        }
        None => {}
    }
    // Unmeasured: unknown, not the earlier sample.
    report.avail_after_bytes = final_sample(host, &measured, deadline, &mut report, log, tier);
    if !deadline_passed(host, deadline) && host.interrupted().is_none() {
        let started = host.now();
        report.held_by_deleted_files = held(
            host,
            &mut checker,
            measured.dev,
            &measured.at.mount,
            deadline,
            log,
        );
        record_deadline_overrun(
            host,
            &mut report,
            deadline,
            started,
            "in_use",
            &request.path,
        );
    }
    if deadline_passed(host, deadline) {
        report.status = Status::Deadline;
    }
    report
}

/// Plan every configured tier in its policy order without taking a host
/// lock. Each plan remains tagged with its configured group and tier.
pub fn dry_run_grouped<H: Host>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    timed_report(host, || {
        dry_run_grouped_inner(host, request, tiers, tier_timeouts, owners, log)
    })
}

pub(super) fn dry_run_grouped_inner<H: Host>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    let mut notes = NotesOncePerRun::new(log);
    let log: &mut dyn FnMut(&str) = &mut |line| notes.line(line);
    let Some(first) = tiers.first().filter(|_| tiers.len() == tier_timeouts.len()) else {
        return usage(
            request.report(Status::DryRun, true),
            log,
            "configuration has no tiers to plan".to_owned(),
        );
    };
    let run_deadline = deadline_of(host, request.timeout);
    let mut for_tier = request.clone();
    for_tier.tier = first.clone();
    for_tier.timeout = bounded_tier_timeout(host, run_deadline, tier_timeouts[0]);
    let mut report = dry_run(host, &for_tier, owners, log);
    if matches!(report.status, Status::UsageError | Status::Error) {
        return report;
    }
    if report.status == Status::Deadline {
        if run_deadline.is_some_and(|end| host.now() >= end) {
            record_unplanned(&mut report, log, &tiers[1..]);
            return report;
        }
        // Only this tier's budget expired; later tiers are still planned.
        // Any error (the cut, or a host error) makes the grouped plan an
        // error. No error: the tier's plan completed and its budget ran out
        // in a later step (the process scan); nothing is missing.
        report.status = if report.errors.is_empty() {
            Status::DryRun
        } else {
            Status::Error
        };
    }
    for (index, (tier, timeout)) in tiers.iter().zip(tier_timeouts).enumerate().skip(1) {
        if host.interrupted().is_some() {
            break;
        }
        if run_deadline.is_some_and(|end| host.now() >= end) {
            report.status = Status::Deadline;
            record_unplanned(&mut report, log, &tiers[index..]);
            break;
        }
        for_tier.tier = tier.clone();
        for_tier.timeout = bounded_tier_timeout(host, run_deadline, *timeout);
        let part = dry_run(host, &for_tier, owners, log);
        report.overruns.extend(part.overruns.iter().cloned());
        if matches!(part.status, Status::UsageError | Status::Error) {
            report.errors.extend(part.errors);
            report.status = part.status;
            report.avail_after_bytes = part.avail_after_bytes;
            return report;
        }
        let part_failed = !part.errors.is_empty();
        report.plan.extend(part.plan);
        report.errors.extend(part.errors);
        report.held_by_deleted_files = part.held_by_deleted_files;
        report.avail_after_bytes = part.avail_after_bytes;
        if part.status == Status::Deadline && run_deadline.is_some_and(|end| host.now() >= end) {
            report.status = Status::Deadline;
            record_unplanned(&mut report, log, &tiers[index + 1..]);
            break;
        }
        if part.status == Status::Deadline && part_failed {
            // A tier deadline alone does not stop the overall plan; its
            // error (as in the first tier's) makes the plan an error.
            report.status = Status::Error;
        }
    }
    report
}

/// The overall deadline stopped a grouped dry run before `unplanned` were
/// planned: each gets the error that says the plan was cut short (the
/// contract of a dry run at the deadline, docs/reference.md `--dry-run`),
/// so a later tier is never silently missing from the plan (TASK-232).
pub(super) fn record_unplanned(report: &mut Report, log: &mut dyn FnMut(&str), unplanned: &[Tier]) {
    debug_assert_eq!(report.status, Status::Deadline);
    for tier in unplanned {
        record_error(
            report,
            log,
            run_error(tier, "deadline passed before this tier was planned"),
        );
    }
}

/// A plan in the making: the eligible candidates with their order key
/// (the kind's idle measure), the skipped ones, and why planning stopped
/// early (an error of the run, or the deadline).
pub(super) struct Planned {
    pub(super) eligible: Vec<(Option<Timestamp>, PlanEntry)>,
    pub(super) skipped: Vec<PlanEntry>,
    pub(super) stopped: Option<Stop>,
}

/// One plan entry of `tier`.
pub(super) fn plan_entry(
    tier: &Tier,
    candidate: &Path,
    reason: Option<Reason>,
    bytes: Option<u64>,
) -> PlanEntry {
    assert!(
        !candidate.as_os_str().is_empty(),
        "a plan row of the tier as a whole is plan_tier_row's"
    );
    PlanEntry {
        candidate: Some(candidate.to_owned()),
        estimated_bytes: bytes,
        ..plan_tier_row(tier, reason)
    }
}

/// The plan row of the tier as a whole, refused (`reason`) before it had a
/// candidate: a missing executable, an unavailable backend.
pub(super) fn plan_tier_row(tier: &Tier, reason: Option<Reason>) -> PlanEntry {
    PlanEntry {
        group: tier.group.clone(),
        tier: tier.name.clone(),
        profile: tier.profile.clone(),
        candidate: None,
        reason,
        estimated_bytes: None,
        operation: tier.operation(),
        checkouts: tier.checkouts,
        build_evidence: None,
        detail: None,
        process_blockers: None,
    }
}

/// The plan row of a candidate `skip` refused. A dry run's verdicts are its
/// plan (stdout), so they are not logged as well.
pub(super) fn plan_skip(tier: &Tier, candidate: &Path, skip: &Skip) -> PlanEntry {
    PlanEntry {
        detail: Some(skip.detail.clone()),
        process_blockers: skip.process_blockers.clone(),
        ..plan_entry(tier, candidate, Some(skip.reason), None)
    }
}

pub(super) fn docker_candidate(tier: &Tier, bound: &docker_backend::Bound) -> PathBuf {
    match &tier.source {
        SourceKind::DockerBuildCache => bound.info.docker_root.join("buildkit"),
        SourceKind::DockerImages => bound.info.image_store.root().to_owned(),
        _ => unreachable!("Docker candidate for a non-Docker tier"),
    }
}

pub(super) fn docker_storage(tier: &Tier) -> Storage {
    match tier.source {
        SourceKind::DockerBuildCache => Storage::BuildCache,
        SourceKind::DockerImages => Storage::Images,
        _ => unreachable!("Docker storage for a non-Docker tier"),
    }
}

/// The plan of a `cargo-target` tier: discovery, then the eligibility
/// check of every target.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_cargo<H: Host>(
    host: &H,
    tier: &Tier,
    dev: Dev,
    rules: &Rules<'_>,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
    report: &mut Report,
    log: &mut dyn FnMut(&str),
) -> Planned {
    let entry = |candidate: &Path, reason: Option<Reason>, bytes: Option<u64>| {
        plan_entry(tier, candidate, reason, bytes)
    };
    let mut eligible = Vec::new();
    let mut skipped = Vec::new();
    let mut stopped = None;
    match discover(host, tier, deadline, log) {
        Err(stop) => stopped = Some(stop),
        Ok(found) => {
            // As ensure: a misconfigured root is an error (not a plan
            // entry) and excludes the tier.
            let misconfigured = misconfigured_roots(tier, &found);
            for skip in &found.refused {
                if !misconfigured.contains(&skip) {
                    skipped.push(plan_skip(tier, &skip.path, skip));
                }
            }
            for skip in &misconfigured {
                record_error(report, log, safety_error(tier, excluded(skip)));
            }
            let negated_matches = match scan_negated(host, &tier.negated, deadline) {
                Ok(matches) => matches,
                Err(error) => {
                    record_error(report, log, run_error(tier, error));
                    return Planned {
                        eligible,
                        skipped,
                        stopped,
                    };
                }
            };
            let targets = if misconfigured.is_empty() {
                found.targets.as_slice()
            } else {
                &[]
            };
            for target in targets {
                if host.interrupted().is_some() {
                    break;
                }
                let excluded =
                    match excluded_by(host, &tier.negated, &negated_matches, &target.path) {
                        Ok(excluded) => excluded,
                        Err(error) => {
                            record_error(report, log, run_error(tier, error));
                            return Planned {
                                eligible,
                                skipped,
                                stopped,
                            };
                        }
                    };
                if let Some(profile) = excluded {
                    let skip = Skip::new(
                        Reason::Excluded,
                        target.path.clone(),
                        format!("overlaps !profile:{profile}"),
                    );
                    skipped.push(plan_skip(tier, &target.path, &skip));
                    continue;
                }
                if let Some(skip) = other_fs(target, dev) {
                    skipped.push(plan_skip(tier, &target.path, &skip));
                    continue;
                }
                match cargo_target::eligibility(
                    host,
                    target,
                    rules,
                    IdleWindow {
                        at_least: tier.idle_for,
                        before: tier.idle_before,
                    },
                    checker,
                    deadline,
                ) {
                    Ok(Ok(e)) => {
                        eligible.push((e.last_build, entry(&target.path, None, Some(e.bytes))));
                    }
                    Ok(Err(skip)) => {
                        skipped.push(plan_skip(tier, &target.path, &skip));
                    }
                    Err(stop) => {
                        stopped = Some(stop);
                        break;
                    }
                }
            }
        }
    }
    Planned {
        eligible,
        skipped,
        stopped,
    }
}

/// The plan of an `fs` tier ([`fs_kind::plan`]): every candidate found
/// below the roots, judged. As for Cargo, a root reached through a symlink
/// or a mount crossing (`unsafe_path`) is a misconfiguration: an error,
/// and the tier is excluded.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_fs<H: Host>(
    host: &H,
    tier: &Tier,
    find: &Find,
    dev: Dev,
    rules: &Rules<'_>,
    deadline: Option<MonoTime>,
    report: &mut Report,
    log: &mut dyn FnMut(&str),
) -> Planned {
    let mut planned = Planned {
        eligible: Vec::new(),
        skipped: Vec::new(),
        stopped: None,
    };
    let (found, stopped) = fs_kind::plan(
        host,
        &tier.roots,
        find,
        dev,
        rules,
        IdleWindow {
            at_least: tier.idle_for,
            before: tier.idle_before,
        },
        deadline,
        log,
    );
    // When the run stopped, the count is what was judged until then; the
    // candidates found but not judged are counted apart.
    let partial = if stopped.is_some() {
        format!(", stopped early: {} found but not judged", found.unjudged)
    } else {
        String::new()
    };
    planned.stopped = stopped;
    if tier.roots.is_empty() {
        assert!(
            found.judged.is_empty(),
            "a tier without roots finds nothing"
        );
        log(&nothing_searched(tier));
    } else {
        log(&format!(
            "{}: `{}`: {} candidate(s) judged below {} ({} found again through another root \
             or pattern, {} systemd-private-* passed over{partial})",
            tier.name,
            tier.select,
            found.judged.len(),
            roots_text(&tier.roots),
            found.duplicates.len(),
            found.private_tmp,
        ));
    }
    let misconfigured: Vec<&Skip> = found
        .refused
        .iter()
        .filter(|skip| is_misconfigured(tier, skip))
        .collect();
    for skip in &misconfigured {
        record_error(report, log, safety_error(tier, excluded(skip)));
    }
    for skip in found.refused.iter().filter(|s| !is_misconfigured(tier, s)) {
        planned.skipped.push(plan_skip(tier, &skip.path, skip));
    }
    if !misconfigured.is_empty() {
        return planned;
    }
    for (path, source_evidence, verdict) in &found.judged {
        match verdict {
            Ok(e) => {
                let mut entry = plan_entry(tier, path, None, Some(e.bytes));
                entry.build_evidence =
                    eligible_build_evidence(host, e.source_evidence.as_ref(), e.newest_change);
                planned.eligible.push((e.newest_change, entry));
            }
            Err(skip) => {
                let mut entry = plan_skip(tier, path, skip);
                entry.build_evidence = skipped_build_evidence(source_evidence.as_ref());
                planned.skipped.push(entry);
            }
        }
    }
    planned
}

/// An eligible plan row's `build_evidence` (docs/reference.md, Output): the
/// source its judgement verified, with the idle age of the newest change
/// the eligibility walk measured; none without both.
pub(super) fn eligible_build_evidence<H: Host>(
    host: &H,
    source: Option<&fs_kind::SourceEvidence>,
    newest: Option<Timestamp>,
) -> Option<BuildEvidence> {
    let (source, newest) = (source?, newest?);
    let now = host.wall_clock();
    let age_ns = (i128::from(now.secs) - i128::from(newest.secs)) * 1_000_000_000
        + i128::from(now.nanos)
        - i128::from(newest.nanos);
    Some(BuildEvidence {
        marker: source.marker,
        source: source.source.clone(),
        idle_age_seconds: Some(u64::try_from(age_ns.max(0) / 1_000_000_000).unwrap_or(u64::MAX)),
    })
}

/// A skipped plan row's `build_evidence`: the source established at
/// discovery, with no age (the skip stopped the walk before one was
/// established).
pub(super) fn skipped_build_evidence(
    source: Option<&fs_kind::SourceEvidence>,
) -> Option<BuildEvidence> {
    source.map(|source| BuildEvidence {
        marker: source.marker,
        source: source.source.clone(),
        idle_age_seconds: None,
    })
}

/// One inode admitted by at least one filesystem source. `found_by` supplies
/// action choices; `guard_by` also retains structural matches whose source
/// evidence failed, so each positive guard has its own root and path for
/// lock-time rechecks. Discovery order chooses the report path.
pub(super) struct FsUnionCandidate {
    pub(super) identity: Identity,
    pub(super) found_by: Vec<(usize, fs_kind::Found)>,
    pub(super) guard_by: Vec<(usize, fs_kind::Found)>,
    pub(super) invalid: bool,
}

impl FsUnionCandidate {
    /// The build source `source` (an index into the tier's sources: the
    /// row's profile) established when it discovered this candidate, for a
    /// skipped plan row's `build_evidence` (docs/reference.md, Output:
    /// discovery's, which may not have survived later rechecks). Only the
    /// row's own term: a row never reports another profile's anchor.
    pub(super) fn discovered(&self, source: usize) -> Option<&fs_kind::SourceEvidence> {
        fn evidence((_, found): &(usize, fs_kind::Found)) -> Option<&fs_kind::SourceEvidence> {
            found.source_evidence.as_ref()
        }
        self.found_by
            .iter()
            .chain(&self.guard_by)
            .filter(|(index, _)| *index == source)
            .find_map(evidence)
    }

    pub(super) fn contributors(&self) -> Vec<(usize, &fs_kind::Found, bool)> {
        let mut contributors: Vec<_> = self
            .found_by
            .iter()
            .map(|(index, found)| (*index, found, true))
            .collect();
        for (index, found) in &self.guard_by {
            if !self.found_by.iter().any(|(admitted_index, admitted)| {
                admitted_index == index
                    && admitted.root == found.root
                    && admitted.path == found.path
            }) {
                contributors.push((*index, found, false));
            }
        }
        contributors
    }
}

pub(super) fn merge_fs_union_found(
    candidates: &mut Vec<FsUnionCandidate>,
    by_inode: &mut HashMap<DevIno, usize>,
    refused: &mut Vec<(usize, Skip)>,
    source_index: usize,
    found: fs_kind::Found,
    admitted: bool,
) {
    if let Some(&index) = by_inode.get(&found.identity.id) {
        let candidate = &mut candidates[index];
        if candidate.identity != found.identity {
            refused.push((
                source_index,
                Skip::new(
                    Reason::Changed,
                    found.path,
                    "the inode changed identity between source discoveries",
                ),
            ));
            candidate.invalid = true;
            candidate.found_by.clear();
            candidate.guard_by.clear();
        } else if !candidate.invalid {
            if admitted {
                candidate.found_by.push((source_index, found));
            } else {
                candidate.guard_by.push((source_index, found));
            }
        }
    } else {
        by_inode.insert(found.identity.id, candidates.len());
        let mut candidate = FsUnionCandidate {
            identity: found.identity,
            found_by: Vec::new(),
            guard_by: Vec::new(),
            invalid: false,
        };
        if admitted {
            candidate.found_by.push((source_index, found));
        } else {
            candidate.guard_by.push((source_index, found));
        }
        candidates.push(candidate);
    }
}

pub(super) fn discover_fs_union<H: Host>(
    host: &H,
    sources: &[FsUnionSourceRef<'_>],
    deadline: Option<MonoTime>,
) -> (Vec<FsUnionCandidate>, Vec<(usize, Skip)>, Option<Stop>) {
    let mut candidates: Vec<FsUnionCandidate> = Vec::new();
    let mut by_inode: HashMap<DevIno, usize> = HashMap::new();
    let mut refused = Vec::new();
    for (source_index, source) in sources.iter().enumerate() {
        let (discovery, stopped) =
            fs_kind::discover_union(host, source.roots, source.find, deadline);
        for found in discovery.guard_candidates {
            merge_fs_union_found(
                &mut candidates,
                &mut by_inode,
                &mut refused,
                source_index,
                found,
                false,
            );
        }
        refused.extend(
            discovery
                .refused
                .into_iter()
                .map(|skip| (source_index, skip)),
        );
        for (_, found) in discovery.candidates {
            let found = match found {
                Ok(found) => found,
                Err(skip) => {
                    refused.push((source_index, skip));
                    continue;
                }
            };
            merge_fs_union_found(
                &mut candidates,
                &mut by_inode,
                &mut refused,
                source_index,
                found,
                true,
            );
        }
        if let Some(stop) = stopped {
            return (candidates, refused, Some(stop));
        }
    }
    candidates.retain(|candidate| !candidate.invalid && !candidate.found_by.is_empty());
    (candidates, refused, None)
}

/// Enumerate a negated source's raw matches. Its own `where` was removed
/// during binding; any incomplete scan is an error, never an empty set.
pub(super) fn scan_negated<H: Host>(
    host: &H,
    sources: &[NegatedSource],
    deadline: Option<MonoTime>,
) -> Result<Vec<Vec<PathBuf>>, String> {
    let mut all = Vec::with_capacity(sources.len());
    for source in sources {
        let mut paths = Vec::new();
        match &source.source {
            NegatedKind::Fs(find) => {
                let (discovery, stopped) = fs_kind::discover(host, &source.roots, find, deadline);
                if let Some(stop) = stopped {
                    return Err(format!(
                        "!profile:{} discovery stopped: {stop}",
                        source.profile
                    ));
                }
                if discovery.private_tmp != 0 {
                    return Err(format!(
                        "!profile:{} discovery skipped {} matching PrivateTmp path(s)",
                        source.profile, discovery.private_tmp
                    ));
                }
                for skip in discovery.refused {
                    if skip.reason != Reason::Missing || !source.roots.contains(&skip.path) {
                        return Err(format!(
                            "!profile:{} discovery uncertain: {}",
                            source.profile, skip.detail
                        ));
                    }
                }
                for (path, found) in discovery.candidates {
                    match found {
                        Ok(_) => paths.push(path),
                        Err(skip) => {
                            return Err(format!(
                                "!profile:{} discovery uncertain: {}",
                                source.profile, skip.detail
                            ));
                        }
                    }
                }
            }
            NegatedKind::CargoTarget => {
                let discovery =
                    cargo_target::discover(host, &source.roots, deadline).map_err(|stop| {
                        format!("!profile:{} discovery stopped: {stop}", source.profile)
                    })?;
                for skip in discovery.refused {
                    if skip.reason != Reason::Missing || !source.roots.contains(&skip.path) {
                        return Err(format!(
                            "!profile:{} discovery uncertain: {}",
                            source.profile, skip.detail
                        ));
                    }
                }
                paths.extend(discovery.targets.into_iter().map(|target| target.path));
            }
        }
        all.push(paths);
    }
    Ok(all)
}

pub(super) fn scan_union_negated<H: Host>(
    host: &H,
    sources: &[FsUnionSourceRef<'_>],
    deadline: Option<MonoTime>,
) -> Result<Vec<Vec<Vec<PathBuf>>>, String> {
    sources
        .iter()
        .map(|source| scan_negated(host, source.negated, deadline))
        .collect()
}

pub(super) fn union_checkouts(sources: &[FsUnionSourceRef<'_>], admitted: &[usize]) -> Checkouts {
    if admitted
        .iter()
        .any(|index| sources[*index].checkouts == Checkouts::Refuse)
    {
        Checkouts::Refuse
    } else {
        Checkouts::Allow
    }
}

pub(super) fn union_checkout_path<'a>(
    sources: &[FsUnionSourceRef<'_>],
    candidate: &'a FsUnionCandidate,
    admitted: &[usize],
) -> Option<(usize, &'a Path, Skip)> {
    candidate
        .found_by
        .iter()
        .filter(|(index, _)| {
            admitted.contains(index) && sources[*index].checkouts == Checkouts::Refuse
        })
        .find_map(|(index, found)| {
            checkout_metadata_below(Checkouts::Refuse, &found.root, &found.path)
                .map(|skip| (*index, found.path.as_path(), skip))
        })
}

/// The checkout refusal of a union candidate whose admitted sources do not
/// all allow checkouts ([`union_checkouts`] is `refuse`): a refusing
/// source's checkout metadata on the path, else a checkout its walk found
/// (`checkout_marker`). When an admitted source allows checkouts, the
/// union is what refused it, and the detail says so in the profiles'
/// terms. That replaces a warning logged on every union run with an
/// `allow` source, though most unions (the two 0.1 `/tmp` built-ins) can
/// never find one candidate twice (TASK-222).
pub(super) fn union_checkout_refusal(
    sources: &[FsUnionSourceRef<'_>],
    candidate: &FsUnionCandidate,
    admitted: &[usize],
    checkout_marker: Option<(usize, &Path)>,
) -> Option<(usize, Skip)> {
    let (refusing, mut skip) = union_checkout_path(sources, candidate, admitted)
        .map(|(index, _, skip)| (index, skip))
        .or_else(|| {
            checkout_marker.map(|(index, marker_path)| {
                (
                    index,
                    Skip::new(
                        Reason::ContainsCheckout,
                        marker_path.to_path_buf(),
                        "an admitted source refuses a version-control checkout in this candidate",
                    ),
                )
            })
        })?;
    if let Some(allowing) = admitted
        .iter()
        .map(|index| &sources[*index])
        .find(|source| source.checkouts == Checkouts::Allow)
    {
        skip.detail = format!(
            "{}; `{}` allows checkouts, but `{}` also selected this candidate and refuses them",
            skip.detail, allowing.profile, sources[refusing].profile
        );
    }
    Some((refusing, skip))
}

/// Retention belongs to every source that discovered this inode, regardless
/// of which source's evidence eventually admits it. Checkout policy remains
/// a separate decision over admitted terms only.
pub(super) fn union_guard_veto<H: Host>(
    host: &H,
    candidate: &FsUnionCandidate,
    sources: &[FsUnionSourceRef<'_>],
    rules: &Rules<'_>,
    dev: Dev,
    deadline: Option<MonoTime>,
) -> Result<Option<(usize, Skip)>, Stop> {
    let evaluation_rules = Rules {
        checkouts: Checkouts::Allow,
        ..*rules
    };
    let alternatives: Vec<_> = candidate
        .guard_by
        .iter()
        .map(|(index, found)| {
            let source = &sources[*index];
            fs_kind::Alternative {
                judge: fs_kind::Judge {
                    find: source.find,
                    rules: &evaluation_rules,
                    idle: source.idle,
                    dev,
                },
                found,
                checkouts: source.checkouts,
                admitted: false,
            }
        })
        .collect();
    fs_kind::retention_guard_veto(host, &alternatives, deadline)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_fs_union<H: Host>(
    host: &H,
    tier: &Tier,
    dev: Dev,
    rules: &Rules<'_>,
    deadline: Option<MonoTime>,
    report: &mut Report,
    log: &mut dyn FnMut(&str),
) -> Planned {
    let sources = tier.fs_sources();
    let (candidates, refused, stopped) = discover_fs_union(host, &sources, deadline);
    let mut planned = Planned {
        eligible: Vec::new(),
        skipped: Vec::new(),
        stopped,
    };
    if sources.iter().all(|source| source.roots.is_empty()) {
        log(&nothing_searched(tier));
    } else {
        log(&format!(
            "{}: `{}`: {} distinct filesystem candidate(s) from {} source term(s)",
            tier.name,
            tier.select,
            candidates.len(),
            sources.len()
        ));
    }
    if planned.stopped.is_some() {
        return planned;
    }
    let misconfigured: Vec<_> = refused
        .iter()
        .filter(|(index, skip)| is_misconfigured_source(&sources[*index], skip))
        .collect();
    for (index, skip) in &misconfigured {
        let mut entry = safety_error(tier, excluded(skip));
        entry.profile = Some(sources[*index].profile.to_owned());
        record_error(report, log, entry);
    }
    for (index, skip) in refused
        .iter()
        .filter(|(index, skip)| !is_misconfigured_source(&sources[*index], skip))
    {
        let mut entry = plan_skip(tier, &skip.path, skip);
        entry.profile = sources[*index].profile.to_owned();
        entry.checkouts = sources[*index].checkouts;
        planned.skipped.push(entry);
    }
    if !misconfigured.is_empty() {
        return planned;
    }
    let negated_matches = match scan_union_negated(host, &sources, deadline) {
        Ok(matches) => matches,
        Err(error) => {
            record_error(report, log, run_error(tier, error));
            return planned;
        }
    };
    let mut checker = Checker::for_plan(rules)
        .with_pid_lookup(tier.pid_lookup())
        .with_pytest_facts(tier.pytest_facts());
    for candidate in &candidates {
        if host.interrupted().is_some() {
            return planned;
        }
        match union_guard_veto(host, candidate, &sources, rules, dev, deadline) {
            Ok(Some((index, skip))) => {
                let (source_index, found) = &candidate.guard_by[index];
                let mut entry = plan_skip(tier, &skip.path, &skip);
                entry.profile = sources[*source_index].profile.to_owned();
                entry.checkouts = sources[*source_index].checkouts;
                entry.build_evidence = skipped_build_evidence(found.source_evidence.as_ref());
                planned.skipped.push(entry);
                continue;
            }
            Ok(None) => {}
            Err(stop) => {
                planned.stopped = Some(stop);
                return planned;
            }
        }
        let evaluation_rules = Rules {
            checkouts: Checkouts::Allow,
            ..*rules
        };
        let mut first_skip = None;
        let mut selected = None;
        let mut selected_record = None;
        let mut admitted = Vec::new();
        let mut checkout_marker = None;
        for (index, found) in &candidate.found_by {
            let source = &sources[*index];
            let excluded =
                match excluded_by(host, source.negated, &negated_matches[*index], &found.path) {
                    Ok(excluded) => excluded,
                    Err(error) => {
                        record_error(report, log, run_error(tier, error));
                        return planned;
                    }
                };
            if let Some(profile) = excluded {
                first_skip.get_or_insert((
                    *index,
                    found.path.as_path(),
                    Skip::new(
                        Reason::Excluded,
                        found.path.clone(),
                        format!("overlaps !profile:{profile}"),
                    ),
                ));
                continue;
            }
            let judge = fs_kind::Judge {
                find: source.find,
                rules: &evaluation_rules,
                idle: source.idle,
                dev,
            };
            match fs_kind::judge_walked(host, &judge, found, &mut checker, deadline) {
                Ok(Ok((tally, record))) => {
                    admitted.push(*index);
                    if source.checkouts == Checkouts::Refuse && tally.contains_checkout {
                        checkout_marker.get_or_insert((*index, found.path.as_path()));
                    }
                    if selected.is_none() {
                        selected = Some((*index, found.path.as_path(), tally));
                        selected_record = Some(record);
                    }
                }
                Ok(Err(skip)) => {
                    first_skip.get_or_insert((*index, found.path.as_path(), skip));
                }
                Err(stop) => {
                    planned.stopped = Some(stop);
                    return planned;
                }
            }
        }
        if let Some((index, path, _)) = &selected {
            // Liveness contracts veto across the union, as in the reclaim.
            // The sources that found it by their own evidence (`found_by`;
            // guard-only matches are `guard_by`), whatever each term's own
            // judgement said: the reclaim's admitted alternatives.
            let claimed: Vec<(usize, &fs_kind::Found)> = candidate
                .found_by
                .iter()
                .map(|(source, found)| (*source, found))
                .collect();
            let contracts: Vec<(&Find, &fs_kind::Found)> = claimed
                .iter()
                .map(|(source, found)| (sources[*source].find, *found))
                .collect();
            let record = selected_record
                .as_ref()
                .expect("the selected term walked the candidate");
            match fs_kind::contract_veto(host, &contracts, &mut checker, deadline, record) {
                Ok(Ok(())) => {}
                Ok(Err((veto, skip))) => {
                    let vetoing = claimed.get(veto).map_or(*index, |(source, _)| *source);
                    let mut entry = plan_skip(tier, path, &skip);
                    entry.profile = sources[vetoing].profile.to_owned();
                    entry.checkouts = sources[vetoing].checkouts;
                    entry.build_evidence = skipped_build_evidence(candidate.discovered(vetoing));
                    planned.skipped.push(entry);
                    continue;
                }
                Err(stop) => {
                    planned.stopped = Some(stop);
                    return planned;
                }
            }
        }
        if let Some((index, path, tally)) = selected {
            let checkouts = union_checkouts(&sources, &admitted);
            if checkouts == Checkouts::Refuse
                && let Some((refusing_index, skip)) =
                    union_checkout_refusal(&sources, candidate, &admitted, checkout_marker)
            {
                let mut entry = plan_skip(tier, path, &skip);
                entry.profile = sources[refusing_index].profile.to_owned();
                entry.checkouts = checkouts;
                entry.build_evidence = skipped_build_evidence(candidate.discovered(refusing_index));
                planned.skipped.push(entry);
                continue;
            }
            let source = &sources[index];
            let mut entry = plan_entry(tier, path, None, Some(tally.bytes));
            entry.profile = source.profile.to_owned();
            entry.checkouts = checkouts;
            // Only the source the selected term's judgement verified: an
            // eligible row's evidence survived its rechecks (a skipped
            // row's records discovery, docs/reference.md, Output).
            entry.build_evidence =
                eligible_build_evidence(host, tally.source_evidence.as_ref(), tally.newest_change);
            planned.eligible.push((tally.newest_change, entry));
        } else {
            let (index, path, skip) =
                first_skip.expect("a discovered union candidate has a verdict");
            let mut entry = plan_skip(tier, path, &skip);
            entry.profile = sources[index].profile.to_owned();
            entry.checkouts = sources[index].checkouts;
            entry.build_evidence = skipped_build_evidence(candidate.discovered(index));
            planned.skipped.push(entry);
        }
    }
    planned
}

pub(super) fn deadline_before_plan<H: Host>(
    host: &H,
    measured: &Measured<H::Fd>,
    deadline: Option<MonoTime>,
    mut report: Report,
    log: &mut dyn FnMut(&str),
    tier: &Tier,
) -> Report {
    // Before planning, the plan holds only roots preflight refused: true
    // rows, kept (TASK-227: a deadline throws no judged work away).
    report.avail_after_bytes = final_sample(host, measured, deadline, &mut report, log, tier);
    record_error(
        &mut report,
        log,
        run_error(tier, "deadline passed before the plan was complete"),
    );
    report.status = Status::Deadline;
    report
}
