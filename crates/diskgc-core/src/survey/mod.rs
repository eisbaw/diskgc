//! `survey` (docs/reference.md, `survey`): what the profiles match below
//! each `PATH`, what could be freed, and what no profile matches yet. It
//! is the dry run's plan (`executor::survey_tier`, each tier on its own) without a goal, one
//! tier per profile, plus the per-term `summary` of that plan and the
//! unmatched top-level entries of `PATH` grouped by name family. Read-only:
//! given only a [`Host`], it deletes nothing, takes no lock and writes no
//! state (no index snapshot is read or written: the plan judges every
//! candidate whatever an index holds).
//!
//! Generic over the `Host` port, like [`crate::invocation`], so the binary
//! and the model tests run the same decisions. The result ([`Survey`])
//! keeps each `PATH`'s plan as the dry run reported it; the summary,
//! candidate lists and families are views derived from it ([`output`]).

mod output;
pub mod unmatched;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::config::{Config, IpcPolicy};
use crate::executor::{self, Request, SelectionFailure, Tier};
use crate::host::{Host, MonoTime, Timestamp};
use crate::invocation::{self, ConfigFile, ConfigUse, Platform};
use crate::profile::{Body, Finds, Library, Roots, RootsError};
use crate::report::{ErrorCause, ErrorEntry, Report, Status};
use crate::select;
use crate::units::Size;

/// `survey`'s arguments, checked for syntax (docs/reference.md,
/// `survey`), without the configuration lookup and output flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurveyOptions {
    /// Each absolute; at least one.
    pub paths: Vec<PathBuf>,
    pub select: Option<String>,
    /// Roots for `roots: required` profiles in `--select`, in place of
    /// each `PATH`; only with `--select`.
    pub roots: Vec<String>,
    /// The cooperative deadline of the whole survey; `None` for none.
    pub timeout: Option<Duration>,
}

/// What [`load`] checked before the host starts: the configuration (if
/// one was found) and the library the survey selects from.
#[derive(Debug, Clone)]
pub struct Loaded {
    config: Option<Config>,
    library: Library,
}

/// The result of one survey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Survey {
    /// The worst of the paths' plans ([`worst`]), or the refusal's.
    pub status: Status,
    pub started_at: Option<String>,
    pub duration_ms: Option<u64>,
    /// One per `PATH` surveyed, in the order given.
    pub paths: Vec<PathSurvey>,
    /// Errors of the survey as a whole (a refusal before any plan).
    pub errors: Vec<ErrorEntry>,
}

/// What a survey found below one `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSurvey {
    /// The dry run's plan of every tier, without a goal.
    pub plan: Report,
    /// The wall clock when the plan finished: ages are measured from it.
    pub surveyed_at: Timestamp,
    /// The profiles not run below this `PATH` (without `--select`), and
    /// why: what they would match is not looked for, and may be listed
    /// as unmatched.
    pub not_surveyed: Vec<NotSurveyed>,
    /// The tiers whose plan failed (an unsafe root, a listing or scan the
    /// host failed): their profiles were not surveyed, whatever their
    /// summary would count, so they have no block; `plan.errors` says why
    /// (docs/reference.md, Survey JSON). A tier the deadline cut is not
    /// failed: its block counts what it did not judge.
    pub failed: Vec<String>,
    /// The unmatched top-level entries; `None` when `PATH` was not listed
    /// (its plan was refused, or the listing failed: an error says why).
    pub unmatched: Option<Vec<unmatched::Entry>>,
}

/// A profile not surveyed below a `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotSurveyed {
    pub profile: String,
    pub why: Why,
}

/// Why a profile was not surveyed below a `PATH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// Its fixed roots are all outside `PATH` (`/tmp`'s under `~`).
    RootsElsewhere,
    /// A backend kind (Docker, Podman, Nix): its store is not below a
    /// `PATH`; `--select` names it.
    Backend,
}

impl Why {
    /// Its name in the JSON (`not_surveyed[].why`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RootsElsewhere => "roots_elsewhere",
            Self::Backend => "backend",
        }
    }
}

/// The tiers a survey runs below one `PATH`, and the profiles it does not.
#[derive(Debug, Clone)]
struct Below {
    tiers: Vec<Tier>,
    not_surveyed: Vec<NotSurveyed>,
}

impl Survey {
    /// A survey refused before any plan: nothing was looked at.
    pub fn refused(cause: ErrorCause, why: impl Into<String>) -> Self {
        Self {
            status: invocation::refusal_status(cause),
            started_at: None,
            duration_ms: None,
            paths: Vec::new(),
            errors: vec![ErrorEntry::run(cause, why)],
        }
    }

    pub fn exit_code(&self) -> u8 {
        self.status.exit_code()
    }
}

/// Step 1, which reads nothing on the machine: the configuration file
/// under the catalog (user profiles, `protect`; its groups are not used),
/// and `--select`, parsed and checked against the library. A bad one is
/// exit 2 whatever the host, as for `ensure`. `log` receives the
/// configuration's path and warnings.
pub fn load(
    options: &SurveyOptions,
    config_file: &ConfigFile,
    platform: &Platform<'_>,
    log: &mut dyn FnMut(&str),
) -> Result<Loaded, (ErrorCause, String)> {
    let config = match config_file {
        ConfigFile::Read { path, bytes } => Some(
            platform
                .catalog
                .config(path, bytes, ConfigUse::Run)
                .map_err(|why| (ErrorCause::Config, why))?,
        ),
        ConfigFile::NotFound { .. } => None,
    };
    let library = match &config {
        Some(config) => {
            log(&format!("config {}", config.path.display()));
            for warning in &config.warnings {
                log(&format!("warning: {warning}"));
            }
            config
                .validate_mode(false)
                .map_err(|why| (ErrorCause::Safety, why))?;
            if config.ipc == IpcPolicy::LiveOnly {
                return Err((
                    ErrorCause::Safety,
                    "ipc: live-only is valid in the configuration, but this version cannot \
                     enforce it yet; remove `ipc:` to use the default, block-any"
                        .to_owned(),
                ));
            }
            config.library.clone()
        }
        None => (platform.builtins)()
            .and_then(|builtins| platform.catalog.library(builtins))
            .map_err(|why| (ErrorCause::Config, why))?,
    };
    if let Some(expr) = &options.select {
        let first = options.paths.first().expect("survey has a PATH");
        let compiled = compile(expr, &library, &options.roots, first)?;
        for warning in compiled.warnings() {
            log(&format!("warning: --select {expr:?}: {warning}"));
        }
    }
    Ok(Loaded { config, library })
}

/// Step 2: the survey on the host. Every error is in the result; nothing
/// is deleted, no lock is taken and no state is written. `log` receives
/// diagnostics, `progress` the planner's progress lines.
pub fn run<H: Host>(
    host: &H,
    options: &SurveyOptions,
    loaded: &Loaded,
    log: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(&str),
) -> Survey {
    let started = host.now();
    let wall = host.wall_clock();
    let mut survey = match survey_paths(host, options, loaded, started, log, progress) {
        Ok(survey) => survey,
        Err((cause, why)) => {
            log(&format!("error: {why}"));
            Survey::refused(cause, why)
        }
    };
    survey.started_at = executor::rfc3339(wall);
    survey.duration_ms = u64::try_from(host.now().saturating_since(started).as_millis()).ok();
    if survey.status != Status::UsageError
        && let Some(signal) = host.interrupted()
    {
        survey.status = Status::Interrupted(signal);
    }
    survey
}

/// [`run`], with a refusal before any plan as its error.
fn survey_paths<H: Host>(
    host: &H,
    options: &SurveyOptions,
    loaded: &Loaded,
    started: MonoTime,
    log: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(&str),
) -> Result<Survey, (ErrorCause, String)> {
    // Run as the user whose files it reports: root is refused, as for
    // `ensure` without `--allow-root` (survey has no root mode).
    let uid = executor::privileges(host, false).map_err(|e| (e.cause, e.message))?;
    let protections = match &loaded.config {
        Some(config) => executor::bind_protections(config, uid, false)
            .map_err(|why| (ErrorCause::Config, why))?,
        None => Vec::new(),
    };
    let mut planned: Vec<(&Path, Below)> = Vec::with_capacity(options.paths.len());
    for path in &options.paths {
        invocation::refuse_missing_roots(
            host,
            loaded.config.as_ref(),
            path,
            &options.roots,
            false,
            uid,
        )?;
        let mut below = tiers_below(options, &loaded.library, path, uid, log)?;
        for tier in &mut below.tiers {
            tier.protections = protections.clone();
        }
        planned.push((path, below));
    }
    // Each PATH named before the environment: a missing one is the
    // caller's mistake (exit 2), not the host's.
    for path in &options.paths {
        executor::reach_path(host, path).map_err(|failure| match failure {
            executor::MeasureFailure::Usage(why) => (ErrorCause::Usage, why),
            executor::MeasureFailure::Host(why) => (ErrorCause::Host, why),
        })?;
    }
    // As a dry run (TASK-263): a plan whose in-use verdicts cannot see
    // every process is refused, not reported as if it could.
    if planned
        .iter()
        .flat_map(|(_, below)| &below.tiers)
        .any(Tier::relies_on_in_use_check)
    {
        executor::process_visibility(host).map_err(|e| (e.cause, e.message))?;
    }
    let deadline = options
        .timeout
        .map(|t| started.checked_add(t).unwrap_or(MonoTime(Duration::MAX)));
    let mut paths = Vec::with_capacity(planned.len());
    let mut cut = false;
    // A process scan's note (other users' processes not inspected) once
    // per survey, not once per tier.
    let mut notes = executor::NotesOncePerRun::new(progress);
    for (path, below) in planned {
        if host.interrupted().is_some() {
            break;
        }
        let (survey, cut_here) = survey_path(host, path, below, uid, deadline, log, &mut |line| {
            notes.line(line);
        });
        cut |= cut_here;
        paths.push(survey);
    }
    let mut status = worst(paths.iter().map(|p| p.plan.status));
    if cut && status == Status::DryRun {
        status = Status::Deadline;
    }
    Ok(Survey {
        status,
        started_at: None,
        duration_ms: None,
        paths,
        errors: Vec::new(),
    })
}

/// The plan of each tier below `path`, each on its own (one profile's
/// error stops no other), then its unmatched entries; and whether the
/// deadline left an unmatched entry unsized.
#[allow(clippy::too_many_arguments)]
fn survey_path<H: Host>(
    host: &H,
    path: &Path,
    below: Below,
    uid: u32,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(&str),
) -> (PathSurvey, bool) {
    let Below {
        tiers,
        not_surveyed,
    } = below;
    let mut plan: Option<Report> = None;
    let mut failed: Vec<String> = Vec::new();
    for (index, tier) in tiers.iter().enumerate() {
        let passed = deadline.is_some_and(|end| host.now() >= end);
        if let Some(signal) = host.interrupted() {
            // This PATH's plan is incomplete: it says so, and its entries
            // are not listed (the survey as a whole is interrupted too).
            if let Some(plan) = plan.as_mut() {
                plan.status = Status::Interrupted(signal);
            }
            break;
        }
        if let Some(plan) = plan.as_mut().filter(|_| passed) {
            // As a grouped dry run at its deadline: every tier not
            // planned says so. A worse status (an earlier profile's
            // error) stays.
            plan.status = worst([plan.status, Status::Deadline].into_iter());
            plan.errors
                .extend(tiers[index..].iter().map(|t| ErrorEntry {
                    tier: Some(t.name.clone()),
                    profile: Some(t.profile.clone()),
                    candidate: None,
                    cause: ErrorCause::Operation,
                    message: "deadline passed before this profile was planned".to_owned(),
                }));
            break;
        }
        let request = Request {
            path: path.to_owned(),
            // No goal: the plan judges every candidate, and nothing is
            // reclaimed toward one.
            min_free: Size::Bytes(0),
            target_free: None,
            timeout: deadline.map(|end| end.saturating_since(host.now())),
            tally_budget: None,
            tier: tier.clone(),
            watch: None,
        };
        let part = executor::survey_tier(host, &request, &[uid], progress);
        if matches!(part.status, Status::Error | Status::UsageError) {
            failed.push(tier.name.clone());
        }
        plan = Some(match plan {
            None => part,
            Some(whole) => merged(whole, part),
        });
    }
    let mut plan = plan.unwrap_or_else(|| {
        log(&format!(
            "survey: {}: no profile looks below it",
            path.display()
        ));
        Report::new(Status::DryRun, Some(path), true)
    });
    // A survey has no goal: the plan's sample of PATH stays (`avail`), the
    // goal of 0 the request needed does not.
    plan.min_free_bytes = None;
    plan.target_free_bytes = None;
    let surveyed_at = host.wall_clock();
    let listable = !matches!(plan.status, Status::UsageError | Status::Interrupted(_));
    let mut cut = false;
    let unmatched = if !listable {
        None
    } else if is_a_candidate(&plan, path) {
        log(&format!(
            "survey: {}: it is a candidate itself: no entry below it is unmatched",
            path.display()
        ));
        Some(Vec::new())
    } else {
        let matched = matched_names(&plan, path);
        match unmatched::entries(host, path, &matched, deadline, log) {
            Ok(Some(entries)) => {
                cut = host.interrupted().is_none()
                    && entries
                        .iter()
                        .any(|e| e.size == unmatched::Size::Unsized(unmatched::Unsized::Stopped));
                Some(entries)
            }
            Ok(None) => None,
            Err(why) => {
                let why = format!("survey: {}: unmatched entries: {why}", path.display());
                log(&format!("error: {why}"));
                plan.errors.push(ErrorEntry::run(ErrorCause::Host, why));
                if plan.status == Status::DryRun {
                    plan.status = Status::Error;
                }
                None
            }
        }
    };
    (
        PathSurvey {
            plan,
            surveyed_at,
            not_surveyed,
            failed,
            unmatched,
        },
        cut,
    )
}

/// `part`, a later tier's plan of the same `PATH`, added to `whole`: its
/// rows, terms, errors and overruns after `whole`'s, the worse status;
/// the first sample of `PATH` stays.
fn merged(mut whole: Report, part: Report) -> Report {
    whole.status = worst([whole.status, part.status].into_iter());
    whole.mount = whole.mount.or(part.mount);
    whole.avail_before_bytes = whole.avail_before_bytes.or(part.avail_before_bytes);
    whole.avail_after_bytes = part.avail_after_bytes.or(whole.avail_after_bytes);
    whole.plan.extend(part.plan);
    whole.terms.extend(part.terms);
    whole.errors.extend(part.errors);
    whole.overruns.extend(part.overruns);
    whole.duration_ms = whole
        .duration_ms
        .zip(part.duration_ms)
        .map(|(a, b)| a.saturating_add(b));
    whole
}

/// Whether a row of the plan has `path` itself as its candidate (`survey
/// ~/p/target` with `cargo-target`): then every entry below is matched.
/// A root row (`other_fs`, `missing`) never is `PATH`, which was reached.
fn is_a_candidate(plan: &Report, path: &Path) -> bool {
    plan.plan
        .iter()
        .any(|row| row.candidate.as_deref() == Some(path))
}

/// The names of `path`'s top-level entries that hold a candidate of the
/// plan, or are one (any row with a candidate: eligible or kept).
fn matched_names(plan: &Report, path: &Path) -> BTreeSet<OsString> {
    plan.plan
        .iter()
        .filter_map(|row| row.candidate.as_deref())
        .filter_map(
            |candidate| match candidate.strip_prefix(path).ok()?.components().next() {
                Some(Component::Normal(name)) => Some(name.to_owned()),
                _ => None,
            },
        )
        .collect()
}

/// The status of a survey of several plans: the first of usage error,
/// interruption, error, deadline that any plan has, else `dry_run`.
fn worst(statuses: impl Iterator<Item = Status>) -> Status {
    let rank = |status: Status| match status {
        Status::UsageError => 0,
        Status::Interrupted(_) => 1,
        Status::Error => 2,
        Status::Deadline => 3,
        _ => 4,
    };
    statuses
        .min_by_key(|status| rank(*status))
        .filter(|status| rank(*status) < 4)
        .unwrap_or(Status::DryRun)
}

/// The tiers of a survey below `path`: `--select`'s one tier, or one per
/// source profile of the library whose kind looks below a path (`fs`,
/// `cargo-target`; the backend kinds' stores are not below a `PATH`, so
/// they are surveyed only when `--select` names them). Every root is held
/// to `path` ([`at_or_below`]); without `--select`, a profile none of
/// whose roots is left is not surveyed below `path`. Both kinds of
/// profile left out are named ([`NotSurveyed`]).
fn tiers_below(
    options: &SurveyOptions,
    library: &Library,
    path: &Path,
    uid: u32,
    log: &mut dyn FnMut(&str),
) -> Result<Below, (ErrorCause, String)> {
    if let Some(expr) = &options.select {
        let compiled = compile(expr, library, &options.roots, path)?;
        let mut tier = tier_of(&compiled, library, uid)?;
        let had_roots = !tier.roots.is_empty();
        hold_roots_below(&mut tier, path, log);
        let roots_left =
            !tier.roots.is_empty() || tier.fs_union.iter().any(|t| !t.roots.is_empty());
        if had_roots && !roots_left {
            // Every root outside PATH: not "found nothing", not looked for.
            let mut profiles = vec![tier.profile.clone()];
            profiles.extend(tier.fs_union.iter().map(|t| t.profile.clone()));
            return Ok(Below {
                tiers: Vec::new(),
                not_surveyed: profiles
                    .into_iter()
                    .map(|profile| NotSurveyed {
                        profile,
                        why: Why::RootsElsewhere,
                    })
                    .collect(),
            });
        }
        return Ok(Below {
            tiers: vec![tier],
            not_surveyed: Vec::new(),
        });
    }
    let mut below = Below {
        tiers: Vec::new(),
        not_surveyed: Vec::new(),
    };
    let mut not = |profile: &str, why: Why| {
        below.not_surveyed.push(NotSurveyed {
            profile: profile.to_owned(),
            why,
        });
    };
    let mut tiers = Vec::new();
    for profile in library.profiles() {
        let Body::Source(source) = &profile.body else {
            continue;
        };
        if !matches!(source.finds, Finds::Paths { .. } | Finds::CargoTargets) {
            not(&profile.name, Why::Backend);
            continue;
        }
        let expr = format!("profile:{}", profile.name);
        let roots = match source.roots {
            Roots::Required => Some(vec![path.to_string_lossy().into_owned()]),
            Roots::Fixed(_) | Roots::Backend => None,
        };
        let compiled =
            Tier::compile_selection(&expr, library, roots.as_deref()).map_err(selection_failure)?;
        let mut tier = tier_of(&compiled, library, uid)?;
        tier.roots.retain(|root| at_or_below(root, path));
        if tier.roots.is_empty() {
            not(&profile.name, Why::RootsElsewhere);
            continue;
        }
        tier.name.clone_from(&profile.name);
        tiers.push(tier);
    }
    below.tiers = tiers;
    Ok(below)
}

/// `expr` compiled against `library`, its `roots: required` profiles bound
/// to `--roots` when given, else to `path`.
fn compile(
    expr: &str,
    library: &Library,
    roots: &[String],
    path: &Path,
) -> Result<executor::CompiledSelection, (ErrorCause, String)> {
    let supplied: Vec<String> = if !roots.is_empty() {
        roots.to_vec()
    } else if needs_roots(expr, library)? {
        vec![path.to_string_lossy().into_owned()]
    } else {
        Vec::new()
    };
    let supplied = (!supplied.is_empty()).then_some(supplied.as_slice());
    Tier::compile_selection(expr, library, supplied).map_err(selection_failure)
}

/// Whether `expr` names a `roots: required` profile, which `PATH` then
/// binds (`--roots` given for an expression without one stays the usage
/// error it is for `ensure`).
fn needs_roots(expr: &str, library: &Library) -> Result<bool, (ErrorCause, String)> {
    let checked = select::compile(expr, library).map_err(|e| (ErrorCause::Parse, e.to_string()))?;
    Ok(matches!(
        library.bind_roots(&checked, None),
        Err(RootsError::Missing { .. })
    ))
}

fn tier_of(
    compiled: &executor::CompiledSelection,
    library: &Library,
    uid: u32,
) -> Result<Tier, (ErrorCause, String)> {
    Tier::from_selection(compiled, library, uid).map_err(selection_failure)
}

fn selection_failure(failure: SelectionFailure) -> (ErrorCause, String) {
    match failure {
        SelectionFailure::Parse(message) => (ErrorCause::Parse, message),
        SelectionFailure::Usage(message) => (ErrorCause::Usage, message),
    }
}

/// `--select`'s tier held to `path`: a root (of its source or of a union
/// term) outside `path` is dropped, with a log line; a negated profile's
/// roots stay (its matches only exclude).
fn hold_roots_below(tier: &mut Tier, path: &Path, log: &mut dyn FnMut(&str)) {
    let mut dropped: Vec<PathBuf> = Vec::new();
    let mut hold = |roots: &mut Vec<PathBuf>| {
        roots.retain(|root| {
            let keep = at_or_below(root, path);
            if !keep {
                dropped.push(root.clone());
            }
            keep
        });
    };
    hold(&mut tier.roots);
    for term in &mut tier.fs_union {
        hold(&mut term.roots);
    }
    if !dropped.is_empty() {
        dropped.sort();
        dropped.dedup();
        log(&format!(
            "survey: {}: not looking in root(s) outside it: {}",
            path.display(),
            dropped
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
}

/// Whether `root` is `path` or below it, component by component (as
/// written: whether either is reached through a symlink is the planner's
/// `unsafe_path` to decide).
fn at_or_below(root: &Path, path: &Path) -> bool {
    root.starts_with(path)
}

#[cfg(test)]
mod tests;
