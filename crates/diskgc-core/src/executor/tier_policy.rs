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
//! What the cached set cannot see (docs/safety.md states it): a directory
//! outside the candidate, on its path, that becomes a match after the
//! enumeration (a new `CACHEDIR.TAG` making an ancestor a Cargo target), and
//! a change inside the candidate whose times were set back past the
//! enumeration by a wall-clock step that [`changed_since`] did not detect.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{NegatedSource, ProtectedSource, policy_skip, policy_verdict, scan_protections};
use crate::engine::Skip;
use crate::host::{Host, MonoTime, Timestamp};
use crate::outcome::{PolicyAt, Stop};

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

/// The run's `protect` matches and when their enumeration began.
#[derive(Debug, Clone)]
pub(super) struct Protection {
    pub matches: Vec<PathBuf>,
    pub enumerated: Instant,
}

impl Protection {
    /// Enumerates the matches of `protections` now (an incomplete
    /// enumeration is an error, never a partial set: `scan_protections`).
    pub fn enumerate<H: Host>(
        host: &H,
        protections: &[ProtectedSource],
        deadline: Option<MonoTime>,
    ) -> Result<Self, String> {
        let enumerated = Instant::now(host);
        let matches = scan_protections(host, protections, deadline)?;
        Ok(Self {
            matches,
            enumerated,
        })
    }
}

/// The policy a delete action's candidates are judged against under their
/// locks and immediately before deletion: the tier's matches, enumerated
/// once, and the definitions to enumerate them again with.
#[derive(Debug, Clone, Copy)]
pub(super) struct DeletePolicy<'p> {
    pub protections: &'p [ProtectedSource],
    pub protected: &'p [PathBuf],
    pub negated: &'p [NegatedSource],
    /// One list per `negated` source, enumerated after `enumerated`.
    pub negated_matches: &'p [Vec<PathBuf>],
    /// When the earliest of these enumerations began.
    pub enumerated: Instant,
}

impl DeletePolicy<'_> {
    /// `path` judged against the enumerated matches; after the walk, if it
    /// saw a change made since they were enumerated (or cannot tell), or
    /// when an enumerated match cannot be proven any more, against matches
    /// enumerated now (`policy_skip`). Uncertain discovery or identity
    /// there stops the run.
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
        if let PolicyAt::AfterWalk { newest_change } = at
            && changed_since(self.enumerated, Instant::now(host), newest_change)
        {
            return policy_skip(host, self.protections, self.negated, deadline, path);
        }
        // A match enumerated earlier whose identity cannot be proven now
        // (removed or moved since: a `cargo clean` of a protected target)
        // says nothing about this candidate: judge it against matches
        // enumerated now, as before TASK-200, rather than stop the run.
        policy_verdict(
            host,
            self.protected,
            self.negated,
            self.negated_matches,
            deadline,
            path,
        )
        .or_else(|_| policy_skip(host, self.protections, self.negated, deadline, path))
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
