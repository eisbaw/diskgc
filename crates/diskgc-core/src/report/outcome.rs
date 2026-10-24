//! What became of each candidate over a whole run, every tier folded into
//! one verdict per candidate: the data the human summary renders
//! (docs/reference.md, Output). Pure functions of the report's lists.
//!
//! The JSON keeps one row per (tier, candidate), which is what a program
//! wants. A person asks per candidate: "was fw-nrf/target reclaimed, and if
//! not, why?". Listing the per-tier rows instead made a target a later tier
//! reclaimed read as kept (TASK-155).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{ActionEntry, PlanEntry, SkipEntry};
use crate::engine::Reason;

/// A candidate one or more actions reclaimed. A deletion part-way and a
/// later completion of the same candidate are one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimed<'a> {
    pub candidate: &'a Path,
    /// The tier of its last action.
    pub tier: &'a str,
    /// The sum of its actions' observed changes; `None` if any is unknown.
    pub avail_delta_bytes: Option<i64>,
    /// A refusal of it in the tier of one of its actions: that reclaim
    /// deleted entries part-way, then stopped for this reason (executor:
    /// `Skipped` with deleted entries is an action and a skip). An earlier
    /// tier's refusal is not one: a later tier then took the candidate.
    pub part_way: Option<&'a SkipEntry>,
}

/// A live run (`ensure` without `--dry-run`), folded per candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live<'a> {
    /// In the order of each candidate's first action.
    pub reclaimed: Vec<Reclaimed<'a>>,
    /// Each candidate no action reclaimed, with the refusal of the last tier
    /// that judged it, in the order of its first refusal. Excludes `empty`
    /// (a Cargo target its previous reclaim left with lock files only is
    /// not news) and candidate-less rows ([`Live::preflight`]).
    pub kept: Vec<&'a SkipEntry>,
    /// Profiles refused before they had a candidate (empty `candidate`).
    pub preflight: Vec<&'a SkipEntry>,
}

/// Fold a live run's `actions` and `skipped` (each in the order the run
/// produced them, tier by tier) into one verdict per candidate.
pub fn live<'a>(actions: &'a [ActionEntry], skipped: &'a [SkipEntry]) -> Live<'a> {
    let mut reclaimed: Vec<Reclaimed<'a>> = Vec::new();
    let mut reclaimed_at: BTreeMap<&Path, usize> = BTreeMap::new();
    let mut action_tiers: BTreeSet<(&Path, &str)> = BTreeSet::new();
    for action in actions {
        action_tiers.insert((&action.candidate, &action.tier));
        assert!(
            !action.candidate.as_os_str().is_empty(),
            "the report's own actions name their candidate (only the JSON aggregates)"
        );
        match reclaimed_at.get(action.candidate.as_path()) {
            Some(&index) => {
                let entry = &mut reclaimed[index];
                entry.tier = &action.tier;
                entry.avail_delta_bytes = entry
                    .avail_delta_bytes
                    .zip(action.avail_delta_bytes)
                    .map(|(sum, delta)| sum.saturating_add(delta));
            }
            None => {
                reclaimed_at.insert(&action.candidate, reclaimed.len());
                reclaimed.push(Reclaimed {
                    candidate: &action.candidate,
                    tier: &action.tier,
                    avail_delta_bytes: action.avail_delta_bytes,
                    part_way: None,
                });
            }
        }
    }
    let mut kept: Vec<&'a SkipEntry> = Vec::new();
    let mut kept_at: BTreeMap<&Path, usize> = BTreeMap::new();
    let mut preflight = Vec::new();
    for skip in skipped.iter().filter(|skip| skip.reason != Reason::Empty) {
        if skip.candidate.as_os_str().is_empty() {
            preflight.push(skip);
            continue;
        }
        if let Some(&index) = reclaimed_at.get(skip.candidate.as_path()) {
            if action_tiers.contains(&(skip.candidate.as_path(), skip.tier.as_str())) {
                reclaimed[index].part_way = Some(skip);
            }
            continue;
        }
        match kept_at.get(skip.candidate.as_path()) {
            // A later tier's refusal replaces an earlier one's.
            Some(&index) => kept[index] = skip,
            None => {
                kept_at.insert(&skip.candidate, kept.len());
                kept.push(skip);
            }
        }
    }
    Live {
        reclaimed,
        kept,
        preflight,
    }
}

/// What a reclaim would do with a candidate a dry run planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Eligible and, by the estimates in plan order, taken before the goal
    /// is reached (or the goal is unknown).
    WouldReclaim,
    /// Eligible, but the estimates before it already reach the goal.
    NotNeeded,
    /// No tier found it eligible; the row is the last tier's refusal.
    Kept,
}

/// One candidate of a dry run with the plan row that decided it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Planned<'a> {
    pub entry: &'a PlanEntry,
    pub verdict: Verdict,
}

/// A dry run, folded per candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRun<'a> {
    /// Eligible candidates in the order a reclaim would take them, then the
    /// kept ones in the order of their first row.
    pub rows: Vec<Planned<'a>>,
    /// Rows without a candidate (a profile refused before discovery).
    pub preflight: Vec<&'a PlanEntry>,
}

/// The bytes a reclaim would have to free: 0 when `avail` is already at
/// least `min_free` (`ensure` then reclaims nothing), else up to
/// `target_free`; `None` when any of them is unknown.
pub fn shortfall(
    avail: Option<u64>,
    min_free: Option<u64>,
    target_free: Option<u64>,
) -> Option<u64> {
    let (avail, min_free, target_free) = (avail?, min_free?, target_free?);
    Some(if avail >= min_free {
        0
    } else {
        target_free.saturating_sub(avail)
    })
}

/// Fold a dry run's `plan` (tier by tier, each tier's eligible rows in the
/// order a reclaim takes them) into one verdict per candidate. A candidate
/// is decided by its first eligible row (the earliest tier that would take
/// it), else by its last refusal. `needed` ([`shortfall`]) splits the
/// eligible ones: estimates are summed in order until they cover it; an
/// unknown estimate adds nothing, so the rows after it stay `WouldReclaim`.
pub fn dry_run(plan: &[PlanEntry], needed: Option<u64>) -> DryRun<'_> {
    let mut eligible: Vec<&PlanEntry> = Vec::new();
    // `None`: the candidate was refused first and taken by a later tier.
    let mut kept: Vec<Option<&PlanEntry>> = Vec::new();
    let mut kept_at: BTreeMap<&Path, usize> = BTreeMap::new();
    let mut taken: BTreeSet<&Path> = BTreeSet::new();
    let mut preflight = Vec::new();
    for entry in plan {
        let candidate = entry.candidate.as_path();
        if candidate.as_os_str().is_empty() {
            preflight.push(entry);
        } else if taken.contains(candidate) {
            // An earlier tier would take it: later rows change nothing.
        } else if entry.reason.is_none() {
            taken.insert(candidate);
            eligible.push(entry);
            if let Some(&index) = kept_at.get(candidate) {
                kept[index] = None;
            }
        } else if let Some(&index) = kept_at.get(candidate) {
            kept[index] = Some(entry);
        } else {
            kept_at.insert(candidate, kept.len());
            kept.push(Some(entry));
        }
    }
    let mut covered: u64 = 0;
    let mut rows: Vec<Planned<'_>> = eligible
        .into_iter()
        .map(|entry| {
            let verdict = match needed {
                Some(needed) if covered >= needed => Verdict::NotNeeded,
                _ => {
                    covered = covered.saturating_add(entry.estimated_bytes.unwrap_or(0));
                    Verdict::WouldReclaim
                }
            };
            Planned { entry, verdict }
        })
        .collect();
    rows.extend(kept.into_iter().flatten().map(|entry| Planned {
        entry,
        verdict: Verdict::Kept,
    }));
    DryRun { rows, preflight }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use proptest::prelude::*;

    use super::*;
    use crate::engine::Checkouts;

    const REASONS: [Reason; 4] = [
        Reason::NotIdle,
        Reason::InUse,
        Reason::Protected,
        Reason::Empty,
    ];

    fn plan_entry(tier: u8, candidate: u8, reason: Option<Reason>, bytes: u64) -> PlanEntry {
        PlanEntry {
            group: String::new(),
            tier: format!("t{tier}"),
            profile: "p".into(),
            candidate: PathBuf::from(format!("/c{candidate}")),
            reason,
            estimated_bytes: reason.is_none().then_some(bytes),
            operation: String::new(),
            checkouts: Checkouts::Refuse,
            build_evidence: None,
            detail: None,
        }
    }

    /// Plans as a run makes them: tiers in order, each candidate at most
    /// once per tier.
    fn plans() -> impl Strategy<Value = Vec<PlanEntry>> {
        proptest::collection::vec(
            proptest::collection::btree_map(
                0u8..6,
                (proptest::option::of(0usize..4), 0u64..100),
                0..6,
            ),
            1..4,
        )
        .prop_map(|tiers| {
            tiers
                .into_iter()
                .enumerate()
                .flat_map(|(tier, rows)| {
                    rows.into_iter().map(move |(candidate, (reason, bytes))| {
                        plan_entry(tier as u8, candidate, reason.map(|r| REASONS[r]), bytes)
                    })
                })
                .collect()
        })
    }

    proptest! {
        /// Every candidate appears exactly once; it is eligible iff some
        /// tier found it eligible; eligible rows come first; a kept row is
        /// the candidate's last row; the would-reclaim rows are a prefix of
        /// the eligible ones that covers `needed` when the estimates do.
        #[test]
        fn a_dry_run_folds_to_one_verdict_per_candidate(plan in plans(), needed in 0u64..300) {
            let folded = dry_run(&plan, Some(needed));
            let candidates: BTreeSet<&Path> = plan.iter().map(|e| e.candidate.as_path()).collect();
            let rows: Vec<&Path> = folded.rows.iter().map(|r| r.entry.candidate.as_path()).collect();
            prop_assert_eq!(rows.len(), candidates.len());
            prop_assert_eq!(rows.iter().copied().collect::<BTreeSet<_>>(), candidates);
            let mut seen_kept = false;
            let mut seen_not_needed = false;
            let mut covered = 0u64;
            for row in &folded.rows {
                let path = row.entry.candidate.as_path();
                let ever_eligible = plan.iter().any(|e| e.candidate == path && e.reason.is_none());
                match row.verdict {
                    Verdict::Kept => {
                        seen_kept = true;
                        prop_assert!(!ever_eligible);
                        let last = plan.iter().rev().find(|e| e.candidate == path);
                        prop_assert_eq!(Some(row.entry), last);
                    }
                    Verdict::WouldReclaim => {
                        prop_assert!(!seen_kept && !seen_not_needed && ever_eligible);
                        prop_assert!(covered < needed);
                        covered += row.entry.estimated_bytes.unwrap_or(0);
                    }
                    Verdict::NotNeeded => {
                        prop_assert!(!seen_kept && ever_eligible);
                        prop_assert!(covered >= needed);
                        seen_not_needed = true;
                    }
                }
            }
        }

        /// A live run: a candidate any action reclaimed is never kept; every
        /// other candidate with a non-`empty` refusal is kept once, with its
        /// last refusal.
        #[test]
        fn a_live_run_folds_to_one_verdict_per_candidate(
            rows in proptest::collection::vec((0u8..3, 0u8..6, proptest::option::of(0usize..4)), 0..20)
        ) {
            let mut actions = Vec::new();
            let mut skipped = Vec::new();
            for (tier, candidate, reason) in rows {
                let entry = plan_entry(tier, candidate, None, 0);
                match reason {
                    None => actions.push(ActionEntry {
                        tier: entry.tier,
                        profile: entry.profile,
                        candidate: entry.candidate,
                        avail_delta_bytes: Some(1),
                        duration_ms: 0,
                        idle_age_seconds: None,
                        count: 1,
                    }),
                    Some(reason) => skipped.push(SkipEntry {
                        tier: entry.tier,
                        profile: entry.profile,
                        candidate: entry.candidate,
                        count: 1,
                        examples: Vec::new(),
                        reason: REASONS[reason],
                        detail: None,
                        process_blockers: None,
                        estimated_bytes: None,
                    }),
                }
            }
            let folded = live(&actions, &skipped);
            let reclaimed: BTreeSet<&Path> = folded.reclaimed.iter().map(|r| r.candidate).collect();
            prop_assert_eq!(reclaimed.len(), folded.reclaimed.len());
            prop_assert_eq!(
                reclaimed.clone(),
                actions.iter().map(|a| a.candidate.as_path()).collect::<BTreeSet<_>>()
            );
            let kept: Vec<&Path> = folded.kept.iter().map(|s| s.candidate.as_path()).collect();
            prop_assert_eq!(kept.iter().copied().collect::<BTreeSet<_>>().len(), kept.len());
            for skip in &skipped {
                let path = skip.candidate.as_path();
                let last = skipped.iter().rev().find(|s| s.candidate == path && s.reason != Reason::Empty);
                match last {
                    Some(last) if !reclaimed.contains(path) => {
                        prop_assert!(folded.kept.iter().any(|k| std::ptr::eq(*k, last)));
                    }
                    _ => prop_assert!(!kept.contains(&path)),
                }
            }
        }
    }
}
