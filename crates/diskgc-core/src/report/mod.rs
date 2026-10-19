//! The result of an `ensure` (docs/reference.md, Exit codes, Output and
//! JSON): plain data, so the exit-code precedence, the JSON object and the
//! human summary are pure functions of it.
//!
//! Field names and enum values are part of the CLI contract; a test reads
//! them from docs/reference.md and compares ([`tests`]). What is derived
//! is not stored: `target_met` comes from the final sample and the target,
//! the exit code from the status.
//!
//! `index` is `null`: there is no index before task-28/29, every run
//! discovers its candidates afresh. `overruns` is always empty until
//! the deadline work of task-54 can report one, and `seed` is `null` (no
//! group, so no `random` ranking).

pub mod json;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use crate::engine::Checkouts;
use crate::units::format_bytes;
use json::Json;

/// How a run ended (`status` in the JSON).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The final sample of available space is at least `--min-free`.
    Met,
    /// Every eligible candidate was processed without errors, still below
    /// `--min-free`.
    Exhausted,
    /// A tier or candidate failed, or the run stopped on a host error,
    /// below `--min-free`.
    Error,
    /// A usage, configuration, provisioning or privilege error, or the
    /// pre-release sandbox gate (every exit-2 cause): nothing was deleted.
    UsageError,
    /// The deadline passed while waiting for another reclaim, and a fresh
    /// sample is still below `--min-free`.
    LockTimeout,
    /// The deadline passed during the reclaim, below `--min-free`.
    Deadline,
    /// Interrupted by a signal (task-54).
    Interrupted(Signal),
    /// `--dry-run`: planned, nothing deleted.
    DryRun,
}

/// The signal that interrupted a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Int,
    Term,
}

impl Status {
    /// Every status, one of each kind (both signals).
    pub const ALL: [Status; 9] = [
        Status::Met,
        Status::Exhausted,
        Status::Error,
        Status::UsageError,
        Status::LockTimeout,
        Status::Deadline,
        Status::Interrupted(Signal::Int),
        Status::Interrupted(Signal::Term),
        Status::DryRun,
    ];

    /// Its name in the JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Met => "met",
            Self::Exhausted => "exhausted",
            Self::Error => "error",
            Self::UsageError => "usage_error",
            Self::LockTimeout => "lock_timeout",
            Self::Deadline => "deadline",
            Self::Interrupted(_) => "interrupted",
            Self::DryRun => "dry_run",
        }
    }

    /// The exit code (docs/reference.md, Exit codes). Which status a run
    /// gets follows the table's order ("the first matching row wins"):
    /// usage errors before anything is measured, interruptions, a lock
    /// timeout before the reclaim, then [`conclude`] for a reclaim that
    /// ran.
    pub fn exit_code(self) -> u8 {
        match self {
            Self::UsageError => 2,
            Self::Interrupted(Signal::Int) => 130,
            Self::Interrupted(Signal::Term) => 143,
            Self::LockTimeout => 75,
            Self::Met | Self::DryRun => 0,
            Self::Deadline => 124,
            Self::Error => 3,
            Self::Exhausted => 1,
        }
    }
}

/// The final sample of available space a status is decided on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Final {
    /// At least `--min-free`.
    Met,
    /// Below `--min-free`.
    Below,
    /// The sample could not be taken: nothing is known about the space.
    Unknown,
}

impl Final {
    /// The sample `avail` (`None`: it failed) against `min`.
    pub fn of(avail: Option<u64>, min: u64) -> Self {
        match avail {
            Some(a) if a >= min => Self::Met,
            Some(_) => Self::Below,
            None => Self::Unknown,
        }
    }
}

/// The status of a reclaim that ran (the host lock taken, not interrupted):
/// the exit-code table's rows 0, 124, 3 and 1, the first that matches.
/// `last`: the final sample; `deadline`: the deadline stopped the reclaim;
/// `errors`: a tier, candidate or the run failed. A sample that could not
/// be taken is a run error (3) whatever else happened: 124 (like 75, see
/// [`after_lock_timeout`]) needs a fresh sample below `--min-free`.
pub fn conclude(last: Final, deadline: bool, errors: bool) -> Status {
    match last {
        Final::Met => Status::Met,
        Final::Unknown => Status::Error,
        Final::Below if deadline => Status::Deadline,
        Final::Below if errors => Status::Error,
        Final::Below => Status::Exhausted,
    }
}

/// The status when the deadline passed waiting for the host lock, from the
/// fresh sample taken then: 0 if another reclaim freed enough, 75 if it is
/// still below `--min-free`, 3 if the sample could not be taken.
pub fn after_lock_timeout(last: Final) -> Status {
    match last {
        Final::Met => Status::Met,
        Final::Below => Status::LockTimeout,
        Final::Unknown => Status::Error,
    }
}

/// A signed change in bytes for people (`+4.5 MiB`), `unknown` when it was
/// not measured.
pub fn signed_bytes(delta: Option<i64>) -> String {
    match delta {
        None => "unknown".to_owned(),
        Some(d) => format!(
            "{}{}",
            if d < 0 { "-" } else { "+" },
            format_bytes(d.unsigned_abs())
        ),
    }
}

/// One candidate of a `--dry-run` plan (`plan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub group: String,
    pub tier: String,
    pub profile: String,
    pub candidate: PathBuf,
    /// The skip reason, `None` for an eligible candidate.
    pub reason: Option<&'static str>,
    /// The candidate's estimated size; `None` for every skipped candidate
    /// (its checks stopped before its walk was complete, so a size would
    /// be a partial count).
    pub estimated_bytes: Option<u64>,
    pub operation: String,
    /// The source profile's `checkouts` setting the candidate was judged
    /// under (docs/safety.md, No source checkout).
    pub checkouts: Checkouts,
}

/// One action (`actions`): a candidate deleted, completely or part-way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionEntry {
    pub tier: String,
    pub profile: String,
    pub candidate: PathBuf,
    /// The observed change in available space around the action (signed:
    /// concurrent writers can make it negative); `None` when a sample
    /// before or after it failed: never a made-up 0.
    pub avail_delta_bytes: Option<i64>,
    pub duration_ms: u64,
    pub count: u64,
}

/// One skipped candidate (`skipped`). Not aggregated yet (one entry per
/// candidate, `count` 1, no `examples`; task-19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipEntry {
    pub tier: String,
    pub profile: String,
    pub candidate: PathBuf,
    pub count: u64,
    pub examples: Vec<PathBuf>,
    pub reason: &'static str,
}

/// A process holding deleted files on the requested filesystem
/// (`held_by_deleted_files`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldEntry {
    pub pid: u32,
    pub command: String,
    pub bytes: u64,
}

/// An operation that blocked past the deadline (`overruns`; task-54).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrunEntry {
    pub operation: String,
    pub path: PathBuf,
    pub over_ms: u64,
}

/// An error (`errors`). `tier`, `profile` and `candidate` are empty where
/// they do not apply (a usage error, an error of the whole run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorEntry {
    pub tier: String,
    pub profile: String,
    pub candidate: String,
    pub message: String,
}

/// The report of one `ensure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub status: Status,
    /// The mount point of `path`'s filesystem; `None` before it was
    /// resolved.
    pub mount: Option<PathBuf>,
    /// `PATH` as given; `None` when the command line was not understood.
    pub path: Option<PathBuf>,
    pub min_free_bytes: Option<u64>,
    pub target_free_bytes: Option<u64>,
    pub avail_before_bytes: Option<u64>,
    pub avail_after_bytes: Option<u64>,
    pub plan: Vec<PlanEntry>,
    pub actions: Vec<ActionEntry>,
    pub skipped: Vec<SkipEntry>,
    pub held_by_deleted_files: Vec<HeldEntry>,
    pub overruns: Vec<OverrunEntry>,
    pub errors: Vec<ErrorEntry>,
}

impl Report {
    /// An empty report of `status` for `path`.
    pub fn new(status: Status, path: Option<&Path>) -> Self {
        Self {
            status,
            mount: None,
            path: path.map(Path::to_owned),
            min_free_bytes: None,
            target_free_bytes: None,
            avail_before_bytes: None,
            avail_after_bytes: None,
            plan: Vec::new(),
            actions: Vec::new(),
            skipped: Vec::new(),
            held_by_deleted_files: Vec::new(),
            overruns: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// A usage error (exit 2) saying `message`; the fields determined so
    /// far are kept.
    pub fn usage_error(mut self, message: impl Into<String>) -> Self {
        self.status = Status::UsageError;
        self.errors.push(ErrorEntry {
            tier: String::new(),
            profile: String::new(),
            candidate: String::new(),
            message: message.into(),
        });
        self
    }

    /// Whether the final sample reached `--target-free`; `None` when either
    /// is not known (a usage error).
    pub fn target_met(&self) -> Option<bool> {
        Some(self.avail_after_bytes? >= self.target_free_bytes?)
    }

    pub fn exit_code(&self) -> u8 {
        self.status.exit_code()
    }

    /// The JSON object (docs/reference.md, Output), one line, no newline.
    pub fn to_json(&self) -> String {
        let path = |p: &Path| Json::str(p.to_string_lossy());
        let opt_u64 = |v: Option<u64>| Json::opt(v, Json::U64);
        Json::Obj(vec![
            ("status", Json::str(self.status.as_str())),
            ("mount", Json::opt(self.mount.as_deref(), path)),
            ("path", Json::opt(self.path.as_deref(), path)),
            ("seed", Json::Null),
            // No index exists yet (task-28/29): every run discovers afresh.
            ("index", Json::Null),
            ("min_free_bytes", opt_u64(self.min_free_bytes)),
            ("target_free_bytes", opt_u64(self.target_free_bytes)),
            ("target_met", Json::opt(self.target_met(), Json::Bool)),
            ("avail_before_bytes", opt_u64(self.avail_before_bytes)),
            ("avail_after_bytes", opt_u64(self.avail_after_bytes)),
            (
                "plan",
                Json::Arr(
                    self.plan
                        .iter()
                        .map(|p| {
                            Json::Obj(vec![
                                ("group", Json::str(&p.group)),
                                ("tier", Json::str(&p.tier)),
                                ("profile", Json::str(&p.profile)),
                                ("candidate", path(&p.candidate)),
                                ("eligible", Json::Bool(p.reason.is_none())),
                                ("reason", Json::opt(p.reason, Json::str)),
                                ("estimated_bytes", opt_u64(p.estimated_bytes)),
                                ("operation", Json::str(&p.operation)),
                                ("checkouts", Json::str(p.checkouts.as_str())),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "actions",
                Json::Arr(
                    self.actions
                        .iter()
                        .map(|a| {
                            Json::Obj(vec![
                                ("tier", Json::str(&a.tier)),
                                ("profile", Json::str(&a.profile)),
                                ("candidate", path(&a.candidate)),
                                (
                                    "avail_delta_bytes",
                                    Json::opt(a.avail_delta_bytes, Json::I64),
                                ),
                                ("duration_ms", Json::U64(a.duration_ms)),
                                ("count", Json::U64(a.count)),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "skipped",
                Json::Arr(
                    self.skipped
                        .iter()
                        .map(|s| {
                            Json::Obj(vec![
                                ("tier", Json::str(&s.tier)),
                                ("profile", Json::str(&s.profile)),
                                ("candidate", path(&s.candidate)),
                                ("count", Json::U64(s.count)),
                                (
                                    "examples",
                                    Json::Arr(s.examples.iter().map(|e| path(e)).collect()),
                                ),
                                ("reason", Json::str(s.reason)),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "held_by_deleted_files",
                Json::Arr(
                    self.held_by_deleted_files
                        .iter()
                        .map(|h| {
                            Json::Obj(vec![
                                ("pid", Json::U64(u64::from(h.pid))),
                                ("command", Json::str(&h.command)),
                                ("bytes", Json::U64(h.bytes)),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "overruns",
                Json::Arr(
                    self.overruns
                        .iter()
                        .map(|o| {
                            Json::Obj(vec![
                                ("operation", Json::str(&o.operation)),
                                ("path", path(&o.path)),
                                ("over_ms", Json::U64(o.over_ms)),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "errors",
                Json::Arr(
                    self.errors
                        .iter()
                        .map(|e| {
                            Json::Obj(vec![
                                ("tier", Json::str(&e.tier)),
                                ("profile", Json::str(&e.profile)),
                                ("candidate", Json::str(&e.candidate)),
                                ("message", Json::str(&e.message)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
        .to_text()
    }

    /// The human-mode result for stdout (docs/reference.md, Output):
    /// `None` when nothing needed doing (the goal was met and nothing was
    /// deleted, like `cp`'s silence); a dry run always prints its plan;
    /// otherwise a short summary. Errors are summarised here and logged in
    /// full on stderr as they happen.
    pub fn summary(&self) -> Option<String> {
        let mount = self
            .mount
            .as_deref()
            .map_or_else(|| "unknown".to_owned(), |m| m.display().to_string());
        let space = |v: Option<u64>| v.map_or_else(|| "unknown".to_owned(), format_bytes);
        let header = format!(
            "{}: {} available on {mount} (min-free {}, target-free {})",
            self.status.as_str(),
            space(self.avail_after_bytes),
            space(self.min_free_bytes),
            space(self.target_free_bytes)
        );
        match self.status {
            Status::Met if self.actions.is_empty() && self.errors.is_empty() => None,
            Status::UsageError => None,
            Status::DryRun => {
                let mut lines = vec![format!(
                    "dry run: {} available on {mount} (min-free {}, target-free {}); {} \
                     candidate(s), in the order ensure would take them:",
                    space(self.avail_before_bytes),
                    space(self.min_free_bytes),
                    space(self.target_free_bytes),
                    self.plan.len()
                )];
                for p in &self.plan {
                    let verdict = match p.reason {
                        None => format!(
                            "eligible  {:>10}",
                            p.estimated_bytes
                                .map_or_else(|| "unknown".to_owned(), format_bytes)
                        ),
                        Some(reason) => format!("skip {reason}"),
                    };
                    lines.push(format!(
                        "  {verdict}  {}  [{} {}: {}]",
                        p.candidate.display(),
                        p.tier,
                        p.profile,
                        p.operation
                    ));
                }
                if !self.errors.is_empty() {
                    lines.push(format!("  {} error(s): see stderr", self.errors.len()));
                }
                Some(lines.join("\n"))
            }
            _ => {
                // Unknown if any action's change is unknown.
                let freed: Option<i64> = self.actions.iter().map(|a| a.avail_delta_bytes).sum();
                let mut text = format!(
                    "{header}; {} action(s) (observed change {}), {} skipped, {} error(s)",
                    self.actions.len(),
                    signed_bytes(freed),
                    self.skipped.len(),
                    self.errors.len()
                );
                if let Some(e) = self.errors.first() {
                    text.push_str(&format!("; first error: {}", e.message));
                }
                Some(text)
            }
        }
    }
}
