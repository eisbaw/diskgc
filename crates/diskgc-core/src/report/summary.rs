//! The per-term summary of a run (docs/reference.md, Output: `summary`):
//! for each source term (a tier's source profile), what was found, what was
//! or would be reclaimed, and what was kept, counted and sized by reason.
//! A pure function of the report's lists, so the JSON `summary`, the human
//! result and `watch`'s cycle line cannot disagree.
//!
//! Rows are counted per tier, not folded per candidate as the human
//! per-candidate lines are ([`super::outcome`]): a candidate a stale tier
//! keeps `not_idle` and a pressure tier takes is kept in the first block
//! and eligible in the second, which is what each tier did. Only the
//! dry run's `would_reclaim` is folded: a reclaim takes each candidate
//! once, by the first tier that finds it eligible.
//!
//! Sizes are what the run measured, never extra I/O: an eligible row's
//! estimate, a kept row's when its walk was complete before the refusal
//! ([`crate::engine::Skip::measured_bytes`]), else it is counted `unsized`.

use std::collections::BTreeSet;
use std::path::Path;

use super::outcome::{self, Verdict};
use super::{PlanEntry, Report};
use crate::engine::Reason;

/// Candidates with what is known of their size: `bytes` sums the sized
/// ones, `unmeasured` counts the others (JSON `unsized`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sized {
    pub count: u64,
    pub bytes: u64,
    pub unmeasured: u64,
}

impl Sized {
    fn add(&mut self, count: u64, bytes: Option<u64>) {
        self.count = self.count.saturating_add(count);
        match bytes {
            Some(bytes) => self.bytes = self.bytes.saturating_add(bytes),
            None => self.unmeasured = self.unmeasured.saturating_add(count),
        }
    }
}

/// What a dry run planned for one term.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Planned {
    /// Every eligible row of the term.
    pub eligible: Sized,
    /// The eligible rows a reclaim would take before reaching the goal
    /// ([`outcome::dry_run`]).
    pub would_reclaim: Sized,
    /// Candidates found but not judged: the plan stopped first. `None`
    /// when the term's planner did not report (the run stopped before it,
    /// or it failed).
    pub not_judged: Option<u64>,
}

/// What a live run reclaimed for one term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reclaimed {
    /// Candidates with at least one action in this tier.
    pub count: u64,
    /// The sum of their observed changes in available space; `None` when
    /// any is unknown.
    pub avail_delta_bytes: Option<i64>,
}

/// One term's block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term<'a> {
    pub tier: &'a str,
    pub profile: &'a str,
    /// A dry run's: `Some` exactly when the report is a dry run.
    pub planned: Option<Planned>,
    /// A live run's: `Some` exactly when it is not.
    pub reclaimed: Option<Reclaimed>,
    /// By reason, in the order the reasons first appear.
    pub kept: Vec<(Reason, Sized)>,
}

impl Term<'_> {
    /// Every kept candidate of the term.
    pub fn kept_count(&self) -> u64 {
        self.kept
            .iter()
            .map(|(_, sized)| sized.count)
            .fold(0, u64::saturating_add)
    }

    /// A dry run's candidates found: eligible, kept, and not judged.
    /// `None` for a live run (a reclaim judges a candidate only when it
    /// takes it, and stops at the goal) and when `not_judged` is unknown.
    pub fn found(&self) -> Option<u64> {
        let planned = self.planned?;
        Some(
            planned
                .eligible
                .count
                .saturating_add(self.kept_count())
                .saturating_add(planned.not_judged?),
        )
    }

    fn kept_row(&mut self, reason: Reason, count: u64, bytes: Option<u64>) {
        match self.kept.iter_mut().find(|(seen, _)| *seen == reason) {
            Some((_, sized)) => sized.add(count, bytes),
            None => {
                let mut sized = Sized::default();
                sized.add(count, bytes);
                self.kept.push((reason, sized));
            }
        }
    }
}

/// The report's terms, in run order. A row without a candidate (a profile
/// refused before discovery) is not a candidate and is not counted; the
/// summary lists it apart.
pub fn terms(report: &Report) -> Vec<Term<'_>> {
    if report.dry_run {
        dry_run_terms(report)
    } else {
        live_terms(report)
    }
}

/// The term `tier`/`profile` in `terms`, added (empty) if missing.
fn term<'t, 'a>(
    terms: &'t mut Vec<Term<'a>>,
    tier: &'a str,
    profile: &'a str,
    dry_run: bool,
) -> &'t mut Term<'a> {
    let index = match terms
        .iter()
        .position(|t| t.tier == tier && t.profile == profile)
    {
        Some(index) => index,
        None => {
            terms.push(Term {
                tier,
                profile,
                planned: dry_run.then(Planned::default),
                reclaimed: (!dry_run).then_some(Reclaimed {
                    count: 0,
                    avail_delta_bytes: Some(0),
                }),
                kept: Vec::new(),
            });
            terms.len() - 1
        }
    };
    &mut terms[index]
}

fn dry_run_terms(report: &Report) -> Vec<Term<'_>> {
    let needed = outcome::shortfall(
        report.avail_before_bytes,
        report.min_free_bytes,
        report.target_free_bytes,
    );
    let folded = outcome::dry_run(&report.plan, needed);
    // The fold's rows are the plan's own entries: told apart by address.
    let would: BTreeSet<*const PlanEntry> = folded
        .rows
        .iter()
        .filter(|row| row.verdict == Verdict::WouldReclaim)
        .map(|row| std::ptr::from_ref(row.entry))
        .collect();
    let mut terms: Vec<Term<'_>> = Vec::new();
    // The planners' terms first, in tier order: a term that found nothing
    // still has its block ("0 found" explains an empty plan).
    for planned in &report.planned_terms {
        let term = term(&mut terms, &planned.tier, &planned.profile, true);
        let counts = term.planned.as_mut().expect("a dry run's term");
        counts.not_judged = Some(
            counts
                .not_judged
                .unwrap_or(0)
                .saturating_add(planned.not_judged),
        );
    }
    for entry in report.plan.iter().filter(|e| e.candidate.is_some()) {
        let term = term(&mut terms, &entry.tier, &entry.profile, true);
        match entry.reason {
            None => {
                let counts = term.planned.as_mut().expect("a dry run's term");
                counts.eligible.add(1, entry.estimated_bytes);
                if would.contains(&std::ptr::from_ref(entry)) {
                    counts.would_reclaim.add(1, entry.estimated_bytes);
                }
            }
            Some(reason) => term.kept_row(reason, 1, entry.estimated_bytes),
        }
    }
    terms
}

fn live_terms(report: &Report) -> Vec<Term<'_>> {
    let mut terms: Vec<Term<'_>> = Vec::new();
    // A candidate with an action in a tier counts as reclaimed there, also
    // when that reclaim stopped part-way (its skip is not a keep).
    let mut acted: BTreeSet<(&str, &Path)> = BTreeSet::new();
    for action in &report.actions {
        let term = term(&mut terms, &action.tier, &action.profile, false);
        let reclaimed = term.reclaimed.as_mut().expect("a live run's term");
        if acted.insert((&action.tier, &action.candidate)) {
            reclaimed.count = reclaimed.count.saturating_add(action.count);
        }
        reclaimed.avail_delta_bytes = reclaimed
            .avail_delta_bytes
            .zip(action.avail_delta_bytes)
            .map(|(sum, delta)| sum.saturating_add(delta));
    }
    for skip in &report.skipped {
        let Some(candidate) = skip.candidate.as_deref() else {
            continue;
        };
        if acted.contains(&(skip.tier.as_str(), candidate)) {
            continue;
        }
        term(&mut terms, &skip.tier, &skip.profile, false).kept_row(
            skip.reason,
            skip.count,
            skip.estimated_bytes,
        );
    }
    terms
}
