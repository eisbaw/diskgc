//! The synchronous tally (in `ensure`; docs/architecture.md, The tally:
//! Synchronous). It may load the machine and must be fast and useful: full
//! speed (no sleeps, normal priority, no pressure reading), for at most
//! the run's `--tally-budget`, before the host lock is taken (it is
//! read-only), over the tiers the run selects in their policy order, in two
//! passes:
//!
//! 1. **Breadth (discovery):** every selected tier's sources are listed
//!    ([`TallyState::list_source`]), but a source whose last complete
//!    listing (`discovered_at`, from the snapshot's seed) is younger than
//!    [`DISCOVERY_PERIOD`] is skipped. The complete listings of the pass
//!    are one round of evictions and recounts ([`super::recount`]).
//! 2. **Depth (sizing), first tier only** (Mark, 2026-10-25): the first
//!    selected tier with a discovered candidate (the one that will act
//!    first) has its stale singles sized, never sized or sized (tallied or
//!    refused) [`super::schedule::RETALLY_PERIOD`] or more ago ([`needs_sizing`]), in the tier's order by the stored keys
//!    (`oldest`: oldest first; `best-fit`: the stored estimate, largest
//!    first, unknown last). Later tiers keep what they have. Then the tally
//!    ends, even with budget left.
//!
//! **Useful** means pass 1 finished: every selected tier has a queue in
//! its order ([`SyncReport::pass1_done`]). Only the sources pass 1 listed
//! in this run without a stop count as discovered for the reclaim walk
//! ([`SyncReport::listed`]); a skipped one does not (docs/architecture.md,
//! The reclaim walk: Discovery is per tier).
//!
//! Blocks are not sized (their members have no names in the index); their
//! members are sized by the reclaim walk's checks when it expands them.

use std::path::PathBuf;

use super::budget::Budget;
use super::listing::TallySource;
use super::recount::{Complete, Recount};
use super::schedule::{DISCOVERY_PERIOD, needs_sizing};
use super::{Listed, Sizing, TallyState};
use crate::config::Order;
use crate::engine::Reason;
use crate::host::{Dev, Host, HostError, MonoTime};
use crate::index::queue::{self, AgeFilter, Queued, Source, TierQuery};
use crate::index::{Finder, MountKey};
use crate::outcome::Stop;

/// A selected tier, as the tally needs it: its sources (each a profile
/// bound to one root) and its order. In policy order (group, tier).
#[derive(Debug, Clone)]
pub struct SyncTier {
    pub sources: Vec<TallySource>,
    pub order: Order,
}

/// What the synchronous tally did.
#[derive(Debug, Default)]
pub struct SyncReport {
    /// Sources pass 1 listed in this run without a stop (refusals on the
    /// way included): these count as discovered for the reclaim walk.
    pub listed: Vec<Finder>,
    /// Sources pass 1 skipped: listed completely less than
    /// [`DISCOVERY_PERIOD`] before (by the snapshot's seed). Not
    /// discovered in this run.
    pub skipped_recent: Vec<Finder>,
    /// Pass 1 finished: the tally is useful.
    pub pass1_done: bool,
    /// The tier pass 2 sized (an index into the tiers), if any.
    pub first_tier: Option<usize>,
    /// Singles pass 2 sized.
    pub sized: usize,
    /// Stale singles of the first tier pass 2 did not size (the deadline).
    pub untallied: usize,
    /// Refusals this run's own tally found: skip the candidate for the
    /// run (docs/architecture.md, Refusals).
    pub refused: Vec<(PathBuf, Reason)>,
    /// What the listings did to the index, summed.
    pub inserted: usize,
    pub recount: Recount,
    /// Why it ended early: the deadline, a failure of the host.
    pub stopped: Option<Stop>,
    /// Filesystem operations of the sizing (pass 2), as counted.
    pub sizing_ops: usize,
}

/// Runs the synchronous tally over `tiers` (policy order) on the
/// filesystem `dev`, until `deadline` (`None`: to completion,
/// `--tally-budget 0`). An error is a failure of the host that stops it;
/// what it did until then stays in `state`.
pub fn synchronous<H: Host>(
    host: &H,
    state: &mut TallyState,
    tiers: &[SyncTier],
    dev: Dev,
    deadline: Option<MonoTime>,
) -> Result<SyncReport, HostError> {
    let mut report = SyncReport::default();
    let mut listings: Vec<(Finder, Listed)> = Vec::new();
    let pass1 = pass1(host, state, tiers, deadline, &mut report, &mut listings);
    let round: Vec<Complete<'_>> = listings
        .iter()
        .filter(|(_, l)| l.listing.complete)
        .map(|(finder, l)| Complete {
            finder,
            listed: &l.listing.listed,
            unindexed: &l.listing.unindexed,
        })
        .collect();
    report.recount = state.recount(&round);
    pass1?;
    if !report.pass1_done {
        return Ok(report);
    }
    pass2(host, state, tiers, dev, deadline, &mut report)?;
    Ok(report)
}

fn deadline_passed<H: Host>(host: &H, deadline: Option<MonoTime>) -> bool {
    deadline.is_some_and(|end| host.now() >= end)
}

fn pass1<H: Host>(
    host: &H,
    state: &mut TallyState,
    tiers: &[SyncTier],
    deadline: Option<MonoTime>,
    report: &mut SyncReport,
    listings: &mut Vec<(Finder, Listed)>,
) -> Result<(), HostError> {
    for source in tiers.iter().flat_map(|t| &t.sources) {
        if listings.iter().any(|(f, _)| *f == source.finder)
            || report.skipped_recent.contains(&source.finder)
        {
            continue; // a source two tiers share is listed once
        }
        let now = host.now();
        let recent = state
            .discovered_at(&source.finder)
            .is_some_and(|at| now.saturating_since(at) < DISCOVERY_PERIOD);
        if recent {
            report.skipped_recent.push(source.finder.clone());
            continue;
        }
        if deadline_passed(host, deadline) {
            report.stopped = Some(Stop::DeadlinePassed {
                path: source.finder.root.to_path_buf(),
            });
            return Ok(());
        }
        if host.interrupted().is_some() {
            report.stopped = Some(Stop::Interrupted {
                path: source.finder.root.to_path_buf(),
            });
            return Ok(());
        }
        let listed = state.list_source(host, source, deadline)?;
        report.inserted += listed.new + listed.replaced;
        if let Some(stop) = &listed.listing.stopped {
            report.stopped = Some(stop.clone());
            listings.push((source.finder.clone(), listed));
            return Ok(());
        }
        report.listed.push(source.finder.clone());
        listings.push((source.finder.clone(), listed));
    }
    report.pass1_done = true;
    Ok(())
}

fn pass2<H: Host>(
    host: &H,
    state: &mut TallyState,
    tiers: &[SyncTier],
    dev: Dev,
    deadline: Option<MonoTime>,
    report: &mut SyncReport,
) -> Result<(), HostError> {
    let Some((first, stale)) = first_tier_stale(host, state, tiers, dev) else {
        return Ok(());
    };
    report.first_tier = Some(first);
    report.untallied = stale.len();
    if stale.is_empty() {
        return Ok(());
    }
    let mounts = host.mountinfo()?;
    let mut budget = Budget::until(deadline);
    for (mount, path) in stale {
        // A signal ends the tally between units (a unit's walk is bounded
        // by the deadline, not checked for signals).
        if host.interrupted().is_some() {
            report.stopped = Some(Stop::Interrupted { path });
            break;
        }
        let sizing = loop {
            match state.size_unit(host, &mounts, &mount, &path, &mut budget)? {
                // Only the deadline pauses a walk without an operation bound.
                Sizing::Paused if !budget.spent(host) => continue,
                other => break other,
            }
        };
        match sizing {
            Sizing::Paused => {
                report.stopped = Some(Stop::DeadlinePassed { path });
                break;
            }
            Sizing::Done => report.sized += 1,
            Sizing::Refused(reason) => report.refused.push((path, reason)),
            Sizing::Gone | Sizing::Missing | Sizing::NoRoom => {}
        }
        report.untallied -= 1;
    }
    report.sizing_ops = budget.ops();
    Ok(())
}

/// The first tier with a discovered candidate, and its stale singles in the
/// tier's order by the stored keys.
fn first_tier_stale<H: Host>(
    host: &H,
    state: &TallyState,
    tiers: &[SyncTier],
    dev: Dev,
) -> Option<(usize, Vec<(MountKey, PathBuf)>)> {
    let now = host.now();
    tiers.iter().enumerate().find_map(|(i, tier)| {
        let sources: Vec<Source<'_>> = tier
            .sources
            .iter()
            .map(|s| Source {
                profile: &s.finder.profile,
                profile_hash: s.finder.profile_hash,
                root: &s.finder.root,
            })
            .collect();
        let q = queue::derive(
            &state.index,
            &TierQuery {
                dev,
                sources: &sources,
                age: AgeFilter::None,
            },
        );
        if q.ready.is_empty() && q.hinted.is_empty() {
            return None;
        }
        let mut entries: Vec<_> = q.ready.into_iter().chain(q.hinted).collect();
        if tier.order == Order::BestFit {
            // Largest stored estimate first, unknown last (stable: ties
            // keep the `oldest` order).
            entries.sort_by_key(|e| std::cmp::Reverse(e.estimate().map(|(bytes, _)| bytes)));
        }
        let stale = entries
            .into_iter()
            .filter_map(|e| match &e.unit {
                Queued::Single(s) => {
                    needs_sizing(s, now).then(|| (e.mount.clone(), e.path.clone()))
                }
                Queued::Block(_) => None,
            })
            .collect();
        Some((i, stale))
    })
}
