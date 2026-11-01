//! The human-mode result of `ensure` (docs/reference.md, Output): a pure
//! rendering of [`outcome`]'s per-candidate verdicts. One line per
//! candidate, `VERB PATH: WHY`, so a reader (or `grep`) finds a path once
//! with its final outcome. Bounded: groups of kept candidates and long
//! lists of reclaimed ones are summarised with examples.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::outcome::{self, Live, Planned, Verdict};
use super::{
    HeldEntry, OTHER_FS_ROOT_DETAIL, PlanEntry, Report, SkipEntry, Status, human_space_values,
    signed_bytes,
};
use crate::engine::{ProcessBlocker, Reason, UnknownCause};
use crate::liveness::{HIDEPID, NOT_INITIAL_PID_NAMESPACE};
use crate::units::{format_age, format_bytes};

/// Kept candidates of one reason listed one per line up to this many;
/// above it, one line with this many examples.
const KEPT_EXAMPLES_MAX: usize = 5;

/// Reasons whose kept candidates are one line from two on. On a shared
/// host they are mostly other users' entries (another user's 0700 session
/// dir is `unreadable`, a readable one `not_owner`): nothing the reader can
/// act on, and one line each buries the lines they can (TASK-140). The
/// JSON still has each one.
fn grouped_from(reason: Reason) -> u64 {
    match reason {
        Reason::NotOwner | Reason::Unreadable => 2,
        _ => KEPT_EXAMPLES_MAX as u64 + 1,
    }
}
/// Reclaimed candidates listed one per line, the rest counted; a dry run
/// lists as many of each eligible verdict (`--json` has every row).
const RECLAIMED_LINES_MAX: usize = 10;
/// A skip's diagnostic is cut here (characters): an `in_use` detail lists
/// every piece of evidence and can run to kilobytes.
const DETAIL_CHARS_MAX: usize = 160;
/// Processes named for an `in_use` candidate; the rest are counted.
const BLOCKERS_MAX: usize = 3;
/// Holders of deleted files named in the summary.
const HELD_TOP_MAX: usize = 3;

/// [`Report::summary`].
pub(super) fn summary(report: &Report) -> Option<String> {
    let held = held_line(report.held_by_deleted_files.as_deref().unwrap_or_default());
    match report.status {
        Status::UsageError => None,
        _ if report.dry_run => Some(dry_run_text(report, held)),
        Status::Met
            if report.actions.is_empty()
                && report.errors.is_empty()
                && held.is_none()
                && report
                    .skipped
                    .iter()
                    .all(|skip| skip.reason == Reason::Empty) =>
        {
            None
        }
        _ => Some(live_text(report, held)),
    }
}

fn mount_text(report: &Report) -> String {
    report.mount.as_deref().map_or_else(
        || "unknown".to_owned(),
        |m| one_line(&m.display().to_string()),
    )
}

fn elapsed_text(report: &Report) -> String {
    report.duration_ms.map_or_else(
        || "unknown".to_owned(),
        |ms| {
            if ms < 1_000 {
                format!("{ms} ms")
            } else {
                format!("{}.{:03} s", ms / 1_000, ms % 1_000)
            }
        },
    )
}

fn live_text(report: &Report, held: Option<String>) -> String {
    let live = outcome::live(&report.actions, &report.skipped);
    let [available, min_free, target_free] = human_space_values([
        report.avail_after_bytes,
        report.min_free_bytes,
        report.target_free_bytes,
    ]);
    let status = match report.status {
        Status::Exhausted => "nothing more could be reclaimed",
        other => other.as_str(),
    };
    // Unknown if any action's change is unknown.
    let freed: Option<i64> = report.actions.iter().map(|a| a.avail_delta_bytes).sum();
    let preflight: u64 = live
        .preflight
        .iter()
        .map(|skip| skip.count)
        .fold(0, u64::saturating_add);
    let preflight_text = if preflight == 0 {
        String::new()
    } else {
        format!(", {preflight} profile(s) skipped without a candidate")
    };
    let kept: u64 = live
        .kept
        .iter()
        .map(|skip| skip.count)
        .fold(0, u64::saturating_add);
    let mut lines = vec![format!(
        "{status}: {available} available on {} (min-free {min_free}, target-free {target_free}); \
         {} reclaimed (observed change {}), {kept} kept{preflight_text}, {} error(s); took {}{}",
        mount_text(report),
        live.reclaimed.len(),
        signed_bytes(freed),
        report.errors.len(),
        elapsed_text(report),
        report.errors.first().map_or_else(String::new, |e| format!(
            "; first error: {}",
            one_line(&e.message)
        ))
    )];
    if report.status == Status::Deadline {
        lines.push(
            "  the deadline passed: the counts cover only the candidates reached before it; \
             run again, with a larger --timeout, to reclaim more"
                .to_owned(),
        );
    }
    let tiers: BTreeSet<&str> = report
        .actions
        .iter()
        .map(|a| a.tier.as_str())
        .chain(report.skipped.iter().map(|s| s.tier.as_str()))
        .collect();
    let tier_label = |tier: &str| {
        if tiers.len() > 1 {
            format!(" [{}]", one_line(tier))
        } else {
            String::new()
        }
    };
    lines.extend(reclaimed_lines(&live, &tier_label));
    let root_tiers = foreign_root_tiers(report.skipped.iter().map(|skip| {
        (
            skip.tier.as_str(),
            skip.candidate.as_deref(),
            Some(skip.reason),
            skip.detail.as_deref(),
        )
    }));
    let kept_rows = with_root_tiers(
        live.kept.iter().map(|s| KeptRow::of_skip(s)).collect(),
        &root_tiers,
    );
    lines.extend(kept_lines(&kept_rows, &tier_label));
    lines.extend(busy_line(
        live.kept
            .iter()
            .filter(|skip| skip.reason == Reason::InUseUnknown && busy(skip.unknown_cause))
            .map(|skip| skip.count)
            .fold(0, u64::saturating_add),
    ));
    lines.extend(hidden_processes_line(
        live.kept
            .iter()
            .map(|skip| (skip.reason, skip.detail.as_deref(), skip.count)),
    ));
    lines.extend(live.preflight.iter().map(|skip| {
        format!(
            "  skipped profile {} [{}]: {}",
            one_line(&skip.profile),
            one_line(&skip.tier),
            skip.reason
        )
    }));
    lines.extend(held);
    lines.join("\n")
}

fn reclaimed_lines(live: &Live<'_>, tier_label: &dyn Fn(&str) -> String) -> Vec<String> {
    let mut lines: Vec<String> = live
        .reclaimed
        .iter()
        .take(RECLAIMED_LINES_MAX)
        .map(|r| {
            let path = one_line(&r.candidate.display().to_string());
            let change = signed_bytes(r.avail_delta_bytes);
            match r.part_way {
                None => format!("  reclaimed {path}: {change}{}", tier_label(r.tier)),
                Some(skip) => format!(
                    "  reclaimed part-way {path}: {change}, then kept: {}{}",
                    skip_why(skip),
                    tier_label(&skip.tier)
                ),
            }
        })
        .collect();
    let rest = &live.reclaimed[live.reclaimed.len().min(RECLAIMED_LINES_MAX)..];
    if !rest.is_empty() {
        let change: Option<i64> = rest.iter().map(|r| r.avail_delta_bytes).sum();
        lines.push(format!(
            "  {} more reclaimed (observed change {})",
            rest.len(),
            signed_bytes(change)
        ));
    }
    lines
}

/// A kept candidate as the summary shows it: a live run's final refusal or
/// a dry run's kept plan row.
struct KeptRow<'a> {
    candidate: &'a Path,
    /// The tier that decided it; for a root on another filesystem, every
    /// tier that skipped it ([`foreign_root_tiers`]).
    tier: String,
    reason: Reason,
    count: u64,
    diagnostic: Option<String>,
    /// Text after the diagnostic (a dry run's build evidence), or empty.
    evidence: String,
}

impl<'a> KeptRow<'a> {
    fn of_skip(skip: &'a SkipEntry) -> Self {
        Self {
            candidate: skip
                .candidate
                .as_deref()
                .expect("a kept candidate has a path (preflight rows are apart)"),
            tier: skip.tier.clone(),
            reason: skip.reason,
            count: skip.count,
            diagnostic: skip_diagnostic(skip),
            evidence: String::new(),
        }
    }

    fn of_plan(entry: &'a PlanEntry) -> Self {
        let reason = entry.reason.expect("the fold keeps only refused rows");
        Self {
            candidate: entry
                .candidate
                .as_deref()
                .expect("a planned candidate has a path (preflight rows are apart)"),
            tier: entry.tier.clone(),
            reason,
            count: 1,
            diagnostic: plain_diagnostic(
                reason,
                entry.unknown_cause,
                entry.detail.as_deref(),
                entry.process_blockers.as_deref(),
            )
            .or_else(|| diagnostic(entry.detail.as_deref(), entry.process_blockers.as_deref())),
            evidence: evidence_text(entry),
        }
    }
}

/// The tiers that skipped each root on another filesystem than PATH's, in
/// run order. The per-candidate fold keeps a candidate's last refusal, but
/// a root is not a candidate: every tier that has it skips it (TASK-210),
/// and naming only the last read as if the others had not.
fn foreign_root_tiers<'a>(
    rows: impl Iterator<Item = (&'a str, Option<&'a Path>, Option<Reason>, Option<&'a str>)>,
) -> BTreeMap<&'a Path, Vec<&'a str>> {
    let mut tiers: BTreeMap<&Path, Vec<&str>> = BTreeMap::new();
    for (tier, candidate, reason, detail) in rows {
        if let Some(root) = candidate
            .filter(|_| reason == Some(Reason::OtherFs) && detail == Some(OTHER_FS_ROOT_DETAIL))
        {
            let seen = tiers.entry(root).or_default();
            if !seen.contains(&tier) {
                seen.push(tier);
            }
        }
    }
    tiers
}

/// [`KeptRow`]s, a foreign root's naming all its tiers.
fn with_root_tiers<'a>(
    mut rows: Vec<KeptRow<'a>>,
    root_tiers: &BTreeMap<&Path, Vec<&str>>,
) -> Vec<KeptRow<'a>> {
    for row in &mut rows {
        if let Some(tiers) = root_tiers.get(row.candidate)
            && row.reason == Reason::OtherFs
        {
            row.tier = tiers.join(", ");
        }
    }
    rows
}

/// One line per kept candidate, grouped by reason in the order the reasons
/// first appear; a reason with more than [`KEPT_EXAMPLES_MAX`] candidates
/// (or as many as [`grouped_from`] says) becomes one line with the first
/// one's diagnostic and examples. Live runs
/// and dry runs share it, so a dry run over thousands of candidates stays
/// as short as the run it previews (TASK-193).
fn kept_lines(kept: &[KeptRow<'_>], tier_label: &dyn Fn(&str) -> String) -> Vec<String> {
    let mut reasons: Vec<Reason> = Vec::new();
    for row in kept {
        if !reasons.contains(&row.reason) {
            reasons.push(row.reason);
        }
    }
    let mut lines = Vec::new();
    for reason in reasons {
        let group: Vec<&KeptRow<'_>> = kept.iter().filter(|r| r.reason == reason).collect();
        let count = group.iter().map(|r| r.count).fold(0, u64::saturating_add);
        if count < grouped_from(reason) {
            for row in group {
                lines.push(format!(
                    "  kept {}: {}{}{}",
                    one_line(&row.candidate.display().to_string()),
                    why(reason, row.diagnostic.as_deref()),
                    row.evidence,
                    tier_label(&row.tier)
                ));
            }
        } else {
            let examples: Vec<String> = group
                .iter()
                .take(KEPT_EXAMPLES_MAX)
                .map(|r| one_line(&r.candidate.display().to_string()))
                .collect();
            let more = count.saturating_sub(examples.len() as u64);
            lines.push(format!(
                "  {count} kept: {}; examples: {}{}{}",
                reason_label(reason),
                examples.join(", "),
                if more == 0 {
                    String::new()
                } else {
                    format!(" ({more} more)")
                },
                group[0]
                    .diagnostic
                    .as_ref()
                    .map_or_else(String::new, |d| format!("; e.g. {d}"))
            ));
        }
    }
    lines
}

/// A reason as the summary names it: its JSON name, except where that
/// reads as code to a person (`keep_marker`: "kept PATH: keep marker").
fn reason_label(reason: Reason) -> &'static str {
    match reason {
        Reason::KeepMarker => "keep marker",
        other => other.as_str(),
    }
}

/// `REASON, DIAGNOSTIC`, or the reason alone.
fn why(reason: Reason, diagnostic: Option<&str>) -> String {
    match diagnostic {
        Some(diagnostic) => format!("{}, {diagnostic}", reason_label(reason)),
        None => reason_label(reason).to_owned(),
    }
}

/// `REASON, DIAGNOSTIC` ([`diagnostic`]).
fn skip_why(skip: &SkipEntry) -> String {
    why(skip.reason, skip_diagnostic(skip).as_deref())
}

fn skip_diagnostic(skip: &SkipEntry) -> Option<String> {
    plain_diagnostic(
        skip.reason,
        skip.unknown_cause,
        skip.detail.as_deref(),
        skip.process_blockers.as_deref(),
    )
    .or_else(|| diagnostic(skip.detail.as_deref(), skip.process_blockers.as_deref()))
}

/// The diagnostic for a reason whose refusal detail is for diagnosis, not
/// for the user: `in_use_unknown`'s names attempts and freshness bounds,
/// and the line [`busy_line`] says what to do. One whose scan could not
/// inspect a process of the user (TASK-190) names that process instead:
/// it is not the host's load, and "used by" would claim a use nobody saw.
/// A `producer_unknown` whose cause is a process list diskgc cannot see
/// whole names that cause in words, not the contract row that met it
/// (TASK-221); [`hidden_processes_line`] says what to do.
fn plain_diagnostic(
    reason: Reason,
    cause: Option<UnknownCause>,
    detail: Option<&str>,
    blockers: Option<&[ProcessBlocker]>,
) -> Option<String> {
    if reason == Reason::ProducerUnknown {
        return hidden_processes(reason, detail).map(|hidden| hidden.why().to_owned());
    }
    if reason != Reason::InUseUnknown {
        return None;
    }
    Some(match blockers.and_then(<[ProcessBlocker]>::first) {
        Some(blocker) => format!(
            "pid {}{} cannot be inspected, so whether it is in use cannot be told",
            blocker.pid,
            blocker
                .command
                .as_deref()
                .map_or_else(String::new, |command| format!(" ({})", one_line(command)))
        ),
        // A cause that stays (TASK-263, TASK-169) says itself; only a
        // busy host (or no cause recorded) reads as one.
        None => match (cause, detail) {
            (Some(cause), Some(detail)) if cause.stays() => brief(detail),
            _ => "the host was too busy to check whether it is in use".to_owned(),
        },
    })
}

/// Whether an `in_use_unknown` counts toward [`busy_line`]: its cause does
/// not stay ([`UnknownCause::stays`]; a skip without one counts).
fn busy(cause: Option<UnknownCause>) -> bool {
    !cause.is_some_and(UnknownCause::stays)
}

/// Why diskgc cannot see every process, when that kept a candidate
/// `producer_unknown` (TASK-221): a liveness contract that looks up process
/// ids permits nothing then (docs/safety.md, Producer liveness).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HiddenProcesses {
    /// diskgc is not in the initial PID namespace: most containers.
    OwnPidNamespace,
    /// The held `/proc` is mounted with `hidepid`.
    Hidepid,
}

impl HiddenProcesses {
    fn why(self) -> &'static str {
        match self {
            Self::OwnPidNamespace => {
                "diskgc runs in its own PID namespace (a container) and cannot see every process"
            }
            Self::Hidepid => "/proc is mounted with hidepid, which hides other processes",
        }
    }

    fn what_to_do(self) -> &'static str {
        match self {
            Self::OwnPidNamespace => "run ensure on the host (outside any container)",
            Self::Hidepid => "run ensure where /proc is mounted without hidepid",
        }
    }
}

/// The cause of a `producer_unknown` refusal, if it is one diskgc's
/// environment imposes rather than the candidate: recognised by the texts
/// [`crate::liveness`] gives those causes, which every such detail ends
/// with (the JSON's `detail`; its `reason` is the same for every cause).
fn hidden_processes(reason: Reason, detail: Option<&str>) -> Option<HiddenProcesses> {
    let detail = detail.filter(|_| reason == Reason::ProducerUnknown)?;
    if detail.ends_with(NOT_INITIAL_PID_NAMESPACE) {
        Some(HiddenProcesses::OwnPidNamespace)
    } else if detail.ends_with(HIDEPID) {
        Some(HiddenProcesses::Hidepid)
    } else {
        None
    }
}

/// One line per cause that kept candidates `producer_unknown` because diskgc
/// cannot see every process: why, and what to do. The reason name and the
/// contract's row say neither (TASK-221: a CI job in a container kept every
/// `/tmp` candidate and learned why only from the release notes).
fn hidden_processes_line<'a>(
    kept: impl Iterator<Item = (Reason, Option<&'a str>, u64)>,
) -> Vec<String> {
    let mut counts: Vec<(HiddenProcesses, u64)> = Vec::new();
    for (reason, detail, count) in kept {
        let Some(hidden) = hidden_processes(reason, detail) else {
            continue;
        };
        match counts.iter_mut().find(|(seen, _)| *seen == hidden) {
            Some((_, total)) => *total = total.saturating_add(count),
            None => counts.push((hidden, count)),
        }
    }
    counts
        .into_iter()
        .map(|(hidden, count)| {
            format!(
                "  {count} candidate(s) kept producer_unknown: {}, so whether their producer \
                 ended cannot be told; {} to reclaim them",
                hidden.why(),
                hidden.what_to_do()
            )
        })
        .collect()
}

/// What a user does about candidates kept as `in_use_unknown` (TASK-144):
/// the reason name alone does not say that a later run may reclaim them.
/// `count` is of final verdicts, so a candidate a later tier reclaimed or
/// refused for another reason is not counted.
fn busy_line(count: u64) -> Option<String> {
    (count > 0).then(|| {
        format!(
            "  the host was too busy to check {count} candidate(s) for use (in_use_unknown): \
             kept; run again when the host is less busy"
        )
    })
}

/// The processes that hold a candidate, when attributed (the full
/// evidence is in the JSON and on stderr), else the refusal's diagnostic
/// cut to one bounded line.
fn diagnostic(detail: Option<&str>, blockers: Option<&[ProcessBlocker]>) -> Option<String> {
    if let Some(blockers) = blockers.filter(|b| !b.is_empty()) {
        let mut pids: Vec<(u32, Option<&str>)> = Vec::new();
        for blocker in blockers {
            if !pids.iter().any(|(pid, _)| *pid == blocker.pid) {
                pids.push((blocker.pid, blocker.command.as_deref()));
            }
        }
        let named: Vec<String> = pids
            .iter()
            .take(BLOCKERS_MAX)
            .map(|(pid, command)| match command {
                Some(command) => format!("pid {pid} ({})", one_line(command)),
                None => format!("pid {pid}"),
            })
            .collect();
        let more = pids.len().saturating_sub(BLOCKERS_MAX);
        return Some(format!(
            "used by {}{}",
            named.join(", "),
            if more == 0 {
                String::new()
            } else {
                format!(" and {more} more")
            }
        ));
    }
    detail.filter(|d| !d.is_empty()).map(brief)
}

fn dry_run_text(report: &Report, held: Option<String>) -> String {
    let needed = outcome::shortfall(
        report.avail_before_bytes,
        report.min_free_bytes,
        report.target_free_bytes,
    );
    let folded = outcome::dry_run(&report.plan, needed);
    // A deadline stops the planners part-way (TASK-227): the rows are the
    // candidates judged before it, and the report does not count the ones
    // found but not yet judged, so the text cannot either (TASK-229).
    let cut = report.status == Status::Deadline;
    let [available, min_free, target_free] = human_space_values([
        report.avail_before_bytes,
        report.min_free_bytes,
        report.target_free_bytes,
    ]);
    let status = match report.status {
        Status::DryRun => "dry run".to_owned(),
        other => format!("dry run, {}", other.as_str()),
    };
    // The colon is added below, only when lines follow it (TASK-216).
    let mut lines = vec![format!(
        "{status}: {available} available on {} (min-free {min_free}, target-free {target_free}); \
         {} (took {})",
        mount_text(report),
        dry_run_outcome(&folded.rows, needed, cut),
        elapsed_text(report)
    )];
    if cut {
        lines.push(
            "  the deadline passed: only the candidates judged before it are listed, not any \
             found but not yet judged; run again with a larger --timeout for a complete plan"
                .to_owned(),
        );
    }
    let tiers: BTreeSet<&str> = report.plan.iter().map(|p| p.tier.as_str()).collect();
    let tier_label = |tier: &str| {
        if tiers.len() > 1 {
            format!(" [{}]", one_line(tier))
        } else {
            String::new()
        }
    };
    let of = |verdict: Verdict| -> Vec<&PlanEntry> {
        folded
            .rows
            .iter()
            .filter(|row| row.verdict == verdict)
            .map(|row| row.entry)
            .collect()
    };
    lines.extend(eligible_lines(
        &of(Verdict::WouldReclaim),
        "would reclaim",
        &tier_label,
    ));
    lines.extend(eligible_lines(
        &of(Verdict::NotNeeded),
        "eligible, not needed",
        &tier_label,
    ));
    let root_tiers = foreign_root_tiers(report.plan.iter().map(|entry| {
        (
            entry.tier.as_str(),
            entry.candidate.as_deref(),
            entry.reason,
            entry.detail.as_deref(),
        )
    }));
    let kept_rows = with_root_tiers(
        of(Verdict::Kept)
            .into_iter()
            .map(KeptRow::of_plan)
            .collect(),
        &root_tiers,
    );
    lines.extend(kept_lines(&kept_rows, &tier_label));
    lines.extend(busy_line(
        folded
            .rows
            .iter()
            .filter(|row| {
                row.entry.reason == Some(Reason::InUseUnknown) && busy(row.entry.unknown_cause)
            })
            .count() as u64,
    ));
    lines.extend(hidden_processes_line(folded.rows.iter().filter_map(
        |row| {
            row.entry
                .reason
                .map(|reason| (reason, row.entry.detail.as_deref(), 1))
        },
    )));
    lines.extend(folded.preflight.iter().map(|entry| {
        format!(
            "  skipped profile {} [{}]: {}",
            one_line(&entry.profile),
            one_line(&entry.tier),
            match entry.reason {
                Some(reason) => why(
                    reason,
                    diagnostic(entry.detail.as_deref(), entry.process_blockers.as_deref())
                        .as_deref()
                ),
                None => "eligible".to_owned(),
            }
        )
    }));
    let mut operations: Vec<(&str, &str)> = Vec::new();
    for entry in &report.plan {
        let pair = (entry.profile.as_str(), entry.operation.as_str());
        if !operations.contains(&pair) {
            operations.push(pair);
        }
    }
    if !operations.is_empty() {
        lines.push(format!(
            "  operations: {}",
            operations
                .iter()
                .map(|(profile, operation)| format!(
                    "{}: {}",
                    one_line(profile),
                    one_line(operation)
                ))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    if !report.errors.is_empty() {
        lines.push(format!("  {} error(s): see stderr", report.errors.len()));
    }
    lines.extend(held);
    if lines.len() > 1 {
        lines[0].push(':');
    }
    lines.join("\n")
}

/// A dry run's eligible candidates of one verdict (`label`), in the order a
/// reclaim would take them: the first [`RECLAIMED_LINES_MAX`] one per line,
/// the rest counted with their summed estimate.
fn eligible_lines(
    entries: &[&PlanEntry],
    label: &str,
    tier_label: &dyn Fn(&str) -> String,
) -> Vec<String> {
    let mut lines: Vec<String> = entries
        .iter()
        .take(RECLAIMED_LINES_MAX)
        .map(|entry| {
            let path = one_line(
                &entry
                    .candidate
                    .as_deref()
                    .expect("a planned candidate has a path (preflight rows are apart)")
                    .display()
                    .to_string(),
            );
            let estimate = entry.estimated_bytes.map_or_else(
                || "size unknown".to_owned(),
                |b| format!("est. {}", format_bytes(b)),
            );
            format!(
                "  {label} {path}: {estimate}{}{}",
                evidence_text(entry),
                tier_label(&entry.tier)
            )
        })
        .collect();
    let rest = &entries[entries.len().min(RECLAIMED_LINES_MAX)..];
    if !rest.is_empty() {
        let estimated = rest
            .iter()
            .filter_map(|e| e.estimated_bytes)
            .fold(0, u64::saturating_add);
        let unestimated = rest.iter().filter(|e| e.estimated_bytes.is_none()).count();
        lines.push(format!(
            "  {} more {label}: est. {}{} (--json lists every candidate)",
            rest.len(),
            format_bytes(estimated),
            if unestimated == 0 {
                String::new()
            } else {
                format!(", {unestimated} without an estimate")
            }
        ));
    }
    lines
}

/// What a reclaim would do, by the estimates: how much of the shortfall
/// the candidates it would take cover. `cut`: the deadline stopped the
/// plan, so the counts and verdict are of the candidates judged before it
/// and a goal they miss may still be reachable.
fn dry_run_outcome(rows: &[Planned<'_>], needed: Option<u64>, cut: bool) -> String {
    let count = |verdict: Verdict| rows.iter().filter(|r| r.verdict == verdict).count();
    let kept = count(Verdict::Kept);
    let would: Vec<&PlanEntry> = rows
        .iter()
        .filter(|r| r.verdict == Verdict::WouldReclaim)
        .map(|r| r.entry)
        .collect();
    let eligible = would.len() + count(Verdict::NotNeeded);
    let estimated: u64 = would
        .iter()
        .filter_map(|e| e.estimated_bytes)
        .fold(0, u64::saturating_add);
    let unestimated = would.iter().filter(|e| e.estimated_bytes.is_none()).count();
    let text = match needed {
        None => format!("{eligible} eligible, {kept} kept; the space needed is unknown"),
        Some(0) => format!(
            "at least min-free already: a reclaim would take nothing ({eligible} eligible, {kept} kept)"
        ),
        Some(needed) if eligible == 0 => format!(
            "nothing is eligible, {} needed: the goal is not reachable{}; {kept} kept",
            format_bytes(needed),
            if cut { " by these" } else { "" }
        ),
        Some(needed) => {
            let estimate = if unestimated == 0 {
                format!("est. {}", format_bytes(estimated))
            } else {
                format!(
                    "est. at least {} ({unestimated} without an estimate)",
                    format_bytes(estimated)
                )
            };
            let verdict = if estimated >= needed {
                "the goal is reachable by estimate".to_owned()
            } else if unestimated > 0 {
                "whether the goal is reachable is unknown".to_owned()
            } else {
                format!(
                    "short of the goal by {} by estimate",
                    format_bytes(needed - estimated)
                )
            };
            format!(
                "would reclaim {} of {eligible} eligible, {estimate} of {} needed: {verdict}; {kept} kept",
                would.len(),
                format_bytes(needed)
            )
        }
    };
    if cut {
        format!("judged before the deadline: {text}")
    } else {
        text
    }
}

fn evidence_text(entry: &PlanEntry) -> String {
    entry.build_evidence.as_ref().map_or_else(String::new, |e| {
        format!(
            " (marker {}, source {}, idle {})",
            e.marker,
            one_line(&e.source.display().to_string()),
            e.idle_age_seconds
                .map_or_else(|| "unknown".to_owned(), format_age)
        )
    })
}

/// Holders of deleted files that pin space. A holder of 0 B (an empty
/// deleted temp file) pins nothing and is left out.
fn held_line(held: &[HeldEntry]) -> Option<String> {
    let mut top: Vec<&HeldEntry> = held.iter().filter(|entry| entry.bytes > 0).collect();
    if top.is_empty() {
        return None;
    }
    let bytes: u128 = top.iter().map(|entry| u128::from(entry.bytes)).sum();
    let bytes = u64::try_from(bytes).map_or_else(|_| format!("{bytes} B"), format_bytes);
    top.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.pid.cmp(&b.pid)));
    let examples = top
        .iter()
        .take(HELD_TOP_MAX)
        .map(|entry| {
            format!(
                "pid {} ({}, {})",
                entry.pid,
                entry
                    .command
                    .as_deref()
                    .map_or_else(|| "command unknown".to_owned(), one_line),
                format_bytes(entry.bytes)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "  held by deleted files: {} process(es), {bytes}; top: {examples}",
        top.len()
    ))
}

/// `value` on one line: control characters and backslashes escaped, so a
/// path cannot forge a line of the summary.
fn one_line(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_control() || ch == '\\' {
            out.extend(ch.escape_default());
        } else {
            out.push(ch);
        }
    }
    out
}

/// [`one_line`], cut to [`DETAIL_CHARS_MAX`] characters.
fn brief(value: &str) -> String {
    let line = one_line(value);
    let mut chars = line.chars();
    let head: String = chars.by_ref().take(DETAIL_CHARS_MAX).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}
