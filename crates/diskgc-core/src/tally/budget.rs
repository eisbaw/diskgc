//! How much one stretch of tally work may do: a background batch (at most
//! [`BATCH_TIME_MAX`] or [`BATCH_OPS_MAX`] filesystem operations), or the
//! synchronous tally (until its deadline). Checked before every operation,
//! so a batch ends at most one operation past its bound.

use std::time::Duration;

use super::pace::{BATCH_OPS_MAX, BATCH_TIME_MAX};
use crate::host::{Host, MonoTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    ops: usize,
    ops_max: usize,
    ends_at: Option<MonoTime>,
}

impl Budget {
    /// A background batch starting at `now`.
    pub fn batch(now: MonoTime) -> Self {
        Self {
            ops: 0,
            ops_max: BATCH_OPS_MAX,
            ends_at: Some(
                now.checked_add(BATCH_TIME_MAX)
                    .expect("monotonic time overflow"),
            ),
        }
    }

    /// The synchronous tally: no operation bound, until `deadline` (`None`:
    /// none, `--tally-budget 0`).
    pub fn until(deadline: Option<MonoTime>) -> Self {
        Self {
            ops: 0,
            ops_max: usize::MAX,
            ends_at: deadline,
        }
    }

    /// At most `ops_max` operations, no time bound (a walk cut into steps
    /// of a fixed size, e.g. by a test).
    pub fn ops_only(ops_max: usize) -> Self {
        Self {
            ops: 0,
            ops_max,
            ends_at: None,
        }
    }

    /// Whether the bound is reached: no operation may start.
    pub fn spent<H: Host>(&self, host: &H) -> bool {
        self.ops >= self.ops_max || self.ends_at.is_some_and(|end| host.now() >= end)
    }

    /// Counts `n` operations about to be made.
    pub fn take(&mut self, n: usize) {
        self.ops = self.ops.saturating_add(n);
    }

    /// Operations counted so far.
    pub fn ops(&self) -> usize {
        self.ops
    }

    /// Time left (`None`: no time bound).
    pub fn time_left<H: Host>(&self, host: &H) -> Option<Duration> {
        self.ends_at.map(|end| end.saturating_since(host.now()))
    }
}
