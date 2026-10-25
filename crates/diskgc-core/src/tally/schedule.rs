//! The background tally's schedule (docs/architecture.md, The tally:
//! Schedule): three kinds of work, each with a period, and the next job is
//! the one most overdue relative to its period (lateness ÷ period), so none
//! starves the others on a busy `/tmp`. Pure: lateness from times the
//! caller reads.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::time::Duration;

use crate::host::MonoTime;

/// Discovery of each (profile, root): listing its patterns for new and
/// vanished candidates (tentative).
pub const DISCOVERY_PERIOD: Duration = Duration::from_secs(5 * 60);
/// A re-check of each unit: still there, same identity (tentative).
pub const RECHECK_PERIOD: Duration = Duration::from_secs(10 * 60);
/// A re-tally of each unit: its size and newest change (tentative).
pub const RETALLY_PERIOD: Duration = Duration::from_secs(60 * 60);

/// How late a job is, relative to its period. Ordered by `late ÷ period`
/// (compared exactly, by cross-multiplying), greater is more overdue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lateness {
    pub late: Duration,
    pub period: Duration,
}

impl Lateness {
    /// A job last done at `last` (`None`: never; then it is due from
    /// `since`, when it became known), with `period`, at `now`. `None` if
    /// it is not due yet.
    pub fn of(
        last: Option<MonoTime>,
        since: MonoTime,
        period: Duration,
        now: MonoTime,
    ) -> Option<Self> {
        assert!(!period.is_zero(), "a job without a period");
        let due = match last {
            Some(last) => last.checked_add(period).expect("monotonic time overflow"),
            None => since,
        };
        (now >= due).then(|| Self {
            late: now.saturating_since(due),
            period,
        })
    }

    fn ratio_cmp(&self, other: &Self) -> Ordering {
        let a = self.late.as_nanos() * other.period.as_nanos();
        let b = other.late.as_nanos() * self.period.as_nanos();
        a.cmp(&b)
    }
}

impl PartialOrd for Lateness {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Lateness {
    /// By ratio, then the longer period first (equal ratios: the rarer job,
    /// which has waited longer in absolute time).
    fn cmp(&self, other: &Self) -> Ordering {
        self.ratio_cmp(other)
            .then_with(|| self.period.cmp(&other.period))
    }
}

/// Keeps the `capacity` most overdue jobs offered, most overdue first; ties
/// keep the job offered first (the caller offers in a fixed order). A
/// bounded heap: `O(n log capacity)` over `n` offers.
#[derive(Debug)]
pub struct MostOverdue<J> {
    capacity: usize,
    /// A max-heap by [`Kept`]'s order, whose top is the job to drop.
    jobs: BinaryHeap<Kept<J>>,
    offered: u64,
}

/// A kept job, ordered least overdue last (then the later offer last).
#[derive(Debug)]
struct Kept<J> {
    lateness: Lateness,
    offer: u64,
    job: J,
}

impl<J> Kept<J> {
    fn key(&self) -> (Reverse<Lateness>, u64) {
        (Reverse(self.lateness), self.offer)
    }
}

impl<J> PartialEq for Kept<J> {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl<J> Eq for Kept<J> {}

impl<J> PartialOrd for Kept<J> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<J> Ord for Kept<J> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl<J> MostOverdue<J> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity >= 1, "keeping no job");
        Self {
            capacity,
            jobs: BinaryHeap::new(),
            offered: 0,
        }
    }

    pub fn offer(&mut self, lateness: Lateness, job: J) {
        self.offered += 1;
        self.jobs.push(Kept {
            lateness,
            offer: self.offered,
            job,
        });
        if self.jobs.len() > self.capacity {
            self.jobs.pop();
        }
    }

    /// The jobs kept, most overdue first.
    pub fn into_jobs(self) -> Vec<J> {
        self.jobs
            .into_sorted_vec()
            .into_iter()
            .map(|k| k.job)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> MonoTime {
        MonoTime(Duration::from_secs(secs))
    }

    #[test]
    fn a_job_is_due_a_period_after_it_was_done_or_from_when_it_was_known() {
        let p = Duration::from_secs(600);
        assert_eq!(Lateness::of(Some(at(100)), at(0), p, at(699)), None);
        assert_eq!(
            Lateness::of(Some(at(100)), at(0), p, at(700)),
            Some(Lateness {
                late: Duration::ZERO,
                period: p
            })
        );
        assert_eq!(
            Lateness::of(None, at(50), p, at(80)),
            Some(Lateness {
                late: Duration::from_secs(30),
                period: p
            })
        );
        assert_eq!(Lateness::of(None, at(50), p, at(49)), None);
    }

    #[test]
    fn lateness_compares_relative_to_the_period() {
        let l = |late: u64, period: u64| Lateness {
            late: Duration::from_secs(late),
            period: Duration::from_secs(period),
        };
        // 5 min late on a 5 min period beats 50 min late on a 60 min period.
        assert!(l(300, 300) > l(3000, 3600));
        // Equal ratios: the longer period first.
        assert!(l(600, 3600) > l(100, 600));
        assert_eq!(l(0, 300).cmp(&l(0, 300)), Ordering::Equal);
    }

    #[test]
    fn most_overdue_keeps_the_latest_in_order_and_breaks_ties_by_offer() {
        let l = |late: u64| Lateness {
            late: Duration::from_secs(late),
            period: Duration::from_secs(10),
        };
        let mut keep = MostOverdue::new(3);
        for (late, name) in [(1, "a"), (9, "b"), (5, "c"), (9, "d"), (0, "e"), (7, "f")] {
            keep.offer(l(late), name);
        }
        assert_eq!(keep.into_jobs(), ["b", "d", "f"]);
    }
}
