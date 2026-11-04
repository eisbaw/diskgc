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
//! discovers its candidates afresh. `overruns` records operations that
//! returned after the deadline. `seed` is `null` unless a configured group
//! uses random ranking.

mod human;
pub mod json;
pub mod outcome;
pub mod summary;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::engine::{Checkouts, ProcessBlocker, Reason, UnknownCause};
use crate::units::{Size, format_bytes};
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
    /// Handled by `watch` only (docs/reference.md, Watch signals), which
    /// exits 0 on it; `ensure` installs no handler for it.
    Hup,
}

impl Signal {
    pub fn name(self) -> &'static str {
        match self {
            Self::Int => "SIGINT",
            Self::Term => "SIGTERM",
            Self::Hup => "SIGHUP",
        }
    }
}

/// The status of a refusal (`ensure`'s and `watch`'s start, a reclaim
/// that cannot start): `error` (exit 3) when the host failed (`host`: a
/// read of diskgc's own capability set or PID namespace facts, a lock
/// that failed), `usage_error` (exit 2) for every other cause (usage,
/// configuration, `safety`, `provisioning`).
pub fn refusal_status(cause: ErrorCause) -> Status {
    if cause == ErrorCause::Host {
        Status::Error
    } else {
        Status::UsageError
    }
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
            // Never an exit code: only `watch` handles SIGHUP, and it
            // exits 0. 128 + 1 for completeness.
            Self::Interrupted(Signal::Hup) => 129,
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

/// Keep two different byte counts distinguishable when the usual rounded
/// display would print them identically (notably `available` vs `min-free`).
pub(crate) fn human_space_values(values: [Option<u64>; 3]) -> [String; 3] {
    let rounded = values.map(|value| value.map_or_else(|| "unknown".to_owned(), format_bytes));
    std::array::from_fn(|index| match values[index] {
        Some(bytes)
            if values.iter().enumerate().any(|(other_index, other)| {
                other_index != index
                    && other.is_some_and(|other_bytes| {
                        other_bytes != bytes && rounded[other_index] == rounded[index]
                    })
            }) =>
        {
            format!("{} ({bytes} B)", rounded[index])
        }
        _ => rounded[index].clone(),
    })
}

/// One candidate of a `--dry-run` plan (`plan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub group: String,
    pub tier: String,
    pub profile: String,
    /// `None` for a row of the tier as a whole: its profile was refused
    /// before discovery (a missing executable, an unavailable backend).
    pub candidate: Option<PathBuf>,
    /// The skip reason, `None` for an eligible candidate.
    pub reason: Option<Reason>,
    /// The candidate's estimated size: an eligible one's, or a skipped
    /// one's whose walk was complete before the refusal
    /// ([`crate::engine::Skip::measured_bytes`]); `None` when unknown, and
    /// for a skip before or during the walk (a size would be a partial
    /// count).
    pub estimated_bytes: Option<u64>,
    pub operation: String,
    /// The source profile's `checkouts` setting the candidate was judged
    /// under (docs/safety.md, No source checkout).
    pub checkouts: Checkouts,
    /// Only for a build output whose recorded source was verified (the
    /// JSON then has `null`).
    pub build_evidence: Option<BuildEvidence>,
    /// The skip's human diagnostic and attributed processes, as in
    /// [`SkipEntry`], for the human plan only: the JSON plan has neither
    /// (programs branch on `reason`).
    pub detail: Option<String>,
    pub process_blockers: Option<Vec<ProcessBlocker>>,
    /// The skip's `unknown_cause`, for the human plan only.
    pub unknown_cause: Option<UnknownCause>,
}

/// A source term a dry run's planner judged (the `summary`'s blocks): its
/// tier and profile, and how many of the candidates it found the plan left
/// unjudged because it stopped first (the deadline, a failure). Kept apart
/// from `plan`, which holds only judged rows (TASK-227).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTerm {
    pub tier: String,
    pub profile: String,
    pub not_judged: u64,
}

/// Source evidence from discovery, with age only after an eligibility walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildEvidence {
    pub marker: &'static str,
    pub source: PathBuf,
    pub idle_age_seconds: Option<u64>,
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
    /// Age of the candidate's newest change at its completed tally.
    pub idle_age_seconds: Option<u64>,
    pub count: u64,
}

/// The `detail` of a root skipped as `other_fs` (a `skipped` row, or a
/// dry run's `plan` row): it is a root of the tier, not a candidate, and
/// nothing below it was looked at (TASK-210). The human summary names
/// every tier of such a row by it.
pub const OTHER_FS_ROOT_DETAIL: &str =
    "a root of the tier, on another filesystem than PATH's: not searched";

/// One skipped candidate (`skipped`). Groups with more than 20 candidates
/// are aggregated in the JSON result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipEntry {
    pub tier: String,
    pub profile: String,
    /// `None` when the tier as a whole was skipped (its profile refused
    /// before it had a candidate) or, in the JSON, for an aggregated row.
    pub candidate: Option<PathBuf>,
    pub count: u64,
    pub examples: Vec<PathBuf>,
    pub reason: Reason,
    /// Human diagnostic from the refusal, when one was available.
    pub detail: Option<String>,
    /// Attributed scan-time process evidence, or `None` when attribution is
    /// unavailable or this is an aggregated row.
    pub process_blockers: Option<Vec<ProcessBlocker>>,
    /// Why an `in_use_unknown` could not be judged (JSON `unknown_cause`);
    /// `None` for every other reason.
    pub unknown_cause: Option<UnknownCause>,
    /// A complete tally made before this refusal, when available.
    pub estimated_bytes: Option<u64>,
}

fn process_blocker_json(blocker: &ProcessBlocker) -> Json {
    Json::Obj(vec![
        ("pid", Json::U64(u64::from(blocker.pid))),
        ("command", Json::opt(blocker.command.as_deref(), Json::str)),
        ("evidence", Json::str(blocker.evidence.as_str())),
    ])
}

/// One `summary` block (docs/reference.md, Output).
fn term_json(term: &summary::Term<'_>) -> Json {
    let sized = |sized: summary::Sized| {
        Json::Obj(vec![
            ("count", Json::U64(sized.count)),
            ("estimated_bytes", Json::U64(sized.bytes)),
            ("unsized", Json::U64(sized.unmeasured)),
        ])
    };
    let planned = term.planned;
    Json::Obj(vec![
        ("tier", Json::str(term.tier)),
        ("profile", Json::str(term.profile)),
        ("found", Json::opt(term.found(), Json::U64)),
        ("eligible", Json::opt(planned.map(|p| p.eligible), sized)),
        (
            "would_reclaim",
            Json::opt(planned.map(|p| p.would_reclaim), sized),
        ),
        (
            "reclaimed",
            Json::opt(term.reclaimed, |reclaimed| {
                Json::Obj(vec![
                    ("count", Json::U64(reclaimed.count)),
                    (
                        "avail_delta_bytes",
                        Json::opt(reclaimed.avail_delta_bytes, Json::I64),
                    ),
                ])
            }),
        ),
        (
            "kept",
            Json::Arr(
                term.kept
                    .iter()
                    .map(|(reason, kept)| {
                        Json::Obj(vec![
                            ("reason", Json::str(reason.as_str())),
                            ("count", Json::U64(kept.count)),
                            ("estimated_bytes", Json::U64(kept.bytes)),
                            ("unsized", Json::U64(kept.unmeasured)),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "not_judged",
            Json::opt(planned.and_then(|p| p.not_judged), Json::U64),
        ),
    ])
}

/// A process holding deleted files on the requested filesystem
/// (`held_by_deleted_files`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldEntry {
    pub pid: u32,
    /// `None` when the process's command could not be read.
    pub command: Option<String>,
    pub bytes: u64,
}

/// The actions past the first 1,000, summed into one JSON row
/// ([`Report::output_actions`]).
struct ActionOverflow {
    avail_delta_bytes: Option<i64>,
    duration_ms: u64,
    count: u64,
}

/// An operation that blocked past the deadline (`overruns`; task-54).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrunEntry {
    pub operation: String,
    pub path: PathBuf,
    pub over_ms: u64,
}

/// An error (`errors`). `tier`, `profile` and `candidate` are `None` where
/// they do not apply (a usage error, an error of the whole run): `null` in
/// the JSON, never an empty string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorEntry {
    pub tier: Option<String>,
    pub profile: Option<String>,
    pub candidate: Option<String>,
    pub cause: ErrorCause,
    pub message: String,
}

impl ErrorEntry {
    /// An error of the whole run or command: no tier, profile or candidate.
    pub fn run(cause: ErrorCause, message: impl Into<String>) -> Self {
        Self {
            tier: None,
            profile: None,
            candidate: None,
            cause,
            message: message.into(),
        }
    }

    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("tier", Json::opt(self.tier.as_deref(), Json::str)),
            ("profile", Json::opt(self.profile.as_deref(), Json::str)),
            ("candidate", Json::opt(self.candidate.as_deref(), Json::str)),
            ("cause", Json::str(self.cause.as_str())),
            ("message", Json::str(&self.message)),
        ])
    }
}

/// The JSON object of a command other than `ensure` that failed
/// (docs/reference.md, Output, Errors): the envelope `ensure`'s report
/// opens with (`status`, `exit_code`) and its `errors` list, so a script
/// reads every command's failure the same way.
pub fn error_json(status: Status, errors: &[ErrorEntry]) -> Json {
    assert!(
        !errors.is_empty(),
        "a failed command reports at least one error"
    );
    Json::Obj(vec![
        ("status", Json::str(status.as_str())),
        ("exit_code", Json::U64(status.exit_code().into())),
        (
            "errors",
            Json::Arr(errors.iter().map(ErrorEntry::to_json).collect()),
        ),
    ])
}

/// Stable machine-readable reason for a reported error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCause {
    Parse,
    Config,
    Safety,
    Host,
    Operation,
    Usage,
    /// The host lacks what its installation must provide (the host lock
    /// file): an operator's fix, not a caller's (TASK-151). A harness alerts
    /// ops instead of retrying or rewriting its call.
    Provisioning,
}

impl ErrorCause {
    pub const ALL: [Self; 7] = [
        Self::Parse,
        Self::Config,
        Self::Safety,
        Self::Host,
        Self::Operation,
        Self::Usage,
        Self::Provisioning,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Config => "config",
            Self::Safety => "safety",
            Self::Host => "host",
            Self::Operation => "operation",
            Self::Usage => "usage",
            Self::Provisioning => "provisioning",
        }
    }
}

/// The report of one `ensure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub status: Status,
    /// `--dry-run` was asked for: nothing is deleted whatever `status` says
    /// (a dry run that fails has `status: error`, not `dry_run`).
    pub dry_run: bool,
    /// UTC start of the run, when the wall clock was representable.
    pub started_at: Option<String>,
    /// Monotonic elapsed time for the whole run, when measured.
    pub duration_ms: Option<u64>,
    /// The seed used to shuffle configured random groups, if any.
    pub seed: Option<u64>,
    /// A reclaim's tally budget ended before all candidates were tallied.
    pub tally_cut_short: bool,
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
    /// A dry run's planned source terms ([`PlannedTerm`]); empty otherwise.
    pub planned_terms: Vec<PlannedTerm>,
    pub actions: Vec<ActionEntry>,
    pub skipped: Vec<SkipEntry>,
    /// `None` (JSON `null`) when no complete process scan of this run
    /// answered: the scan failed or was not made, or its process list did
    /// not settle (TASK-187: unknown is never `[]`, which means none held).
    pub held_by_deleted_files: Option<Vec<HeldEntry>>,
    pub overruns: Vec<OverrunEntry>,
    pub errors: Vec<ErrorEntry>,
}

impl Report {
    /// Keep the first 1,000 action details as they are; the remainder is
    /// one summary with no tier, profile or candidate (`null` in the JSON),
    /// preserving the count and measured totals.
    fn output_actions(&self) -> (&[ActionEntry], Option<ActionOverflow>) {
        const DETAIL_LIMIT: usize = 1_000;
        let split = self.actions.len().min(DETAIL_LIMIT);
        let (details, overflow) = self.actions.split_at(split);
        if overflow.is_empty() {
            return (details, None);
        }
        let delta = overflow.iter().try_fold(0_i128, |sum, action| {
            sum.checked_add(i128::from(action.avail_delta_bytes?))
        });
        let summary = ActionOverflow {
            avail_delta_bytes: delta.and_then(|sum| i64::try_from(sum).ok()),
            duration_ms: overflow
                .iter()
                .fold(0_u64, |sum, action| sum.saturating_add(action.duration_ms)),
            count: overflow
                .iter()
                .fold(0_u64, |sum, action| sum.saturating_add(action.count)),
        };
        (details, Some(summary))
    }

    /// Above 20 candidates, show one row per tier/profile/reason with at
    /// most 20 example paths and no candidate. Preserve the first
    /// occurrence's output order.
    fn output_skips(&self) -> Vec<SkipEntry> {
        const EXAMPLE_LIMIT: usize = 20;
        type Key<'a> = (&'a str, &'a str, &'a str);
        let mut groups: BTreeMap<Key<'_>, (u64, Vec<PathBuf>)> = BTreeMap::new();
        for skip in &self.skipped {
            let group = groups
                .entry((&skip.tier, &skip.profile, skip.reason.as_str()))
                .or_insert_with(|| (0, Vec::new()));
            group.0 = group.0.saturating_add(skip.count);
            if let Some(candidate) = &skip.candidate
                && group.1.len() < EXAMPLE_LIMIT
                && !group.1.contains(candidate)
            {
                group.1.push(candidate.clone());
            }
            for example in &skip.examples {
                if group.1.len() == EXAMPLE_LIMIT {
                    break;
                }
                if !group.1.contains(example) {
                    group.1.push(example.clone());
                }
            }
        }
        let mut emitted = BTreeSet::new();
        let mut output = Vec::with_capacity(self.skipped.len());
        for skip in &self.skipped {
            let key = (
                skip.tier.as_str(),
                skip.profile.as_str(),
                skip.reason.as_str(),
            );
            let (count, examples) = &groups[&key];
            if *count > EXAMPLE_LIMIT as u64 {
                if emitted.insert(key) {
                    output.push(SkipEntry {
                        tier: skip.tier.clone(),
                        profile: skip.profile.clone(),
                        candidate: None,
                        count: *count,
                        examples: examples.clone(),
                        reason: skip.reason,
                        detail: None,
                        process_blockers: None,
                        unknown_cause: None,
                        estimated_bytes: None,
                    });
                }
            } else {
                output.push(skip.clone());
            }
        }
        output
    }

    /// An empty report of `status` for `path`; `dry_run` as asked for.
    pub fn new(status: Status, path: Option<&Path>, dry_run: bool) -> Self {
        Self {
            status,
            dry_run,
            started_at: None,
            duration_ms: None,
            seed: None,
            tally_cut_short: false,
            mount: None,
            path: path.map(Path::to_owned),
            min_free_bytes: None,
            target_free_bytes: None,
            avail_before_bytes: None,
            avail_after_bytes: None,
            plan: Vec::new(),
            planned_terms: Vec::new(),
            actions: Vec::new(),
            skipped: Vec::new(),
            held_by_deleted_files: None,
            overruns: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// A usage error (exit 2) saying `message`; the fields determined so
    /// far are kept.
    pub fn usage_error(mut self, message: impl Into<String>) -> Self {
        self = self.usage_error_with_cause(ErrorCause::Usage, message);
        self
    }

    pub fn usage_error_with_cause(mut self, cause: ErrorCause, message: impl Into<String>) -> Self {
        self.status = Status::UsageError;
        self.errors.push(ErrorEntry::run(cause, message));
        self
    }

    /// A refusal for `cause` saying `message` ([`refusal_status`]: exit 3
    /// for `host`, 2 for every other cause); the fields determined so far
    /// are kept.
    pub fn refused(mut self, cause: ErrorCause, message: impl Into<String>) -> Self {
        self.status = refusal_status(cause);
        self.errors.push(ErrorEntry::run(cause, message));
        self
    }

    /// The space goal as asked for, before anything is measured: a byte
    /// count is known from the command line, a percentage only from the
    /// filesystem's size (it stays `None` until the first sample). A run
    /// that fails before its first sample still says what it was asked to
    /// reach (TASK-191). `target` `None`: `--target-free` defaults to
    /// `--min-free`.
    pub fn with_goal(mut self, min: Size, target: Option<Size>) -> Self {
        self.min_free_bytes = min.absolute_bytes();
        self.target_free_bytes = target.unwrap_or(min).absolute_bytes();
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
        self.json().to_text()
    }

    /// `watch`'s record of a reclaim cycle (docs/reference.md, Watch JSON
    /// lines): the object of [`Self::to_json`] with `exit_code` `null`
    /// (the process does not exit) and one more field, `watch`, last.
    pub fn to_watch_json(&self, watch: Json) -> String {
        let Json::Obj(mut fields) = self.json() else {
            unreachable!("a report is an object")
        };
        for (key, value) in &mut fields {
            if *key == "exit_code" {
                *value = Json::Null;
            }
        }
        fields.push(("watch", watch));
        Json::Obj(fields).to_text()
    }

    fn json(&self) -> Json {
        let path = |p: &Path| Json::str(p.to_string_lossy());
        let opt_u64 = |v: Option<u64>| Json::opt(v, Json::U64);
        let (actions, overflow) = self.output_actions();
        Json::Obj(vec![
            ("status", Json::str(self.status.as_str())),
            ("exit_code", Json::U64(self.exit_code().into())),
            ("dry_run", Json::Bool(self.dry_run)),
            (
                "started_at",
                Json::opt(self.started_at.as_deref(), Json::str),
            ),
            ("duration_ms", opt_u64(self.duration_ms)),
            ("version", Json::str(env!("CARGO_PKG_VERSION"))),
            ("mount", Json::opt(self.mount.as_deref(), path)),
            ("path", Json::opt(self.path.as_deref(), path)),
            ("seed", Json::opt(self.seed, Json::U64)),
            ("tally_cut_short", Json::Bool(self.tally_cut_short)),
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
                                ("candidate", Json::opt(p.candidate.as_deref(), path)),
                                ("eligible", Json::Bool(p.reason.is_none())),
                                (
                                    "reason",
                                    Json::opt(p.reason, |reason| Json::str(reason.as_str())),
                                ),
                                ("estimated_bytes", opt_u64(p.estimated_bytes)),
                                ("operation", Json::str(&p.operation)),
                                ("checkouts", Json::str(p.checkouts.as_str())),
                                (
                                    "build_evidence",
                                    Json::opt(p.build_evidence.as_ref(), |evidence| {
                                        Json::Obj(vec![
                                            ("marker", Json::str(evidence.marker)),
                                            ("source", path(&evidence.source)),
                                            (
                                                "idle_age_seconds",
                                                opt_u64(evidence.idle_age_seconds),
                                            ),
                                        ])
                                    }),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "actions",
                Json::Arr(
                    actions
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
                                ("idle_age_seconds", opt_u64(a.idle_age_seconds)),
                                ("count", Json::U64(a.count)),
                            ])
                        })
                        .chain(overflow.map(|o| {
                            Json::Obj(vec![
                                ("tier", Json::Null),
                                ("profile", Json::Null),
                                ("candidate", Json::Null),
                                (
                                    "avail_delta_bytes",
                                    Json::opt(o.avail_delta_bytes, Json::I64),
                                ),
                                ("duration_ms", Json::U64(o.duration_ms)),
                                ("idle_age_seconds", Json::Null),
                                ("count", Json::U64(o.count)),
                            ])
                        }))
                        .collect(),
                ),
            ),
            (
                "skipped",
                Json::Arr(
                    self.output_skips()
                        .iter()
                        .map(|s| {
                            Json::Obj(vec![
                                ("tier", Json::str(&s.tier)),
                                ("profile", Json::str(&s.profile)),
                                ("candidate", Json::opt(s.candidate.as_deref(), path)),
                                ("count", Json::U64(s.count)),
                                (
                                    "examples",
                                    Json::Arr(s.examples.iter().map(|e| path(e)).collect()),
                                ),
                                ("reason", Json::str(s.reason.as_str())),
                                ("detail", Json::opt(s.detail.as_deref(), Json::str)),
                                (
                                    "process_blockers",
                                    Json::opt(s.process_blockers.as_ref(), |blockers| {
                                        Json::Arr(
                                            blockers.iter().map(process_blocker_json).collect(),
                                        )
                                    }),
                                ),
                                (
                                    "unknown_cause",
                                    Json::opt(s.unknown_cause, |cause| Json::str(cause.as_str())),
                                ),
                                ("estimated_bytes", opt_u64(s.estimated_bytes)),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "summary",
                Json::Arr(summary::terms(self).iter().map(term_json).collect()),
            ),
            (
                "held_by_deleted_files",
                Json::opt(self.held_by_deleted_files.as_deref(), |held| {
                    Json::Arr(
                        held.iter()
                            .map(|h| {
                                Json::Obj(vec![
                                    ("pid", Json::U64(u64::from(h.pid))),
                                    ("command", Json::opt(h.command.as_deref(), Json::str)),
                                    ("bytes", Json::U64(h.bytes)),
                                ])
                            })
                            .collect(),
                    )
                }),
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
                Json::Arr(self.errors.iter().map(ErrorEntry::to_json).collect()),
            ),
        ])
    }

    /// The human-mode result for stdout (docs/reference.md, Output):
    /// `None` when nothing needed doing (the goal was met and nothing was
    /// deleted, like `cp`'s silence); a dry run always prints its plan;
    /// otherwise a bounded summary, one line per candidate with its final
    /// outcome over every tier ([`outcome`], [`human`]). Errors are
    /// summarised here and logged in full on stderr as they happen.
    pub fn summary(&self) -> Option<String> {
        human::summary(self)
    }

    /// One text per source term, `PROFILE [TIER]: ...`, as the human
    /// result shows them ([`summary::terms`]): `watch`'s cycle line joins
    /// them.
    pub fn term_summaries(&self) -> Vec<String> {
        human::term_texts(self)
    }
}
