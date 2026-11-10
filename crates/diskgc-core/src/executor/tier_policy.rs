//! A tier's `protect` and `!profile:` matches, enumerated once for its
//! delete actions and enumerated again only when they may be stale
//! (TASK-200, TASK-268 review, TASK-285). Which changes make a check
//! enumerate again, why (the TASK-184 cost of enumerating per candidate),
//! and what the cached set cannot see are in docs/safety.md (Filesystem
//! candidates, Not protected, not excluded); when is
//! [`super::policy::refresh`]'s rule, and [`DeletePolicy::skip`] applies it
//! under the lock. Command actions run code that may touch anything in the
//! candidate, walked or not: they keep the full recheck
//! (`super::policy_skip`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::policy::{
    Acts, Check, NegatedMatches, enumerates_again, excluded_by_in, overlaps_checked_in,
    policy_failed, refresh, tagged_ancestor,
};
use super::{
    NegatedKind, NegatedSource, ProtectedKind, ProtectedSource, scan_negated, scan_protections,
};
use crate::engine::{Reason, Skip};
use crate::host::{Host, MonoTime, MountEntry, Timestamp};
use crate::outcome::{PolicyAt, Stop};
use crate::places;

/// How far before the enumeration a change's recorded time may lie and
/// still count as made after it began. A filesystem records a change at or
/// below the time it was made: the kernel's coarse clock lags by up to a
/// tick (1 to 10 ms), and a filesystem with whole-second (ext4 with
/// 128-byte inodes) or two-second (FAT) times truncates on top of that, so
/// up to 2 s plus a tick. Three seconds covers both with margin; a change
/// it wrongly counts as new only costs one enumeration.
pub(super) const TIMESTAMP_SLACK: Duration = Duration::from_secs(3);

/// One instant on both clocks: the wall clock the filesystem stamps changes
/// with, and the monotonic one (including suspend) that tells whether the
/// wall clock was stepped since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Instant {
    pub wall: Timestamp,
    pub mono: MonoTime,
}

impl Instant {
    pub fn now<H: Host>(host: &H) -> Self {
        Self {
            mono: host.now(),
            wall: host.wall_clock(),
        }
    }
}

/// The run's `protect` matches, when their enumeration began and the mount
/// table read just before it, or the latest table proven to answer their
/// enumeration alike ([`DeletePolicy::skip`]). The instant and the table
/// are the horizon of
/// the tier's cached matches, `protect`'s and the negated sources' (which
/// are enumerated after these): what changed since then may have changed a
/// verdict ([`DeletePolicy::skip`]).
#[derive(Debug, Clone)]
pub(super) struct Protection {
    pub matches: Vec<PathBuf>,
    pub enumerated: Instant,
    pub mounts: Vec<MountEntry>,
}

impl Protection {
    /// Enumerates the matches of `protections` now (an incomplete
    /// enumeration is an error, never a partial set: `scan_protections`).
    pub fn enumerate<H: Host>(
        host: &H,
        protections: &[ProtectedSource],
        deadline: Option<MonoTime>,
    ) -> Result<Self, String> {
        let mounts = host.mountinfo().map_err(|error| {
            format!("cannot read mountinfo before enumerating protected matches: {error}")
        })?;
        let enumerated = Instant::now(host);
        let matches = scan_protections(host, protections, deadline)?;
        Ok(Self {
            matches,
            enumerated,
            mounts,
        })
    }

    /// Takes `fresh`'s matches under this horizon: the tier's negated matches
    /// were enumerated after this one began, not after `fresh` did, so the
    /// cache stays as old as its oldest part. Changes made since cost an
    /// enumeration more, never a verdict on stale matches.
    pub fn refresh(&mut self, fresh: Protection) {
        self.matches = fresh.matches;
    }
}

/// The policy a delete action's candidates are judged against under their
/// locks and immediately before deletion: the tier's matches, enumerated
/// once, and the definitions to enumerate them again with. A check that
/// enumerates every one of them again keeps what it found, with its own
/// horizon ([`Protection`]), so one change costs one enumeration, not one
/// per later candidate (TASK-200's cost).
#[derive(Debug)]
pub(super) struct DeletePolicy<'p> {
    pub protections: &'p [ProtectedSource],
    pub protection: &'p mut Protection,
    /// The tier's negated sources, per source term (a union has one term
    /// per source), and one match list per source each, enumerated after
    /// `protection` began.
    pub negated: &'p [&'p [NegatedSource]],
    pub negated_matches: &'p mut [NegatedMatches],
    /// The term whose negated sources judge this candidate (`None`: none).
    pub term: Option<usize>,
}

impl DeletePolicy<'_> {
    /// `path` judged against the tier's matches, the mount table read once
    /// for the step: against matches enumerated now when one of them cannot
    /// be proven any more (a `cargo clean` of a protected target says
    /// nothing about this candidate), and whenever the cached matches may be
    /// stale: after the walk, a change it saw made since they were
    /// enumerated (or cannot tell); at any check, a mount table that may
    /// answer the enumeration otherwise than the one the matches were
    /// enumerated (or last proven) for ([`Self::mounts_unchanged`], TASK-300;
    /// TASK-285 and its review: a rename above a bind's source, a bind or
    /// overlay mounted at a protected root, which no time or identity of the
    /// candidate shows), or a `CACHEDIR.TAG` on its way down
    /// ([`tagged_ancestor`]). When that is decided is [`refresh`]'s rule
    /// ([`Check::UnderLock`]). Then every match is enumerated again and kept
    /// with the new horizon. Uncertain discovery or identity stops the run.
    pub fn skip<H: Host>(
        &mut self,
        host: &H,
        deadline: Option<MonoTime>,
        path: &Path,
        at: PolicyAt,
    ) -> Result<Option<Skip>, Stop> {
        assert_eq!(
            self.negated.len(),
            self.negated_matches.len(),
            "one match set per negated term"
        );
        let term = self.term.map_or(&[][..], |term| self.negated[term]);
        if self.protections.is_empty() && term.is_empty() {
            return Ok(None);
        }
        let started = Instant::now(host);
        let mounts = host.mountinfo().map_err(|error| {
            policy_failed(
                path,
                "protect overlap",
                format!("cannot read mountinfo: {error}"),
            )
        })?;
        // Staleness is read before the verdicts, in this order, as the
        // checks always have: the walk's times, the mount table, the tags.
        let changed = match at {
            PolicyAt::AfterWalk { newest_change } => {
                changed_since(self.protection.enumerated, started, newest_change)
            }
            PolicyAt::BeforeWalk => false,
        };
        let moved = !self.mounts_unchanged(&mounts);
        let stale = changed || moved || self.tag_on_the_way(host, &mounts, path);
        if !moved {
            // The matches answer for this table as for the one they were
            // proven for: it becomes their table, so the next check
            // compares against it (one change judged once, not again at
            // every later candidate).
            self.protection.mounts.clone_from(&mounts);
        }
        let protected =
            overlaps_checked_in(host, &mounts, path, &self.protection.matches, deadline);
        if matches!(protected, Ok(true)) {
            return Ok(Some(protected_skip(path)));
        }
        let excluded = match self.term {
            Some(index) => excluded_by_in(
                host,
                &mounts,
                term,
                &self.negated_matches[index],
                path,
                deadline,
            ),
            None => Ok(None),
        };
        if let Ok(Some(profile)) = &excluded {
            return Ok(Some(excluded_skip(path, profile)));
        }
        // Neither overlaps: an error of either, or the rule, sends the
        // check on to matches enumerated now.
        let cached: Result<bool, String> = match (&protected, &excluded) {
            (Ok(_), Ok(_)) => Ok(false),
            (Err(error), _) | (_, Err(error)) => Err(error.clone()),
        };
        let refresh = refresh(Check::UnderLock { stale }, Acts::OnWalkedEntries);
        if !enumerates_again(&cached, refresh) {
            return Ok(None);
        }
        if stale {
            self.enumerate_all(host, deadline, path, started, mounts)?;
        } else {
            // Only a match that cannot be proven any more: that list again,
            // under the old horizon (the others are as old as it is).
            if protected.is_err() {
                let fresh = Protection::enumerate(host, self.protections, deadline)
                    .map_err(|error| policy_failed(path, "protect overlap", error))?;
                self.protection.refresh(fresh);
            }
            if let (Err(_), Some(index)) = (&excluded, self.term) {
                self.negated_matches[index] = scan_negated(host, term, deadline)
                    .map_err(|error| policy_failed(path, "!profile overlap", error))?;
            }
        }
        // Judged on a table read after the enumeration: the fresh matches
        // are resolved on mounts it names.
        let after = host.mountinfo().map_err(|error| {
            policy_failed(
                path,
                "protect overlap",
                format!("cannot read mountinfo: {error}"),
            )
        })?;
        if overlaps_checked_in(host, &after, path, &self.protection.matches, deadline)
            .map_err(|error| policy_failed(path, "protect overlap", error))?
        {
            return Ok(Some(protected_skip(path)));
        }
        let Some(index) = self.term else {
            return Ok(None);
        };
        Ok(excluded_by_in(
            host,
            &after,
            term,
            &self.negated_matches[index],
            path,
            deadline,
        )
        .map_err(|error| policy_failed(path, "!profile overlap", error))?
        .map(|profile| excluded_skip(path, &profile)))
    }

    /// Every `protect` match and every term's negated matches enumerated
    /// again, kept with the horizon `started` and `mounts` (read before any
    /// of them began).
    fn enumerate_all<H: Host>(
        &mut self,
        host: &H,
        deadline: Option<MonoTime>,
        path: &Path,
        started: Instant,
        mounts: Vec<MountEntry>,
    ) -> Result<(), Stop> {
        let matches = scan_protections(host, self.protections, deadline)
            .map_err(|error| policy_failed(path, "protect recheck", error))?;
        let mut negated = Vec::with_capacity(self.negated.len());
        for sources in self.negated {
            negated.push(
                scan_negated(host, sources, deadline)
                    .map_err(|error| policy_failed(path, "!profile recheck", error))?,
            );
        }
        *self.protection = Protection {
            matches,
            enumerated: started,
            mounts,
        };
        for (kept, fresh) in self.negated_matches.iter_mut().zip(negated) {
            *kept = fresh;
        }
        Ok(())
    }

    /// Whether the mount table `mounts` answers every question the
    /// enumeration of the tier's matches asks of it as the table they were
    /// enumerated (or last proven) for does ([`places::unchanged_for`], from
    /// every `protect` root and every term's negated roots; docs/safety.md,
    /// Not protected, not excluded). Only the roots bound what is
    /// enumerated: `protect` and `!profile:` sources are bound without
    /// their `where` (no source anchor, no nested-build guard reads a path
    /// elsewhere), which this asserts.
    fn mounts_unchanged(&self, mounts: &[MountEntry]) -> bool {
        if self.protection.mounts == mounts {
            return true;
        }
        let rootless =
            |find: &crate::fs_kind::Find| find.source_anchor.is_none() && !find.nested_build_guard;
        for source in self.protections {
            if let ProtectedKind::Fs(find) = &source.kind {
                assert!(
                    rootless(find),
                    "protect `{}` is bound without its where",
                    source.profile
                );
            }
        }
        for source in self.negated.iter().flat_map(|term| term.iter()) {
            if let NegatedKind::Fs(find) = &source.source {
                assert!(
                    rootless(find),
                    "!profile:{} is bound without its where",
                    source.profile
                );
            }
        }
        let roots: Vec<&Path> = self
            .protections
            .iter()
            .flat_map(|source| &source.roots)
            .chain(
                self.negated
                    .iter()
                    .flat_map(|term| term.iter())
                    .flat_map(|source| &source.roots),
            )
            .map(PathBuf::as_path)
            .collect();
        places::unchanged_for(&self.protection.mounts, mounts, &roots)
    }

    /// [`tagged_ancestor`] for the Cargo sources of `protect` and of this
    /// term's `!profile:`s, against their enumerated matches.
    fn tag_on_the_way<H: Host>(&self, host: &H, mounts: &[MountEntry], path: &Path) -> bool {
        let term = self.term.map_or(&[][..], |term| self.negated[term]);
        let protected = self
            .protections
            .iter()
            .filter(|source| matches!(source.kind, ProtectedKind::CargoTarget))
            .flat_map(|source| &source.roots);
        let negated = term
            .iter()
            .filter(|source| matches!(source.source, NegatedKind::CargoTarget))
            .flat_map(|source| &source.roots);
        let term_matches = self.term.map(|term| &self.negated_matches[term]);
        let matched = |dir: &Path| {
            self.protection.matches.iter().any(|m| m == dir)
                || term_matches
                    .into_iter()
                    .flatten()
                    .flatten()
                    .any(|m| m == dir)
        };
        tagged_ancestor(host, mounts, protected.chain(negated), matched, path)
    }
}

fn protected_skip(path: &Path) -> Skip {
    Skip::new(
        Reason::Protected,
        path.to_owned(),
        "overlaps a protected match at final action check".to_owned(),
    )
}

fn excluded_skip(path: &Path, profile: &str) -> Skip {
    Skip::new(
        Reason::Excluded,
        path.to_owned(),
        format!("overlaps !profile:{profile} at final action check"),
    )
}

/// Whether a walk whose newest mtime or ctime is `newest` (`None`: some time
/// unknown) may have seen a change made at or after `since`, judged at
/// `now`. Every doubt answers `true` (enumerate again): an unknown time; a
/// time no more than [`TIMESTAMP_SLACK`] before `since`, or later; a wall
/// clock that moved more than [`TIMESTAMP_SLACK`] away from the monotonic
/// one since `since` (stepped, so a change's time says nothing about when
/// it was made relative to `since`); a monotonic clock that went back.
pub(super) fn changed_since(since: Instant, now: Instant, newest: Option<Timestamp>) -> bool {
    let Some(newest) = newest else {
        return true;
    };
    let slack = i128::try_from(TIMESTAMP_SLACK.as_nanos()).expect("two seconds fit");
    // A monotonic clock going back is a bug of the host: doubt.
    let Some(mono_elapsed) = now.mono.0.checked_sub(since.mono.0) else {
        return true;
    };
    let mono_elapsed =
        i128::try_from(mono_elapsed.as_nanos()).expect("a Duration's nanoseconds fit an i128");
    let wall_elapsed = nanos(now.wall) - nanos(since.wall);
    if (wall_elapsed - mono_elapsed).abs() > slack {
        return true;
    }
    nanos(newest) >= nanos(since.wall) - slack
}

fn nanos(t: Timestamp) -> i128 {
    i128::from(t.secs) * 1_000_000_000 + i128::from(t.nanos)
}

#[cfg(test)]
mod tests {
    use super::{Instant, TIMESTAMP_SLACK, changed_since, nanos};
    use crate::host::{MonoTime, Timestamp};
    use proptest::prelude::*;
    use std::time::Duration;

    const SINCE_SECS: i64 = 1_790_000_000;

    fn at(wall_ns: i128, mono_ns: u64) -> Instant {
        Instant {
            wall: timestamp(wall_ns),
            mono: MonoTime(Duration::from_nanos(mono_ns)),
        }
    }

    fn timestamp(ns: i128) -> Timestamp {
        Timestamp {
            secs: i64::try_from(ns.div_euclid(1_000_000_000)).expect("in range"),
            nanos: u32::try_from(ns.rem_euclid(1_000_000_000)).expect("below 1e9"),
        }
    }

    fn since() -> Instant {
        at(i128::from(SINCE_SECS) * 1_000_000_000, 3_600_000_000_000)
    }

    #[test]
    fn an_unknown_time_is_a_change() {
        assert!(changed_since(since(), since(), None));
    }

    #[test]
    fn a_change_inside_the_slack_before_the_enumeration_counts() {
        let s = since();
        let slack = i128::try_from(TIMESTAMP_SLACK.as_nanos()).unwrap();
        let start = nanos(s.wall);
        assert!(changed_since(s, s, Some(timestamp(start - slack))));
        assert!(!changed_since(s, s, Some(timestamp(start - slack - 1))));
    }

    #[test]
    fn a_stepped_wall_clock_is_doubted_whatever_the_time() {
        let s = since();
        let old = Some(timestamp(nanos(s.wall) - 86_400_000_000_000));
        // One hour on the monotonic clock, an hour minus 4 s on the wall
        // clock: stepped back by 4 s, more than the slack.
        let hour = 3_600_000_000_000_u64;
        let back = at(
            nanos(s.wall) + i128::from(hour) - 4_000_000_000,
            3_600_000_000_000 + hour,
        );
        assert!(changed_since(s, back, old));
        let even = at(nanos(s.wall) + i128::from(hour), 3_600_000_000_000 + hour);
        assert!(!changed_since(s, even, old));
    }

    #[test]
    fn a_monotonic_clock_going_back_is_doubted() {
        let s = since();
        let old = Some(timestamp(nanos(s.wall) - 86_400_000_000_000));
        assert!(changed_since(s, at(nanos(s.wall), 1), old));
    }

    proptest! {
        /// A change made at or after the enumeration began, recorded by a
        /// filesystem that truncates to `granularity` (at most the slack)
        /// and a clock lagging by up to a tick, is always seen, whenever
        /// the check runs, as long as the wall clock was not stepped.
        #[test]
        fn a_change_after_the_enumeration_is_always_seen(
            // Half the cases just after the enumeration began, where
            // truncation and the clock's lag matter most.
            made_after_ns in prop_oneof![0_i128..5_000_000, 0_i128..10_000_000_000_000],
            lag_ns in 0_i128..10_000_000,
            granularity_ns in prop::sample::select(vec![1_i128, 1_000_000_000, 2_000_000_000]),
            checked_after_ns in 0_u64..10_000_000_000_000,
            drift_ns in -1_000_000_000_i128..1_000_000_000,
            // Half the cases just after a 2 s granule's start.
            phase_ns in prop_oneof![0_i128..5_000_000, 0_i128..2_000_000_000],
        ) {
            // The enumeration may begin anywhere in a 2 s granule.
            let base = since();
            let s = at(nanos(base.wall) + phase_ns, 3_600_000_000_000);
            let made = nanos(s.wall) + made_after_ns;
            let recorded = (made - lag_ns).div_euclid(granularity_ns) * granularity_ns;
            let now = at(
                nanos(s.wall) + i128::from(checked_after_ns) + drift_ns,
                3_600_000_000_000 + checked_after_ns,
            );
            prop_assert!(changed_since(s, now, Some(timestamp(recorded))));
        }
    }
}
