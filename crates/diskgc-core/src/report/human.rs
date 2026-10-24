//! The human-mode result of `ensure` (docs/reference.md, Output): a pure
//! rendering of [`outcome`]'s per-candidate verdicts. One line per
//! candidate, `VERB PATH: WHY`, so a reader (or `grep`) finds a path once
//! with its final outcome. Bounded: groups of kept candidates and long
//! lists of reclaimed ones are summarised with examples.

use std::collections::BTreeSet;

use super::outcome::{self, Live, Planned, Verdict};
use super::{HeldEntry, PlanEntry, Report, SkipEntry, Status, human_space_values, signed_bytes};
use crate::engine::Reason;
use crate::units::{format_age, format_bytes};

/// Kept candidates of one reason listed one per line up to this many;
/// above it, one line with this many examples.
const KEPT_EXAMPLES_MAX: usize = 5;
/// Reclaimed candidates listed one per line, the rest counted.
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
    let held = held_line(&report.held_by_deleted_files);
    let is_dry_run = report.status == Status::DryRun || !report.plan.is_empty();
    match report.status {
        Status::UsageError => None,
        _ if is_dry_run => Some(dry_run_text(report, held)),
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
    lines.extend(kept_lines(&live.kept, &tier_label));
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

/// One line per kept candidate, grouped by reason in the order the reasons
/// first appear; a reason with more than [`KEPT_EXAMPLES_MAX`] candidates
/// becomes one line with the first one's diagnostic and examples.
fn kept_lines(kept: &[&SkipEntry], tier_label: &dyn Fn(&str) -> String) -> Vec<String> {
    let mut reasons: Vec<Reason> = Vec::new();
    for skip in kept {
        if !reasons.contains(&skip.reason) {
            reasons.push(skip.reason);
        }
    }
    let mut lines = Vec::new();
    for reason in reasons {
        let group: Vec<&SkipEntry> = kept
            .iter()
            .copied()
            .filter(|s| s.reason == reason)
            .collect();
        let count = group.iter().map(|s| s.count).fold(0, u64::saturating_add);
        if count <= KEPT_EXAMPLES_MAX as u64 {
            for skip in group {
                lines.push(format!(
                    "  kept {}: {}{}",
                    one_line(&skip.candidate.display().to_string()),
                    skip_why(skip),
                    tier_label(&skip.tier)
                ));
            }
        } else {
            let examples: Vec<String> = group
                .iter()
                .take(KEPT_EXAMPLES_MAX)
                .map(|s| one_line(&s.candidate.display().to_string()))
                .collect();
            let more = count.saturating_sub(examples.len() as u64);
            lines.push(format!(
                "  {count} kept: {reason}; examples: {}{}{}",
                examples.join(", "),
                if more == 0 {
                    String::new()
                } else {
                    format!(" ({more} more)")
                },
                skip_diagnostic(group[0]).map_or_else(String::new, |d| format!("; the first: {d}"))
            ));
        }
    }
    lines
}

/// `REASON, DIAGNOSTIC` ([`skip_diagnostic`]).
fn skip_why(skip: &SkipEntry) -> String {
    match skip_diagnostic(skip) {
        Some(diagnostic) => format!("{}, {diagnostic}", skip.reason),
        None => skip.reason.as_str().to_owned(),
    }
}

/// The processes that hold an `in_use` candidate, when attributed, else
/// the refusal's diagnostic cut to one bounded line.
fn skip_diagnostic(skip: &SkipEntry) -> Option<String> {
    if let Some(blockers) = skip.process_blockers.as_deref().filter(|b| !b.is_empty()) {
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
            "held by {}{}",
            named.join(", "),
            if more == 0 {
                String::new()
            } else {
                format!(" and {more} more")
            }
        ));
    }
    skip.detail.as_deref().filter(|d| !d.is_empty()).map(brief)
}

fn with_detail(reason: &str, detail: Option<&str>) -> String {
    match detail.filter(|d| !d.is_empty()) {
        Some(detail) => format!("{reason}, {}", brief(detail)),
        None => reason.to_owned(),
    }
}

fn dry_run_text(report: &Report, held: Option<String>) -> String {
    let needed = outcome::shortfall(
        report.avail_before_bytes,
        report.min_free_bytes,
        report.target_free_bytes,
    );
    let folded = outcome::dry_run(&report.plan, needed);
    let [available, min_free, target_free] = human_space_values([
        report.avail_before_bytes,
        report.min_free_bytes,
        report.target_free_bytes,
    ]);
    let status = match report.status {
        Status::DryRun => "dry run".to_owned(),
        other => format!("dry run, {}", other.as_str()),
    };
    let mut lines = vec![format!(
        "{status}: {available} available on {} (min-free {min_free}, target-free {target_free}); \
         {} (took {}):",
        mount_text(report),
        dry_run_outcome(&folded.rows, needed),
        elapsed_text(report)
    )];
    let tiers: BTreeSet<&str> = report.plan.iter().map(|p| p.tier.as_str()).collect();
    for Planned { entry, verdict } in &folded.rows {
        let path = one_line(&entry.candidate.display().to_string());
        let tier = if tiers.len() > 1 {
            format!(" [{}]", one_line(&entry.tier))
        } else {
            String::new()
        };
        let estimate = || {
            entry.estimated_bytes.map_or_else(
                || "size unknown".to_owned(),
                |b| format!("est. {}", format_bytes(b)),
            )
        };
        let evidence = evidence_text(entry);
        lines.push(match verdict {
            Verdict::WouldReclaim => {
                format!("  would reclaim {path}: {}{evidence}{tier}", estimate())
            }
            Verdict::NotNeeded => {
                format!(
                    "  eligible, not needed {path}: {}{evidence}{tier}",
                    estimate()
                )
            }
            Verdict::Kept => format!(
                "  kept {path}: {}{evidence}{tier}",
                with_detail(
                    entry.reason.map_or("eligible", Reason::as_str),
                    entry.detail.as_deref()
                )
            ),
        });
    }
    lines.extend(folded.preflight.iter().map(|entry| {
        format!(
            "  skipped profile {} [{}]: {}",
            one_line(&entry.profile),
            one_line(&entry.tier),
            with_detail(
                entry.reason.map_or("eligible", Reason::as_str),
                entry.detail.as_deref()
            )
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
    lines.join("\n")
}

/// What a reclaim would do, by the estimates: how much of the shortfall
/// the candidates it would take cover.
fn dry_run_outcome(rows: &[Planned<'_>], needed: Option<u64>) -> String {
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
    match needed {
        None => format!("{eligible} eligible, {kept} kept; the space needed is unknown"),
        Some(0) => format!(
            "at least min-free already: a reclaim would take nothing ({eligible} eligible, {kept} kept)"
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
                one_line(&entry.command),
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
pub(super) fn one_line(value: &str) -> String {
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
