//! `watch`'s back-off (docs/reference.md, The reclaim cycle: Back-off):
//! after a cycle that ends below `--min-free` (every status but `met` and
//! `interrupted`), no cycle starts until a sample meets one of three
//! rules. Pure: the clock and the samples come in as values.
//!
//! - class `retry` when the cycle had an error of the host, of an
//!   operation or of provisioning, a `backend_unavailable`, `changed` or
//!   `in_use_unknown` skip, or ended `lock_timeout` or `deadline` (the
//!   environment may recover); `hold` otherwise (nothing changes until
//!   space drops further or files age);
//! - rule 1: available space at or below the cycle's final sample minus
//!   [`step`] (not applicable when that sample failed);
//! - rule 2: [`HOLD`] after the cycle ended;
//! - rule 3, `retry` only: [`RETRY_FIRST`] after the cycle ended, doubling
//!   with each consecutive `retry` cycle, capped by rule 2.
//!
//! A sample at or above `--min-free` ends the back-off and resets the
//! doubling; so does a `met` cycle (the caller drops the [`Backoff`]).
//! The constants are tentative (PRD, Irreversibility map); the classes and
//! the rules are the contract.

use std::time::Duration;

use crate::host::MonoTime;
use crate::report::json::Json;
use crate::report::{ErrorCause, Report, Status};
use crate::units::format_bytes;

/// Rule 2: the longest a back-off lasts.
pub const HOLD: Duration = Duration::from_secs(15 * 60);
/// Rule 3: the first `retry` wait.
pub const RETRY_FIRST: Duration = Duration::from_secs(60);
/// Rule 1's least step: 1 GiB (or 1 % of the filesystem, if more).
pub const STEP_MIN_BYTES: u64 = 1 << 30;

/// The skip reasons that make a cycle `retry`, by their JSON names
/// (`backend_unavailable`: kind `nix-store`, docs/reference.md, Backend
/// skips and errors).
pub const RETRY_SKIPS: [&str; 3] = ["backend_unavailable", "changed", "in_use_unknown"];

/// Why no cycle starts yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Retry,
    Hold,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Hold => "hold",
        }
    }
}

/// Rule 1's step for a filesystem of `size_bytes`: `max(1 GiB, 1 %)`.
pub fn step(size_bytes: u64) -> u64 {
    STEP_MIN_BYTES.max(size_bytes / 100)
}

/// The class of a cycle that ended below `--min-free`, and the causes that
/// chose it (for the log line), in the order found.
pub fn classify(report: &Report) -> (Class, Vec<String>) {
    let mut retry: Vec<String> = Vec::new();
    for error in &report.errors {
        if matches!(
            error.cause,
            ErrorCause::Host | ErrorCause::Operation | ErrorCause::Provisioning
        ) {
            push_once(&mut retry, format!("error ({})", error.cause.as_str()));
        }
    }
    for skip in &report.skipped {
        if RETRY_SKIPS.contains(&skip.reason.as_str()) {
            push_once(&mut retry, format!("skip {}", skip.reason.as_str()));
        }
    }
    if matches!(report.status, Status::LockTimeout | Status::Deadline) {
        push_once(&mut retry, format!("status {}", report.status.as_str()));
    }
    if !retry.is_empty() {
        return (Class::Retry, retry);
    }
    let mut hold = vec![format!("status {}", report.status.as_str())];
    for error in &report.errors {
        push_once(&mut hold, format!("error ({})", error.cause.as_str()));
    }
    (Class::Hold, hold)
}

fn push_once(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

/// Whether a cycle's status starts a back-off: every status but `met` and
/// `interrupted`.
pub fn backs_off(status: Status) -> bool {
    !matches!(status, Status::Met | Status::Interrupted(_))
}

/// A back-off in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    pub class: Class,
    /// Consecutive `retry` cycles (`0` for `hold`).
    pub attempt: u32,
    /// When the cycle that started it ended.
    pub ended: MonoTime,
    /// The cycle's final sample (`None`: it failed) and rule 1's step.
    pub final_avail: Option<u64>,
    pub step: u64,
    /// What chose the class, for the log line.
    pub causes: Vec<String>,
}

impl Backoff {
    /// The back-off after a cycle of `class` that ended at `ended` with the
    /// final sample `final_avail` (`None`: it failed), on a filesystem of
    /// `size_bytes`. `previous` is the back-off that cycle ran out of, if
    /// any: consecutive `retry` cycles double the wait.
    pub fn after(
        class: Class,
        causes: Vec<String>,
        ended: MonoTime,
        final_avail: Option<u64>,
        size_bytes: u64,
        previous: Option<&Backoff>,
    ) -> Self {
        let attempt = match class {
            Class::Hold => 0,
            Class::Retry => previous
                .filter(|b| b.class == Class::Retry)
                .map_or(1, |b| b.attempt.saturating_add(1)),
        };
        Self {
            class,
            attempt,
            ended,
            final_avail,
            step: step(size_bytes),
            causes,
        }
    }

    /// How long after [`Self::ended`] rules 2 and 3 next allow a cycle.
    pub fn wait(&self) -> Duration {
        match self.class {
            Class::Hold => HOLD,
            Class::Retry => {
                let doublings = self.attempt.saturating_sub(1).min(16);
                RETRY_FIRST.saturating_mul(1 << doublings).min(HOLD)
            }
        }
    }

    /// Rule 1's level: a sample at or below it starts a cycle. `None` when
    /// rule 1 cannot apply: the final sample failed, or it was below the
    /// step, so available space cannot fall by it (a full filesystem must
    /// not run a cycle at every interval).
    pub fn resume_below_bytes(&self) -> Option<u64> {
        self.final_avail?.checked_sub(self.step)
    }

    /// Whether a sample taken at `now` (`None`: it failed) below
    /// `--min-free` starts a cycle.
    pub fn allows(&self, now: MonoTime, avail: Option<u64>) -> bool {
        let dropped = matches!(
            (avail, self.resume_below_bytes()),
            (Some(a), Some(level)) if a <= level
        );
        dropped || now.saturating_since(self.ended) >= self.wait()
    }

    /// The record's `watch.backoff` object.
    pub fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("class", Json::str(self.class.as_str())),
            ("attempt", Json::U64(self.attempt.into())),
            ("wait_seconds", Json::U64(self.wait().as_secs())),
            (
                "resume_below_bytes",
                Json::opt(self.resume_below_bytes(), Json::U64),
            ),
        ])
    }

    /// The one log line of a back-off: its class, the causes that chose
    /// it, and both resume conditions.
    pub fn log_line(&self) -> String {
        let drop = match (self.final_avail, self.resume_below_bytes()) {
            (_, Some(level)) => format!("at a sample at or below {}", format_bytes(level)),
            (None, None) => "not on a drop (the final sample failed)".to_owned(),
            (Some(avail), None) => format!(
                "not on a drop (the {} available cannot fall by the step of {})",
                format_bytes(avail),
                format_bytes(self.step)
            ),
        };
        format!(
            "back-off {} (attempt {}; {}): the next cycle starts {drop}, or at the first sample \
             below --min-free after {} s",
            self.class.as_str(),
            self.attempt,
            self.causes.join(", "),
            self.wait().as_secs()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Reason;
    use crate::report::{ErrorEntry, SkipEntry};

    const GIB: u64 = 1 << 30;

    fn at(secs: u64) -> MonoTime {
        MonoTime(Duration::from_secs(secs))
    }

    fn report(status: Status) -> Report {
        Report::new(status, None, false)
    }

    fn skip(reason: Reason) -> SkipEntry {
        SkipEntry {
            tier: "t".into(),
            profile: "p".into(),
            candidate: None,
            count: 1,
            examples: Vec::new(),
            reason,
            detail: None,
            process_blockers: None,
            unknown_cause: None,
            estimated_bytes: None,
        }
    }

    #[test]
    fn classes_follow_the_documented_table() {
        assert_eq!(classify(&report(Status::Exhausted)).0, Class::Hold);
        assert_eq!(classify(&report(Status::LockTimeout)).0, Class::Retry);
        assert_eq!(classify(&report(Status::Deadline)).0, Class::Retry);
        for cause in [
            ErrorCause::Host,
            ErrorCause::Operation,
            ErrorCause::Provisioning,
        ] {
            let mut r = report(Status::Error);
            r.errors.push(ErrorEntry::run(cause, "x"));
            assert_eq!(classify(&r).0, Class::Retry, "{cause:?}");
        }
        for cause in [ErrorCause::Config, ErrorCause::Safety, ErrorCause::Usage] {
            let mut r = report(Status::Error);
            r.errors.push(ErrorEntry::run(cause, "x"));
            assert_eq!(classify(&r).0, Class::Hold, "{cause:?}");
        }
        for reason in [Reason::Changed, Reason::InUseUnknown] {
            let mut r = report(Status::Exhausted);
            r.skipped.push(skip(reason));
            assert_eq!(classify(&r).0, Class::Retry, "{reason:?}");
        }
        let mut r = report(Status::Exhausted);
        r.skipped.push(skip(Reason::NotIdle));
        assert_eq!(classify(&r).0, Class::Hold);
    }

    #[test]
    fn a_mixed_outcome_is_retry_and_names_its_causes() {
        let mut r = report(Status::Exhausted);
        r.skipped.push(skip(Reason::NotIdle));
        r.skipped.push(skip(Reason::InUseUnknown));
        r.skipped.push(skip(Reason::InUseUnknown));
        let (class, causes) = classify(&r);
        assert_eq!(class, Class::Retry);
        assert_eq!(causes, ["skip in_use_unknown"]);
    }

    #[test]
    fn the_retry_skips_are_reasons_or_reserved_ones() {
        for name in RETRY_SKIPS {
            let known = crate::engine::Reason::ALL
                .iter()
                .any(|r| r.as_str() == name)
                || crate::engine::RESERVED_REASONS
                    .iter()
                    .any(|(r, _)| *r == name);
            assert!(known, "{name} is no skip reason");
        }
    }

    #[test]
    fn met_and_interrupted_do_not_back_off() {
        assert!(!backs_off(Status::Met));
        assert!(!backs_off(Status::Interrupted(crate::report::Signal::Term)));
        for status in [
            Status::Exhausted,
            Status::Error,
            Status::LockTimeout,
            Status::Deadline,
            Status::UsageError,
        ] {
            assert!(backs_off(status), "{status:?}");
        }
    }

    #[test]
    fn retry_doubles_and_caps_at_15_minutes() {
        let mut previous: Option<Backoff> = None;
        let mut waits = Vec::new();
        for _ in 0..7 {
            let b = Backoff::after(
                Class::Retry,
                vec![],
                at(0),
                Some(GIB),
                100 * GIB,
                previous.as_ref(),
            );
            waits.push(b.wait().as_secs() / 60);
            previous = Some(b);
        }
        assert_eq!(waits, [1, 2, 4, 8, 15, 15, 15]);
        // A hold in between resets the doubling.
        let hold = Backoff::after(Class::Hold, vec![], at(0), None, 0, previous.as_ref());
        assert_eq!((hold.attempt, hold.wait()), (0, HOLD));
        let retry = Backoff::after(Class::Retry, vec![], at(0), None, 0, Some(&hold));
        assert_eq!(retry.attempt, 1);
    }

    #[test]
    fn rule_1_a_drop_by_the_step_resumes_at_once() {
        // 200 GiB filesystem: the step is 2 GiB (1 %), not 1 GiB.
        let b = Backoff::after(
            Class::Hold,
            vec![],
            at(100),
            Some(10 * GIB),
            200 * GIB,
            None,
        );
        assert_eq!(b.resume_below_bytes(), Some(8 * GIB));
        assert!(!b.allows(at(101), Some(8 * GIB + 1)));
        assert!(b.allows(at(101), Some(8 * GIB)));
        // Small filesystem: 1 GiB.
        let b = Backoff::after(Class::Hold, vec![], at(100), Some(3 * GIB), 10 * GIB, None);
        assert_eq!(b.resume_below_bytes(), Some(2 * GIB));
        // A failed final sample: rule 1 never applies, not even to 0.
        let b = Backoff::after(Class::Retry, vec![], at(100), None, 10 * GIB, None);
        assert_eq!(b.resume_below_bytes(), None);
        assert!(!b.allows(at(101), Some(0)));
        assert!(!b.allows(at(101), None));
        // A final sample below the step: available space cannot fall by it,
        // so rule 1 never applies, not even to an unchanged 0 (Codex review
        // of TASK-32: it used to saturate to a level of 0).
        let full = Backoff::after(Class::Hold, vec![], at(100), Some(0), 10 * GIB, None);
        assert_eq!(full.resume_below_bytes(), None);
        assert!(!full.allows(at(101), Some(0)));
        assert!(full.log_line().contains("cannot fall by the step"));
        assert!(
            full.to_json()
                .to_text()
                .contains("\"resume_below_bytes\":null")
        );
        let short = Backoff::after(Class::Hold, vec![], at(100), Some(GIB - 1), GIB, None);
        assert!(!short.allows(at(101), Some(0)));
        assert!(short.allows(at(100 + 900), Some(0)));
    }

    #[test]
    fn rules_2_and_3_are_timers_from_the_cycle_end() {
        let hold = Backoff::after(Class::Hold, vec![], at(100), Some(GIB), GIB, None);
        assert!(!hold.allows(at(100 + 899), None));
        assert!(hold.allows(at(100 + 900), None));
        let retry = Backoff::after(Class::Retry, vec![], at(100), Some(GIB), GIB, None);
        assert!(!retry.allows(at(159), Some(GIB)));
        assert!(retry.allows(at(160), Some(GIB)));
    }

    #[test]
    fn the_record_object_and_log_line_say_what_resumes_it() {
        let b = Backoff::after(
            Class::Retry,
            vec!["skip backend_unavailable".into()],
            at(0),
            Some(5 * GIB),
            10 * GIB,
            None,
        );
        assert_eq!(
            b.to_json().to_text(),
            format!(
                "{{\"class\":\"retry\",\"attempt\":1,\"wait_seconds\":60,\"resume_below_bytes\":{}}}",
                4 * GIB
            )
        );
        let line = b.log_line();
        assert!(line.contains("back-off retry"), "{line}");
        assert!(line.contains("skip backend_unavailable"), "{line}");
        assert!(line.contains("after 60 s"), "{line}");
        assert!(line.contains("at or below 4"), "{line}");
    }
}
