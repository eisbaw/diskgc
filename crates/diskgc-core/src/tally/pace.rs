//! The background tally's pacing (docs/architecture.md, The tally): the
//! worker duty cycle and the pressure brake. Pure: the caller reads the
//! clock and the pressure through the `Host` port and sleeps what this
//! says.
//!
//! - **Duty cycle.** After a batch that took `t`, sleep `19 × t`: the
//!   worker is active about 5 % of wall-clock time whatever the device.
//! - **Pressure brake** (Mark, 2026-10-25). Before each batch the caller
//!   reads the pressure. Under pressure (`io` or `memory` `some avg10`
//!   above 10 %, `cpu` above 50 %; at a threshold is not above it) the batch
//!   is skipped and the sleep doubles: the first skip sleeps `19 × t` of the
//!   last batch, at least [`BRAKE_SLEEP_MIN`], each further one twice the
//!   one before, at most [`BRAKE_SLEEP_MAX`]. The first reading below every
//!   threshold runs a batch, after which the sleep is `19 × t` again.
//!   Without a reading (no PSI) only the duty cycle applies.

use std::time::Duration;

use crate::host::Pressure;

/// A batch ends after this much time ...
pub const BATCH_TIME_MAX: Duration = Duration::from_millis(50);
/// ... or this many filesystem operations, whichever comes first.
pub const BATCH_OPS_MAX: usize = 2_000;
/// Sleep this many times a batch's time after it: active 1 / (1 + 19) =
/// 5 % of the time.
pub const SLEEP_PER_WORK: u32 = 19;
/// The first sleep of the brake is at least this.
pub const BRAKE_SLEEP_MIN: Duration = Duration::from_secs(1);
/// The brake's sleep doubles up to this (tentative, docs/architecture.md).
pub const BRAKE_SLEEP_MAX: Duration = Duration::from_secs(60);
/// `io` and `memory` above this (hundredths of a percent: 10.00 %) is
/// pressure (tentative).
pub const IO_MEMORY_PRESSURE_MAX: u32 = 10_00;
/// `cpu` above this (50.00 %) is pressure: a busy laptop runs at load 5 to
/// 18, and `nice 19` already yields the CPU (tentative).
pub const CPU_PRESSURE_MAX: u32 = 50_00;

/// Whether `reading` is above any threshold.
pub fn under_pressure(reading: &Pressure) -> bool {
    reading.io > IO_MEMORY_PRESSURE_MAX
        || reading.memory > IO_MEMORY_PRESSURE_MAX
        || reading.cpu > CPU_PRESSURE_MAX
}

/// What to do before a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Run the batch.
    Run,
    /// Skip it and sleep this long (the brake).
    Skip(Duration),
}

/// The duty cycle and the brake's state between batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pace {
    /// How long the last batch took.
    last_batch: Duration,
    /// The brake's last sleep, while it is on.
    braking: Option<Duration>,
}

impl Pace {
    /// Before a batch, with the pressure `reading` (`None`: no PSI).
    pub fn before_batch(&mut self, reading: Option<&Pressure>) -> Gate {
        if !reading.is_some_and(under_pressure) {
            self.braking = None;
            return Gate::Run;
        }
        let sleep = match self.braking {
            None => duty_sleep(self.last_batch).max(BRAKE_SLEEP_MIN),
            Some(last) => last.saturating_mul(2),
        }
        .min(BRAKE_SLEEP_MAX);
        self.braking = Some(sleep);
        Gate::Skip(sleep)
    }

    /// After a batch that took `took`: the sleep before the next one.
    pub fn after_batch(&mut self, took: Duration) -> Duration {
        assert!(self.braking.is_none(), "a batch ran while the brake was on");
        self.last_batch = took;
        duty_sleep(took)
    }

    /// Whether the brake is on (the last gate skipped).
    pub fn braking(&self) -> bool {
        self.braking.is_some()
    }
}

/// `19 × took`.
fn duty_sleep(took: Duration) -> Duration {
    took.saturating_mul(SLEEP_PER_WORK)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psi(io: u32, memory: u32, cpu: u32) -> Pressure {
        Pressure { io, memory, cpu }
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn the_duty_cycle_sleeps_nineteen_times_the_batch() {
        let mut pace = Pace::default();
        assert_eq!(pace.before_batch(None), Gate::Run);
        assert_eq!(pace.after_batch(50 * MS), 950 * MS);
        assert_eq!(pace.before_batch(Some(&psi(0, 0, 0))), Gate::Run);
        assert_eq!(pace.after_batch(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn each_resource_alone_brakes_above_its_threshold_and_not_at_it() {
        for (above, at) in [
            (psi(10_01, 0, 0), psi(10_00, 0, 0)),
            (psi(0, 10_01, 0), psi(0, 10_00, 0)),
            (psi(0, 0, 50_01), psi(0, 0, 50_00)),
        ] {
            assert!(under_pressure(&above), "{above:?}");
            assert!(!under_pressure(&at), "{at:?}");
            let mut pace = Pace::default();
            assert!(matches!(pace.before_batch(Some(&above)), Gate::Skip(_)));
            let mut pace = Pace::default();
            assert_eq!(pace.before_batch(Some(&at)), Gate::Run);
        }
    }

    #[test]
    fn the_brake_doubles_from_the_duty_sleep_to_its_cap_and_resets() {
        let high = psi(30_00, 0, 0);
        let mut pace = Pace::default();
        pace.after_batch(10 * MS); // 19 x 10 ms = 190 ms: below the 1 s floor
        let mut sleeps = Vec::new();
        for _ in 0..9 {
            match pace.before_batch(Some(&high)) {
                Gate::Skip(s) => sleeps.push(s.as_secs()),
                Gate::Run => panic!("ran under pressure"),
            }
        }
        assert_eq!(sleeps, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(pace.before_batch(Some(&psi(5_00, 5_00, 40_00))), Gate::Run);
        assert!(!pace.braking());
        assert_eq!(pace.after_batch(20 * MS), 380 * MS);
        // A long batch starts the brake at its own duty sleep.
        pace.after_batch(100 * MS);
        assert_eq!(pace.before_batch(Some(&high)), Gate::Skip(1900 * MS));
        assert_eq!(pace.before_batch(Some(&high)), Gate::Skip(3800 * MS));
    }
}
