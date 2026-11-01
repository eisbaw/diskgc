//! A tier's `protect` and `!profile:` matches, enumerated once for its
//! delete actions (TASK-200; docs/safety.md, Filesystem candidates:
//! protected and negated matches).
//!
//! Enumerating every protected and negated tree again for each candidate,
//! three times (before its lock, under it, and inside its freshness
//! window), cost a full discovery of those trees per candidate: with a
//! 61,000-directory protect, 2.2 to 3.3 s per candidate and clears expiring
//! before their deletion (`in_use_unknown`), against 5 to 33 ms without
//! (TASK-184 walk). A delete action unlinks only the entries its
//! eligibility walk listed under the candidate's lock, so a match the
//! enumeration missed can only matter if it was there when the walk ran:
//! inside the candidate, that is a change the walk sees as an mtime or ctime
//! at or after the enumeration began, and then the matches are enumerated
//! again, as before, for that candidate ([`DeletePolicy::skip`]). Command
//! actions run code that may touch anything in the candidate, walked or
//! not: they keep the full recheck (`super::policy_skip`).
//!
//! A directory outside the candidate, on its path, that becomes a Cargo
//! target after the enumeration (a new `CACHEDIR.TAG`) changes no time the
//! walk reads: a tag on the candidate's way down that is no enumerated match
//! makes the check enumerate again ([`tagged_ancestor`]).
//!
//! Nor does a rename above the source of a bind mount the candidate is
//! reached through (TASK-285): `/alias/cruft` keeps its path, identity and
//! times when `/src/work`, which `/alias` binds a directory of, becomes
//! `/src/keep-work`. The mount table shows it (the bind's root field
//! follows the rename), so the table read before the enumeration is kept,
//! and a candidate the table now shows differently ([`places::Ancestry`])
//! makes the check enumerate again.
//!
//! What the cached set cannot see (docs/safety.md states it): such a tag made
//! after the candidate's last check, and a change inside the candidate whose
//! times were set back past the enumeration by a wall-clock step that
//! [`changed_since`] did not detect.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::policy::{Refresh, judge_negated, judge_protect, policy_failed, tagged_ancestor};
use super::{NegatedKind, NegatedSource, ProtectedKind, ProtectedSource, scan_protections};
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
/// table read just before it. The instant and the table are the horizon of
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
/// once, and the definitions to enumerate them again with.
#[derive(Debug, Clone, Copy)]
pub(super) struct DeletePolicy<'p> {
    pub protections: &'p [ProtectedSource],
    pub protection: &'p Protection,
    pub negated: &'p [NegatedSource],
    /// One list per `negated` source, enumerated after `protection` was.
    pub negated_matches: &'p [Vec<PathBuf>],
}

impl DeletePolicy<'_> {
    /// `path` judged against the enumerated matches ([`judge_protect`],
    /// [`judge_negated`]): against matches enumerated now when one of them
    /// cannot be proven any more (a `cargo clean` of a protected target
    /// says nothing about this candidate), and, after the walk, whenever
    /// it saw a change made since they were enumerated (or cannot tell).
    /// Uncertain discovery or identity there stops the run. What it
    /// enumerates again is not kept: the reclaim's next first screen
    /// refreshes the tier's matches if it needs to.
    pub fn skip<H: Host>(
        &self,
        host: &H,
        deadline: Option<MonoTime>,
        path: &Path,
        at: PolicyAt,
    ) -> Result<Option<Skip>, Stop> {
        assert_eq!(
            self.negated.len(),
            self.negated_matches.len(),
            "one match list per negated source"
        );
        let changed = match at {
            PolicyAt::AfterWalk { newest_change } => changed_since(
                self.protection.enumerated,
                Instant::now(host),
                newest_change,
            ),
            _ => false,
        };
        let refresh = if changed || self.moved(host, path) || self.tag_on_the_way(host, path) {
            Refresh::Always
        } else {
            Refresh::OnError
        };
        let (protected, _) = judge_protect(
            host,
            self.protections,
            self.protection,
            path,
            deadline,
            refresh,
        )
        .map_err(|error| policy_failed(path, "protect overlap", error))?;
        if protected {
            return Ok(Some(Skip::new(
                Reason::Protected,
                path.to_owned(),
                "overlaps a protected match at final action check".to_owned(),
            )));
        }
        let (excluded, _) = judge_negated(
            host,
            self.negated,
            self.negated_matches,
            path,
            deadline,
            refresh,
        )
        .map_err(|error| policy_failed(path, "!profile overlap", error))?;
        Ok(excluded.map(|profile| {
            Skip::new(
                Reason::Excluded,
                path.to_owned(),
                format!("overlaps !profile:{profile} at final action check"),
            )
        }))
    }
}

impl DeletePolicy<'_> {
    /// Whether the mount table shows `path` differently now than before the
    /// enumeration (TASK-285): another place, or other mount points showing
    /// it or attached where they are ([`places::ancestry`]). A rename above
    /// a bind's source, a mount point's ancestor renamed, or a mount made
    /// or removed on the way changes what the matches say about it while
    /// its path, identity and times stay. A table that cannot be read, or
    /// a path either table cannot place, is a doubt: yes. Nothing to judge
    /// against (no `protect`, no negated source) asks nothing.
    fn moved<H: Host>(&self, host: &H, path: &Path) -> bool {
        if self.protections.is_empty() && self.negated.is_empty() {
            return false;
        }
        let Ok(now) = host.mountinfo() else {
            return true;
        };
        match (
            places::ancestry(&self.protection.mounts, path),
            places::ancestry(&now, path),
        ) {
            (Ok(then), Ok(now)) => then != now,
            _ => true,
        }
    }

    /// [`tagged_ancestor`] for the Cargo sources of `protect` and of this
    /// term's `!profile:`s, against their enumerated matches.
    fn tag_on_the_way<H: Host>(&self, host: &H, path: &Path) -> bool {
        let protected = self
            .protections
            .iter()
            .filter(|source| matches!(source.kind, ProtectedKind::CargoTarget))
            .flat_map(|source| &source.roots);
        let negated = self
            .negated
            .iter()
            .filter(|source| matches!(source.source, NegatedKind::CargoTarget))
            .flat_map(|source| &source.roots);
        let matched = |dir: &Path| {
            self.protection.matches.iter().any(|m| m == dir)
                || self.negated_matches.iter().flatten().any(|m| m == dir)
        };
        tagged_ancestor(host, protected.chain(negated), matched, path)
    }
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
