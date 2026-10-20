//! The executor (docs/architecture.md, The reclaim walk), first slice: the
//! minimal `ensure` of one `--select` tier of kind `cargo-target` (task-10)
//! or of kind `fs` with the `delete` action (task-18, [`crate::fs_kind`]),
//! and their dry runs. Later tasks extend it: groups, ranking and per-tier
//! discovery for config tiers (task-20), deadlines with overruns, signals
//! and re-sampling (task-54), the index (task-31), command actions
//! (task-37).
//!
//! Two entry points, kept apart by the type system:
//!
//! - [`ensure`] reclaims. It takes a [`Delete`] host, refuses an action it
//!   has no reclaim for, asks the pre-release sandbox gate
//!   ([`Delete::may_delete`]), then measures; at or above `--min-free` it
//!   returns at once, without the host lock. Otherwise it takes the host
//!   lock ([`hostlock`]), measures again, and runs the tier: discovery
//!   below the roots, a read-only tally of each candidate for the `oldest`
//!   order ([`cargo_target::tally`], [`fs_kind::tally`]), then the kind's
//!   own reclaim protocol on each in that order ([`cargo_target::reclaim`],
//!   [`fs_kind::reclaim`], each of which repeats every check under the
//!   candidate's locks), re-sampling after each, until the goal
//!   (`--target-free`) is observed.
//! - [`dry_run`] plans. It takes only a [`Host`], so it cannot reach a
//!   deleting method (nor the gate, which only `Delete` has): it measures,
//!   discovers and runs the eligibility check of every target
//!   ([`cargo_target::eligibility`]: no lock, no deletion), or of every
//!   `fs` candidate ([`fs_kind::plan`]: the reclaim's discovery and
//!   judgement), whatever the available space, and lists them in the order
//!   [`ensure`] would take them (`oldest`: least recently built or changed
//!   first), eligible first.
//!
//! Both start with [`privileges`] and a [`Tier`], built and checked by the
//! caller before anything touches the machine (static validation).
//!
//! Each [`ReclaimEnd`], whatever the kind, maps to the report as task-10's
//! note says (`Reclaim::record`): `Complete` is an action; `Skipped` a skip
//! (and an action too when entries were deleted part-way); `Failed` an
//! error of that candidate (the next one is still tried; an `fs`
//! candidate's `ENOTEMPTY` is never retried); `GateRefused` and
//! `Stopped(Failed)` errors of the run, which stop it;
//! `Stopped(DeadlinePassed)` the deadline path.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cargo_target::{self, Discovery, ReclaimEnd, Stop, Tally, Target};
use crate::command_action::{self, Preflight, Ready};
use crate::docker_backend::{self, Binding, Storage};
use crate::engine::{Checkouts, Identity, Reason, Rules, Skip, checkout_metadata_below};
use crate::fs_kind::{self, EntryFilter, Find};
use crate::host::{
    Cause, CommandExit, CommandInvocation, CommandRun, Delete, Dev, DevIno, Host, HostError,
    MonoTime, Space, Timestamp,
};
use crate::hostlock::{self, LockError};
use crate::inuse::{Checker, REUSE_LIMIT};
use crate::profile::{Action, Body, Finds, IdleFor, IdleWindow, Library};
use crate::report::{
    ActionEntry, BuildEvidence, ErrorEntry, Final, HeldEntry, PlanEntry, Report, SkipEntry, Status,
    after_lock_timeout, conclude, signed_bytes,
};
use crate::resolve::{Resolved, Want, resolve};
use crate::select::{self, Factor};
use crate::units::{Size, format_bytes};

/// The name of the one group and tier `--select` makes, in reports
/// (docs/reference.md, Commands).
pub const SELECT_TIER: &str = "--select";

/// One selected tier. Its ordinary fields hold the first source term;
/// [`Tier::fs_union`] holds any additional positive filesystem delete
/// terms. Built by [`Tier::select`] only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    pub group: String,
    pub name: String,
    /// The expression, normalised (as `check` prints it).
    pub select: String,
    /// The source profile.
    pub profile: String,
    /// What the source finds, by kind.
    pub source: SourceKind,
    /// Where it looks: the bound roots, `{uid}` replaced.
    pub roots: Vec<PathBuf>,
    /// The term's idle requirement (the longest of its `filter:`s).
    pub idle_for: Option<IdleFor>,
    /// Every negated idle filter requires a last use strictly newer than
    /// this shortest bound. Checked again in the kind's reclaim protocol.
    pub idle_before: Option<IdleFor>,
    /// The source profile's `checkouts` setting: whether a candidate may
    /// hold a VCS checkout (docs/safety.md, No source checkout).
    pub checkouts: Checkouts,
    /// The source profile's action (for the code-backed kinds, the kind's).
    pub action: Action,
    /// Additional positive filesystem terms of a union. The first term is
    /// held in the tier's ordinary fields, so there is one source of truth.
    /// Empty for a single-source tier.
    pub fs_union: Vec<FsUnionSource>,
}

/// One source contribution to a filesystem union. The same inode may be
/// found by several entries and is judged once with all its contributors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsUnionSource {
    pub profile: String,
    pub find: Find,
    pub roots: Vec<PathBuf>,
    pub idle: IdleWindow,
    pub checkouts: Checkouts,
}

/// A borrowed view of either the tier's first source or an additional
/// union term. Discovery indices refer to this ordered list.
#[derive(Clone, Copy)]
struct FsUnionSourceRef<'a> {
    profile: &'a str,
    find: &'a Find,
    roots: &'a [PathBuf],
    idle: IdleWindow,
    checkouts: Checkouts,
}

impl Tier {
    /// The action, as the report names it (`operation`, human-readable).
    pub fn operation(&self) -> String {
        self.action.to_string()
    }

    fn fs_sources(&self) -> Vec<FsUnionSourceRef<'_>> {
        let SourceKind::Fs(find) = &self.source else {
            unreachable!("a filesystem union retains a filesystem first source")
        };
        let mut sources = vec![FsUnionSourceRef {
            profile: &self.profile,
            find,
            roots: &self.roots,
            idle: IdleWindow {
                at_least: self.idle_for,
                before: self.idle_before,
            },
            checkouts: self.checkouts,
        }];
        sources.extend(self.fs_union.iter().map(|source| FsUnionSourceRef {
            profile: &source.profile,
            find: &source.find,
            roots: &source.roots,
            idle: source.idle,
            checkouts: source.checkouts,
        }));
        sources
    }
}

/// What a tier's source profile finds, by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// Cargo target directories, by their structure.
    CargoTarget,
    /// Entries matching its `find` (kind `fs`), deleted by the kind's own
    /// protocol ([`fs_kind::reclaim`]) when its action is `delete`.
    Fs(Box<Find>),
    DockerBuildCache,
    DockerImages,
}

impl Tier {
    /// The tier `--select EXPR` makes (with `--roots`, `roots`), checked
    /// against `library` (the built-ins plus the config's profiles), for
    /// the user `uid` (the `{uid}` placeholder). Every error is a usage
    /// error (exit 2) and says what is wrong; what the grammar allows but
    /// this build cannot run yet is refused rather than approximated.
    pub fn select(
        expr: &str,
        library: &Library,
        roots: Option<&[String]>,
        uid: u32,
    ) -> Result<Tier, String> {
        let checked = select::compile(expr, library).map_err(|e| e.to_string())?;
        let not_yet = |what: &str, task: &str| {
            format!(
                "--select `{expr}`: {what} is not supported by ensure in this build yet \
                 ({task}); select one source profile (`profile:NAME`, kind cargo-target, or \
                 kind fs), optionally with supported filters, or a positive fs delete union"
            )
        };
        let bound = library
            .bind_roots(&checked, roots)
            .map_err(|e| format!("--select `{expr}`: {e}"))?;
        let mut terms = Vec::new();
        for term in &checked.expr().terms {
            terms.push(Self::select_term(
                expr, term, library, &bound, uid, &not_yet,
            )?);
        }
        let mut first = terms.remove(0);
        if !terms.is_empty() {
            if !matches!(first.source, SourceKind::Fs(_))
                || !matches!(first.action, Action::Delete { .. })
            {
                return Err(not_yet(
                    "a union of this kind or action",
                    "task-16, task-20",
                ));
            }
            if terms.iter().any(|term| term.action != first.action) {
                return Err(format!(
                    "--select `{expr}`: filesystem union sources must have exactly the same delete action and options"
                ));
            }
            let mut extra = Vec::with_capacity(terms.len());
            for tier in &terms {
                let SourceKind::Fs(find) = &tier.source else {
                    return Err(not_yet("a union of non-filesystem sources", "task-16"));
                };
                extra.push(FsUnionSource {
                    profile: tier.profile.clone(),
                    find: (**find).clone(),
                    roots: tier.roots.clone(),
                    idle: IdleWindow {
                        at_least: tier.idle_for,
                        before: tier.idle_before,
                    },
                    checkouts: tier.checkouts,
                });
            }
            first.fs_union = extra;
            first.select = checked.expr().to_string();
        }
        Ok(first)
    }

    fn select_term(
        expr: &str,
        term: &select::Term,
        library: &Library,
        bound: &std::collections::BTreeMap<String, Vec<String>>,
        uid: u32,
        not_yet: &impl Fn(&str, &str) -> String,
    ) -> Result<Tier, String> {
        if let Some(factor) = term
            .factors
            .iter()
            .find(|f| matches!(f, Factor::NotProfile(_)))
        {
            return Err(not_yet(&format!("`{factor}`"), "task-16"));
        }
        let Some(Body::Source(source)) = library.get(&term.source).map(|p| &p.body) else {
            return Err(format!(
                "--select `{expr}`: no source profile `{}`",
                term.source
            ));
        };
        // The loader admits only these finds, each paired with its action.
        let mut kind = match &source.finds {
            Finds::CargoTargets => SourceKind::CargoTarget,
            Finds::Paths { .. } => SourceKind::Fs(Box::new(
                Find::of(source, uid).expect("fs source has a filesystem find"),
            )),
            Finds::DockerBuildCache => SourceKind::DockerBuildCache,
            Finds::DockerImages => SourceKind::DockerImages,
        };
        let mut idle_before: Option<IdleFor> = None;
        for factor in &term.factors {
            let (name, negated) = match factor {
                Factor::Filter(name) => (name, false),
                Factor::NotFilter(name) => (name, true),
                Factor::NotProfile(_) => continue,
            };
            let Some(Body::Filter(where_)) = library.get(name).map(|p| &p.body) else {
                return Err(format!("--select `{expr}`: no filter profile `{name}`"));
            };
            if negated {
                if where_.predicates() == [crate::kind::PredicateName::IdleFor] {
                    let bound = where_.idle_for.expect("idle predicate present");
                    idle_before = Some(idle_before.map_or(bound, |old| old.min(bound)));
                    continue;
                }
                if !matches!(kind, SourceKind::Fs(_))
                    || !matches!(
                        where_.predicates().as_slice(),
                        [crate::kind::PredicateName::Has | crate::kind::PredicateName::HasNot]
                    )
                {
                    return Err(not_yet(
                        &format!("`!filter:{name}` (unsupported predicates)"),
                        "task-16",
                    ));
                }
                let SourceKind::Fs(find) = &mut kind else {
                    unreachable!()
                };
                find.entry_filters.push(EntryFilter {
                    name: name.clone(),
                    entry: where_
                        .has
                        .as_ref()
                        .or(where_.has_not.as_ref())
                        .expect("entry predicate present")
                        .clone(),
                    has: where_.has.is_some(),
                    negated: true,
                });
                continue;
            }
            if where_.has.is_some() || where_.has_dir.is_some() || where_.has_not.is_some() {
                return Err(not_yet(
                    &format!("`filter:{name}` (a `has` / `has_dir` / `has_not` filter)"),
                    "task-16",
                ));
            }
            if let SourceKind::Fs(find) = &mut kind {
                find.apply_version_filter(where_)
                    .map_err(|error| format!("--select `{expr}`: filter:{name}: {error}"))?;
            }
        }
        let idle_for = library.idle_for(term)?;
        let roots = bound
            .get(&term.source)
            .map(|roots| {
                roots
                    .iter()
                    .map(|r| PathBuf::from(r.replace("{uid}", &uid.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Tier {
            group: SELECT_TIER.to_owned(),
            name: SELECT_TIER.to_owned(),
            select: term.to_string(),
            profile: term.source.clone(),
            source: kind,
            roots,
            idle_for,
            idle_before,
            checkouts: source.checkouts,
            action: source.action.clone(),
            fs_union: Vec::new(),
        })
    }
}

/// What `ensure` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The directory whose filesystem must have the space (absolute).
    pub path: PathBuf,
    pub min_free: Size,
    /// `None`: the same as `min_free`.
    pub target_free: Option<Size>,
    /// The cooperative deadline of the whole call; `None` for none.
    pub timeout: Option<Duration>,
    /// How long the tally that orders a tier may take (`--tally-budget`;
    /// [`default_tally_budget`]); `None` for no limit. Targets it leaves
    /// untallied are taken after the tallied ones, in the order discovery
    /// found them. The dry run judges every target fully and ignores it.
    pub tally_budget: Option<Duration>,
    pub tier: Tier,
}

/// `--tally-budget`'s default (docs/reference.md, `ensure`): the smaller of
/// 30 s and a quarter of `--timeout` (30 s without a timeout).
pub fn default_tally_budget(timeout: Option<Duration>) -> Duration {
    const MOST: Duration = Duration::from_secs(30);
    timeout.map_or(MOST, |t| (t / 4).min(MOST))
}

/// The tier's `oldest` order (docs/architecture.md, Order within a tier):
/// least recently built first, an unknown last build last, ties in the
/// order given (a stable sort). The one ordering both [`ensure`] and
/// [`dry_run`] use, so a plan lists candidates as a reclaim takes them
/// (when the tally was not cut short by `--tally-budget`).
fn oldest_first<T>(items: &mut [T], last_build: impl Fn(&T) -> Option<Timestamp>) {
    items.sort_by_key(|item| {
        let last = last_build(item);
        (last.is_none(), last)
    });
}

/// Appends `entry` to the report's errors and logs it: no error reaches
/// the report without reaching stderr too.
fn record_error(report: &mut Report, log: &mut dyn FnMut(&str), entry: ErrorEntry) {
    let mut at = entry.tier.clone();
    if !entry.candidate.is_empty() {
        at = format!("{at} {}", entry.candidate);
    }
    if at.is_empty() {
        log(&format!("error: {}", entry.message));
    } else {
        log(&format!("{at}: error: {}", entry.message));
    }
    report.errors.push(entry);
}

/// A usage error (exit 2), logged.
fn usage(report: Report, log: &mut dyn FnMut(&str), message: String) -> Report {
    log(&format!("error: {message}"));
    report.usage_error(message)
}

/// Privileges (docs/safety.md, Privileges): diskgc refuses to run as root
/// without `--allow-root`. The caller validates and resolves `owners:`
/// before passing those IDs to the planner or reclaim. Returns the effective
/// uid used for `{uid}` substitution, not an implicit ownership allowlist.
pub fn privileges<H: Host>(host: &H, allow_root: bool) -> Result<u32, String> {
    let uid = host.effective_uid();
    if uid == 0 && !allow_root {
        return Err(
            "diskgc refuses to run as root (uid 0) without --allow-root (docs/safety.md, \
             Privileges): run it as the user whose caches it frees"
                .to_owned(),
        );
    }
    Ok(uid)
}

/// The requested filesystem, measured.
struct Measured<F> {
    /// `PATH`, reached safely (held for the run: its fd is what is sampled).
    at: Resolved<F>,
    dev: Dev,
    min: u64,
    target: u64,
}

/// Reaches `PATH`, samples its filesystem and turns the sizes into bytes;
/// fills the report's `mount`, sizes and `avail_before_bytes`. An error is
/// a usage error (the report says why).
fn measure<H: Host>(
    host: &H,
    request: &Request,
    report: &mut Report,
) -> Result<Measured<H::Fd>, String> {
    let at = resolve(host, &request.path, Want::Directory)
        .map_err(|e| format!("PATH {}: {e}", request.path.display()))?;
    report.mount = Some(at.mount.mount_point.clone());
    let space = host
        .statvfs(&at.fd)
        .map_err(|e| format!("PATH {}: {e}", request.path.display()))?;
    report.avail_before_bytes = Some(space.avail_bytes);
    let min = request
        .min_free
        .bytes(space.size_bytes)
        .map_err(|e| format!("--min-free {}: {e}", request.min_free))?;
    report.min_free_bytes = Some(min);
    let target = match request.target_free {
        None => min,
        Some(size) => size
            .bytes(space.size_bytes)
            .map_err(|e| format!("--target-free {size}: {e}"))?,
    };
    report.target_free_bytes = Some(target);
    if target < min {
        return Err(format!(
            "--target-free ({target} bytes) is below --min-free ({min} bytes)"
        ));
    }
    Ok(Measured {
        dev: at.stat.dev,
        at,
        min,
        target,
    })
}

/// The deadline `timeout` from now.
fn deadline_of<H: Host>(host: &H, timeout: Option<Duration>) -> Option<MonoTime> {
    timeout.and_then(|t| host.now().checked_add(t))
}

fn warn_if_host_lock_unready<H: Host>(host: &H, log: &mut dyn FnMut(&str)) {
    if let Err(error) = hostlock::readiness(host) {
        log(&format!("warning: host lock is not ready: {error}"));
    }
}

/// Brings `PATH`'s filesystem to the goal (see the module docs). Progress
/// goes to `log`, one line each.
///
/// Trusted-caller precondition: call [`privileges`] first. With root, load
/// policy through a protected config path, resolve `owners` to non-system
/// UIDs, and reject actions with [`Action::root_mode_error`] before calling.
/// The CLI enforces these conditions; this library entry point accepts an
/// already-validated request and ownership allowlist.
pub fn ensure<H: Delete>(
    host: &H,
    request: &Request,
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    // Not a success until a path below says so.
    let report = Report::new(Status::Error, Some(&request.path));
    // Before anything else (the gate, the first sample, the host lock):
    // only a source whose removal this build implements is reclaimed. An
    // allowlist: kind cargo-target through its lock protocol, kind fs
    // through its own deletion or checked command protocol. Anything else
    // is refused here, never approximated by a different kind's protocol.
    match (&request.tier.source, &request.tier.action) {
        (SourceKind::CargoTarget, Action::CargoLockProtocol)
        | (SourceKind::Fs(_), Action::Delete { .. } | Action::Command { .. })
        | (SourceKind::DockerBuildCache, Action::DockerBuilderPrune)
        | (SourceKind::DockerImages, Action::DockerImagePrune { .. }) => {}
        (_, action) => {
            return usage(
                report,
                log,
                format!(
                    "--select `{}`: `{}` removes with `{action}`, which this build does not \
                     run: a reclaiming ensure refuses it; use \
                     --dry-run for a read-only plan",
                    request.tier.select, request.tier.profile
                ),
            );
        }
    }
    let mut report = report;
    if let Err(gate) = host.may_delete() {
        return usage(
            report,
            log,
            format!(
                "a reclaiming ensure cannot run here: {gate} (use --dry-run for a read-only plan)"
            ),
        );
    }
    let deadline = deadline_of(host, request.timeout);
    let docker_argv = ["docker".to_owned()];
    let command_argv = match &request.tier.action {
        Action::Command { argv, .. } => Some(argv.as_slice()),
        Action::DockerBuilderPrune | Action::DockerImagePrune { .. } => {
            Some(docker_argv.as_slice())
        }
        _ => None,
    };
    let command_ready = if let Some(argv) = command_argv {
        match command_action::preflight(host, argv) {
            Ok(Preflight::Ready(ready)) => {
                log(&format!(
                    "{}: command executables: {}",
                    request.tier.name,
                    ready
                        .executables
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                Some(ready)
            }
            Ok(Preflight::Missing(program)) => {
                let measured = match measure(host, request, &mut report) {
                    Ok(measured) => measured,
                    Err(error) => return usage(report, log, error),
                };
                log(&format!(
                    "{}: profile `{}` is missing executable {program:?}",
                    request.tier.name, request.tier.profile
                ));
                report.skipped.push(SkipEntry {
                    tier: request.tier.name.clone(),
                    profile: request.tier.profile.clone(),
                    candidate: PathBuf::new(),
                    count: 1,
                    examples: Vec::new(),
                    reason: Reason::Missing.as_str(),
                });
                report.avail_after_bytes = Some(report.avail_before_bytes.expect("measured"));
                report.status = conclude(
                    Final::of(report.avail_after_bytes, measured.min),
                    false,
                    false,
                );
                if report.status == Status::Met {
                    warn_if_host_lock_unready(host, log);
                }
                return report;
            }
            Err(error) => {
                let measured = match measure(host, request, &mut report) {
                    Ok(measured) => measured,
                    Err(message) => return usage(report, log, message),
                };
                record_error(
                    &mut report,
                    log,
                    run_error(&request.tier, error.to_string()),
                );
                report.avail_after_bytes = Some(report.avail_before_bytes.expect("measured"));
                report.status = conclude(
                    Final::of(report.avail_after_bytes, measured.min),
                    false,
                    true,
                );
                return report;
            }
        }
    } else {
        None
    };
    let measured = match measure(host, request, &mut report) {
        Ok(m) => m,
        Err(e) => return usage(report, log, e),
    };
    let avail = report.avail_before_bytes.expect("measured");
    if avail >= measured.min {
        report.status = Status::Met;
        report.avail_after_bytes = Some(avail);
        warn_if_host_lock_unready(host, log);
        return report;
    }
    log(&format!(
        "{} available on {}, below --min-free {}: taking the host lock",
        format_bytes(avail),
        measured.at.mount.mount_point.display(),
        format_bytes(measured.min)
    ));
    let lock = match hostlock::acquire(host, deadline) {
        Ok(lock) => lock,
        Err(LockError::DeadlinePassed) => {
            log(&LockError::DeadlinePassed.to_string());
            // A fresh sample decides: the other holder may have freed enough.
            report.avail_after_bytes =
                final_sample(host, &measured, &mut report, log, &request.tier);
            report.status = after_lock_timeout(Final::of(report.avail_after_bytes, measured.min));
            return report;
        }
        Err(e) => return usage(report, log, e.to_string()),
    };
    log(&format!(
        "host lock {} taken after {} ms",
        hostlock::host_lock_path().display(),
        lock.waited.as_millis()
    ));
    // Someone else (the reclaim we waited for) may have freed it meanwhile.
    match sample(host, &measured) {
        Ok(space) if space.avail_bytes >= measured.min => {
            log(&format!(
                "{} available now: --min-free is met, nothing to reclaim",
                format_bytes(space.avail_bytes)
            ));
            report.status = Status::Met;
            report.avail_after_bytes = Some(space.avail_bytes);
            return report;
        }
        Ok(_) => {}
        Err(e) => {
            record_error(&mut report, log, run_error(&request.tier, e));
            return report;
        }
    }
    let mut run = Reclaim {
        host,
        tier: &request.tier,
        measured: &measured,
        deadline,
        tally_budget: request.tally_budget,
        command_ready,
        report: &mut report,
        log,
        deadline_hit: false,
    };
    run.run(owners);
    let deadline_hit = run.deadline_hit;
    report.avail_after_bytes = final_sample(host, &measured, &mut report, log, &request.tier);
    report.status = conclude(
        Final::of(report.avail_after_bytes, measured.min),
        deadline_hit,
        !report.errors.is_empty(),
    );
    // Released only here, by closing, after the final sample.
    drop(lock);
    report
}

/// Plans what [`ensure`] would do, deleting nothing and taking no lock
/// (docs/reference.md, `--dry-run`). Given only a [`Host`]. Its caller must
/// satisfy the same root policy and `owners` precondition as [`ensure`].
pub fn dry_run<H: Host>(
    host: &H,
    request: &Request,
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    let deadline = deadline_of(host, request.timeout);
    let mut report = Report::new(Status::DryRun, Some(&request.path));
    let measured = match measure(host, request, &mut report) {
        Ok(m) => m,
        Err(e) => return usage(report, log, e),
    };
    let tier = &request.tier;
    if matches!(
        tier.source,
        SourceKind::DockerBuildCache | SourceKind::DockerImages
    ) {
        match docker_backend::inspect_backend(host, measured.dev, docker_storage(tier)) {
            Ok(Binding::Bound(bound)) => {
                let candidate = docker_candidate(tier, &bound);
                log(&format!(
                    "{}: Docker backend bound to {}",
                    tier.name,
                    candidate.display()
                ));
                report.plan.push(plan_entry(tier, &candidate, None, None));
            }
            Ok(Binding::Skipped(skip)) => {
                log(&format!(
                    "{}: skip Docker backend: {}: {}",
                    tier.name, skip.reason, skip.detail
                ));
                report
                    .plan
                    .push(plan_entry(tier, Path::new(""), Some(skip.reason), None));
            }
            Err(error) => record_error(&mut report, log, run_error(tier, error.to_string())),
        }
        report.avail_after_bytes = final_sample(host, &measured, &mut report, log, tier);
        return report;
    }
    if let Action::Command { argv, .. } = &tier.action {
        match command_action::preflight(host, argv) {
            Ok(Preflight::Ready(ready)) => log(&format!(
                "{}: command executables: {}",
                tier.name,
                ready
                    .executables
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Ok(Preflight::Missing(program)) => {
                log(&format!(
                    "{}: profile `{}` is missing executable {program:?}",
                    tier.name, tier.profile
                ));
                report.plan.push(plan_entry(
                    tier,
                    Path::new(""),
                    Some(Reason::Missing.as_str()),
                    None,
                ));
                report.avail_after_bytes = final_sample(host, &measured, &mut report, log, tier);
                return report;
            }
            Err(error) => {
                record_error(&mut report, log, run_error(tier, error.to_string()));
                report.avail_after_bytes = final_sample(host, &measured, &mut report, log, tier);
                return report;
            }
        }
    }
    let rules = rules(tier, owners, log);
    let mut checker = Checker::for_plan(&rules);
    let planned = match &tier.source {
        SourceKind::CargoTarget => plan_cargo(
            host,
            tier,
            measured.dev,
            &rules,
            &mut checker,
            deadline,
            &mut report,
            log,
        ),
        SourceKind::Fs(find) => {
            if tier.fs_union.is_empty() {
                plan_fs(
                    host,
                    tier,
                    find,
                    measured.dev,
                    &rules,
                    deadline,
                    &mut report,
                    log,
                )
            } else {
                plan_fs_union(host, tier, measured.dev, &rules, deadline, &mut report, log)
            }
        }
        SourceKind::DockerBuildCache | SourceKind::DockerImages => unreachable!("handled above"),
    };
    let Planned {
        mut eligible,
        skipped,
        stopped,
    } = planned;
    oldest_first(&mut eligible, |(last, _)| *last);
    report.plan = eligible
        .into_iter()
        .map(|(_, e)| e)
        .chain(skipped)
        .collect();
    if let Some(stop) = stopped {
        if matches!(stop, Stop::DeadlinePassed { .. }) {
            report.status = Status::Deadline;
        }
        record_error(
            &mut report,
            log,
            run_error(tier, format!("stopped: {stop}")),
        );
    }
    // Unmeasured: unknown, not the earlier sample.
    report.avail_after_bytes = final_sample(host, &measured, &mut report, log, tier);
    report.held_by_deleted_files = held(host, &mut checker, measured.dev, log);
    report
}

/// A plan in the making: the eligible candidates with their order key
/// (the kind's idle measure), the skipped ones, and why planning stopped
/// early (an error of the run, or the deadline).
struct Planned {
    eligible: Vec<(Option<Timestamp>, PlanEntry)>,
    skipped: Vec<PlanEntry>,
    stopped: Option<Stop>,
}

/// One plan entry of `tier`.
fn plan_entry(
    tier: &Tier,
    candidate: &Path,
    reason: Option<&'static str>,
    bytes: Option<u64>,
) -> PlanEntry {
    PlanEntry {
        group: tier.group.clone(),
        tier: tier.name.clone(),
        profile: tier.profile.clone(),
        candidate: candidate.to_owned(),
        reason,
        estimated_bytes: bytes,
        operation: tier.operation(),
        checkouts: tier.checkouts,
        build_evidence: None,
    }
}

fn docker_candidate(tier: &Tier, bound: &docker_backend::Bound) -> PathBuf {
    match &tier.source {
        SourceKind::DockerBuildCache => bound.info.docker_root.join("buildkit"),
        SourceKind::DockerImages => bound.info.image_store.root().to_owned(),
        _ => unreachable!("Docker candidate for a non-Docker tier"),
    }
}

fn docker_storage(tier: &Tier) -> Storage {
    match tier.source {
        SourceKind::DockerBuildCache => Storage::BuildCache,
        SourceKind::DockerImages => Storage::Images,
        _ => unreachable!("Docker storage for a non-Docker tier"),
    }
}

/// The plan of a `cargo-target` tier: discovery, then the eligibility
/// check of every target.
#[allow(clippy::too_many_arguments)]
fn plan_cargo<H: Host>(
    host: &H,
    tier: &Tier,
    dev: Dev,
    rules: &Rules<'_>,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
    report: &mut Report,
    log: &mut dyn FnMut(&str),
) -> Planned {
    let entry = |candidate: &Path, reason: Option<&'static str>, bytes: Option<u64>| {
        plan_entry(tier, candidate, reason, bytes)
    };
    let mut eligible = Vec::new();
    let mut skipped = Vec::new();
    let mut stopped = None;
    match discover(host, tier, deadline, log) {
        Err(stop) => stopped = Some(stop),
        Ok(found) => {
            // As ensure: a misconfigured root is an error (not a plan
            // entry) and excludes the tier.
            let misconfigured = misconfigured_roots(tier, &found);
            for skip in &found.refused {
                if !misconfigured.contains(&skip) {
                    skipped.push(entry(&skip.path, Some(skip.reason.as_str()), None));
                }
            }
            for skip in &misconfigured {
                record_error(report, log, run_error(tier, excluded(skip)));
            }
            let targets = if misconfigured.is_empty() {
                found.targets.as_slice()
            } else {
                &[]
            };
            for target in targets {
                if let Some(skip) = other_fs(target, dev) {
                    log_skip(log, tier, &skip);
                    skipped.push(entry(&target.path, Some(skip.reason.as_str()), None));
                    continue;
                }
                match cargo_target::eligibility(
                    host,
                    target,
                    rules,
                    IdleWindow {
                        at_least: tier.idle_for,
                        before: tier.idle_before,
                    },
                    checker,
                    deadline,
                ) {
                    Ok(Ok(e)) => {
                        log(&format!(
                            "{} {}: eligible, {} entries, {} (last build {})",
                            tier.name,
                            target.path.display(),
                            e.entries,
                            format_bytes(e.bytes),
                            e.last_build
                                .map_or_else(|| "unknown".to_owned(), |t| t.secs.to_string())
                        ));
                        eligible.push((e.last_build, entry(&target.path, None, Some(e.bytes))));
                    }
                    Ok(Err(skip)) => {
                        log_skip(log, tier, &skip);
                        skipped.push(entry(&target.path, Some(skip.reason.as_str()), None));
                    }
                    Err(stop) => {
                        stopped = Some(stop);
                        break;
                    }
                }
            }
        }
    }
    Planned {
        eligible,
        skipped,
        stopped,
    }
}

/// The plan of an `fs` tier ([`fs_kind::plan`]): every candidate found
/// below the roots, judged. As for Cargo, a root reached through a symlink
/// or a mount crossing (`unsafe_path`) is a misconfiguration: an error,
/// and the tier is excluded.
#[allow(clippy::too_many_arguments)]
fn plan_fs<H: Host>(
    host: &H,
    tier: &Tier,
    find: &Find,
    dev: Dev,
    rules: &Rules<'_>,
    deadline: Option<MonoTime>,
    report: &mut Report,
    log: &mut dyn FnMut(&str),
) -> Planned {
    let mut planned = Planned {
        eligible: Vec::new(),
        skipped: Vec::new(),
        stopped: None,
    };
    let (found, stopped) = fs_kind::plan(
        host,
        &tier.roots,
        find,
        dev,
        rules,
        IdleWindow {
            at_least: tier.idle_for,
            before: tier.idle_before,
        },
        deadline,
        log,
    );
    // When the run stopped, the count is what was judged until then; the
    // candidates found but not judged are counted apart.
    let partial = if stopped.is_some() {
        format!(", stopped early: {} found but not judged", found.unjudged)
    } else {
        String::new()
    };
    planned.stopped = stopped;
    log(&format!(
        "{}: `{}`: {} candidate(s) judged below {} ({} found again through another root or \
         pattern, {} systemd-private-* passed over{partial})",
        tier.name,
        tier.select,
        found.judged.len(),
        tier.roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        found.duplicates.len(),
        found.private_tmp,
    ));
    let misconfigured: Vec<&Skip> = found
        .refused
        .iter()
        .filter(|skip| is_misconfigured(tier, skip))
        .collect();
    for skip in &misconfigured {
        record_error(report, log, run_error(tier, excluded(skip)));
    }
    for skip in found.refused.iter().filter(|s| !is_misconfigured(tier, s)) {
        log_skip(log, tier, skip);
        planned.skipped.push(plan_entry(
            tier,
            &skip.path,
            Some(skip.reason.as_str()),
            None,
        ));
    }
    if !misconfigured.is_empty() {
        return planned;
    }
    for (path, source_evidence, verdict) in &found.judged {
        match verdict {
            Ok(e) => {
                log(&format!(
                    "{} {}: eligible, {} entries, {} (newest change {})",
                    tier.name,
                    path.display(),
                    e.entries,
                    format_bytes(e.bytes),
                    e.newest_change
                        .map_or_else(|| "unknown".to_owned(), |t| t.secs.to_string())
                ));
                let mut entry = plan_entry(tier, path, None, Some(e.bytes));
                if let (Some(source), Some(newest)) = (&e.source_evidence, e.newest_change) {
                    let now = host.wall_clock();
                    let age_ns = (i128::from(now.secs) - i128::from(newest.secs)) * 1_000_000_000
                        + i128::from(now.nanos)
                        - i128::from(newest.nanos);
                    entry.build_evidence = Some(BuildEvidence {
                        marker: source.marker,
                        source: source.source.clone(),
                        idle_age_seconds: Some(
                            u64::try_from(age_ns.max(0) / 1_000_000_000).unwrap_or(u64::MAX),
                        ),
                    });
                }
                planned.eligible.push((e.newest_change, entry));
            }
            Err(skip) => {
                log(&format!("{} {}: skip {skip}", tier.name, path.display()));
                let mut entry = plan_entry(tier, path, Some(skip.reason.as_str()), None);
                if let Some(source) = source_evidence {
                    entry.build_evidence = Some(BuildEvidence {
                        marker: source.marker,
                        source: source.source.clone(),
                        idle_age_seconds: None,
                    });
                }
                planned.skipped.push(entry);
            }
        }
    }
    planned
}

/// One inode found by one or more positive filesystem sources. Retaining
/// every contributing `Found` keeps each root and source predicate available
/// for the lock-time recheck. Discovery order chooses the report path.
struct FsUnionCandidate {
    identity: Identity,
    found_by: Vec<(usize, fs_kind::Found)>,
}

fn discover_fs_union<H: Host>(
    host: &H,
    sources: &[FsUnionSourceRef<'_>],
    deadline: Option<MonoTime>,
) -> (Vec<FsUnionCandidate>, Vec<(usize, Skip)>, Option<Stop>) {
    let mut candidates: Vec<FsUnionCandidate> = Vec::new();
    let mut by_inode: HashMap<DevIno, usize> = HashMap::new();
    let mut refused = Vec::new();
    for (source_index, source) in sources.iter().enumerate() {
        let (discovery, stopped) = fs_kind::discover(host, source.roots, source.find, deadline);
        refused.extend(
            discovery
                .refused
                .into_iter()
                .map(|skip| (source_index, skip)),
        );
        for (path, found) in discovery.candidates {
            let found = match found {
                Ok(found) => found,
                Err(skip) => {
                    refused.push((source_index, skip));
                    continue;
                }
            };
            if let Some(&index) = by_inode.get(&found.identity.id) {
                if candidates[index].identity != found.identity {
                    refused.push((
                        source_index,
                        Skip::new(
                            Reason::Changed,
                            path,
                            "the inode changed identity between source discoveries",
                        ),
                    ));
                    // Neither old nor new identity is safe to reclaim from
                    // this incomplete view of the union.
                    candidates[index].found_by.clear();
                } else if !candidates[index].found_by.is_empty() {
                    candidates[index].found_by.push((source_index, found));
                }
            } else {
                by_inode.insert(found.identity.id, candidates.len());
                candidates.push(FsUnionCandidate {
                    identity: found.identity,
                    found_by: vec![(source_index, found)],
                });
            }
        }
        if let Some(stop) = stopped {
            return (candidates, refused, Some(stop));
        }
    }
    candidates.retain(|candidate| !candidate.found_by.is_empty());
    (candidates, refused, None)
}

fn union_checkouts(sources: &[FsUnionSourceRef<'_>], candidate: &FsUnionCandidate) -> Checkouts {
    if candidate
        .found_by
        .iter()
        .any(|(index, _)| sources[*index].checkouts == Checkouts::Refuse)
    {
        Checkouts::Refuse
    } else {
        Checkouts::Allow
    }
}

fn union_checkout_path(
    candidate: &FsUnionCandidate,
    checkouts: Checkouts,
) -> Option<(usize, &Path, Skip)> {
    candidate.found_by.iter().find_map(|(index, found)| {
        checkout_metadata_below(checkouts, &found.root, &found.path)
            .map(|skip| (*index, found.path.as_path(), skip))
    })
}

#[allow(clippy::too_many_arguments)]
fn plan_fs_union<H: Host>(
    host: &H,
    tier: &Tier,
    dev: Dev,
    rules: &Rules<'_>,
    deadline: Option<MonoTime>,
    report: &mut Report,
    log: &mut dyn FnMut(&str),
) -> Planned {
    let sources = tier.fs_sources();
    let (candidates, refused, stopped) = discover_fs_union(host, &sources, deadline);
    let mut planned = Planned {
        eligible: Vec::new(),
        skipped: Vec::new(),
        stopped,
    };
    log(&format!(
        "{}: `{}`: {} distinct filesystem candidate(s) from {} source term(s)",
        tier.name,
        tier.select,
        candidates.len(),
        sources.len()
    ));
    if planned.stopped.is_some() {
        return planned;
    }
    let misconfigured: Vec<_> = refused
        .iter()
        .filter(|(index, skip)| is_misconfigured_source(&sources[*index], skip))
        .collect();
    for (index, skip) in &misconfigured {
        let mut entry = run_error(tier, excluded(skip));
        entry.profile = sources[*index].profile.to_owned();
        record_error(report, log, entry);
    }
    for (index, skip) in refused
        .iter()
        .filter(|(index, skip)| !is_misconfigured_source(&sources[*index], skip))
    {
        let mut entry = plan_entry(tier, &skip.path, Some(skip.reason.as_str()), None);
        entry.profile = sources[*index].profile.to_owned();
        entry.checkouts = sources[*index].checkouts;
        planned.skipped.push(entry);
    }
    if !misconfigured.is_empty() {
        return planned;
    }
    let mut checker = Checker::for_plan(rules);
    for candidate in &candidates {
        let checkouts = union_checkouts(&sources, candidate);
        if let Some((index, path, skip)) = union_checkout_path(candidate, checkouts) {
            let mut entry = plan_entry(tier, path, Some(skip.reason.as_str()), None);
            entry.profile = sources[index].profile.to_owned();
            entry.checkouts = checkouts;
            planned.skipped.push(entry);
            continue;
        }
        let rules = Rules {
            checkouts,
            ..*rules
        };
        let mut first_skip = None;
        let mut selected = None;
        for (index, found) in &candidate.found_by {
            let source = &sources[*index];
            let judge = fs_kind::Judge {
                find: source.find,
                rules: &rules,
                idle: source.idle,
                dev,
            };
            match fs_kind::judge_one(host, &judge, found, &mut checker, deadline) {
                Ok(Ok(tally)) => {
                    selected = Some((*index, found.path.as_path(), tally));
                    break;
                }
                Ok(Err(skip)) => {
                    first_skip.get_or_insert((*index, found.path.as_path(), skip));
                }
                Err(stop) => {
                    planned.stopped = Some(stop);
                    return planned;
                }
            }
        }
        if let Some((index, path, tally)) = selected {
            let source = &sources[index];
            let mut entry = plan_entry(tier, path, None, Some(tally.bytes));
            entry.profile = source.profile.to_owned();
            entry.checkouts = checkouts;
            log(&format!(
                "{}: {}: eligible by `{}` ({} estimated)",
                tier.name,
                entry.candidate.display(),
                source.profile,
                format_bytes(tally.bytes)
            ));
            planned.eligible.push((tally.newest_change, entry));
        } else {
            let (index, path, skip) =
                first_skip.expect("a discovered union candidate has a verdict");
            log_skip(log, tier, &skip);
            let mut entry = plan_entry(tier, path, Some(skip.reason.as_str()), None);
            entry.profile = sources[index].profile.to_owned();
            entry.checkouts = checkouts;
            planned.skipped.push(entry);
        }
    }
    planned
}

/// The one set of walk rules of `tier`'s run: every entry one of
/// `owners`', any socket or FIFO refuses (the default policy), the default
/// entry budget ([`Rules::new`]), and the source profile's `checkouts`
/// setting. An `allow` is logged, so a run that may delete a checkout says
/// so where it runs (docs/safety.md, No source checkout).
fn rules<'o>(tier: &Tier, owners: &'o [u32], log: &mut dyn FnMut(&str)) -> Rules<'o> {
    if !tier.fs_union.is_empty() {
        for source in tier
            .fs_sources()
            .into_iter()
            .filter(|source| source.checkouts == Checkouts::Allow)
        {
            log(&format!(
                "{}: profile `{}` says `checkouts: allow`; in a union, a candidate also found by any refusing source uses `checkouts: refuse`",
                tier.name, source.profile
            ));
        }
    } else if tier.checkouts == Checkouts::Allow {
        log(&format!(
            "{}: profile `{}` says `checkouts: allow`: a candidate holding .git, .hg or .svn, or \
             inside a checkout's metadata, is not refused for that",
            tier.name, tier.profile
        ));
    }
    Rules {
        checkouts: tier.checkouts,
        ..Rules::new(owners)
    }
}

/// The final sample of a run: `None` (unknown: never met) when it fails,
/// which is a logged run error.
fn final_sample<H: Host>(
    host: &H,
    measured: &Measured<H::Fd>,
    report: &mut Report,
    log: &mut dyn FnMut(&str),
    tier: &Tier,
) -> Option<u64> {
    match sample(host, measured) {
        Ok(space) => Some(space.avail_bytes),
        Err(e) => {
            record_error(report, log, run_error(tier, e));
            None
        }
    }
}

/// A fresh sample of the requested filesystem.
fn sample<H: Host>(host: &H, measured: &Measured<H::Fd>) -> Result<Space, String> {
    host.statvfs(&measured.at.fd)
        .map_err(|e| format!("sampling available space: {e}"))
}

/// Discovery of the tier's targets, logged.
fn discover<H: Host>(
    host: &H,
    tier: &Tier,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
) -> Result<Discovery, Stop> {
    // Only a Cargo tier discovers Cargo targets: an fs tier's roots are
    // where its patterns look, never a place to look for targets (and
    // reclaim them). No `_` arm: a new source kind must decide here.
    match &tier.source {
        SourceKind::CargoTarget => {}
        SourceKind::Fs(_) => {
            return Err(Stop::Failed {
                path: tier.roots.first().cloned().unwrap_or_default(),
                error: HostError::invalid(
                    "discover",
                    &tier.profile,
                    "kind fs: Cargo discovery and reclaim run only for a cargo-target tier",
                ),
            });
        }
        SourceKind::DockerBuildCache | SourceKind::DockerImages => {
            return Err(Stop::Failed {
                path: PathBuf::new(),
                error: HostError::invalid(
                    "discover",
                    &tier.profile,
                    "Docker tiers do not use Cargo discovery",
                ),
            });
        }
    }
    let found = cargo_target::discover(host, &tier.roots, deadline)?;
    log(&format!(
        "{}: `{}`: {} target(s) below {}",
        tier.name,
        tier.select,
        found.targets.len(),
        tier.roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for skip in &found.refused {
        // A misconfigured root is reported (and logged) once, as an error.
        if !is_misconfigured(tier, skip) {
            log_skip(log, tier, skip);
        }
    }
    for path in &found.duplicates {
        log(&format!(
            "{}: {} is a target found through another root too: listed once",
            tier.name,
            path.display()
        ));
    }
    Ok(found)
}

/// The tier's roots discovery refused as `unsafe_path` (a symlink or a
/// mount crossing on the way): a misconfiguration, not a normal skip
/// (docs/safety.md, Validation), which excludes the tier: it is reported
/// as an error (exit 3 when the goal is not met) and nothing of the tier
/// is reclaimed.
fn misconfigured_roots<'f>(tier: &Tier, found: &'f Discovery) -> Vec<&'f Skip> {
    found
        .refused
        .iter()
        .filter(|skip| is_misconfigured(tier, skip))
        .collect()
}

fn is_misconfigured(tier: &Tier, skip: &Skip) -> bool {
    skip.reason == Reason::UnsafePath
        && (tier.roots.contains(&skip.path)
            || tier
                .fs_union
                .iter()
                .any(|term| term.roots.contains(&skip.path)))
}

fn is_misconfigured_source(source: &FsUnionSourceRef<'_>, skip: &Skip) -> bool {
    skip.reason == Reason::UnsafePath && source.roots.contains(&skip.path)
}

/// The error message of a misconfigured root (the detail without the
/// reason and path it repeats).
fn excluded(skip: &Skip) -> String {
    let repeated = format!("{}: {}: ", skip.reason, skip.path.display());
    let why = skip
        .detail
        .strip_prefix(repeated.as_str())
        .unwrap_or(&skip.detail);
    format!(
        "root {}: {} (misconfigured, or the mounts changed): {why}; the tier is excluded",
        skip.path.display(),
        skip.reason
    )
}

/// `other_fs` for a target on another filesystem than `PATH`'s: deleting
/// it would free nothing there (docs/architecture.md, Filesystem
/// matching). By device: a bind mount of the same filesystem frees the
/// same space.
fn other_fs(target: &Target, dev: Dev) -> Option<Skip> {
    (target.identity.id.dev != dev).then(|| {
        Skip::new(
            Reason::OtherFs,
            target.path.clone(),
            format!(
                "on device {}, not on PATH's filesystem (device {dev})",
                target.identity.id.dev
            ),
        )
    })
}

fn log_skip(log: &mut dyn FnMut(&str), tier: &Tier, skip: &Skip) {
    log(&format!("{}: skip {skip}", tier.name));
}

/// An error of the tier or the run (no one candidate).
fn run_error(tier: &Tier, message: impl Into<String>) -> ErrorEntry {
    ErrorEntry {
        tier: tier.name.clone(),
        profile: tier.profile.clone(),
        candidate: String::new(),
        message: message.into(),
    }
}

/// `held_by_deleted_files` (docs/reference.md, Output) from the run's
/// latest process scan; if the run made none, from one made now.
fn held<H: Host>(
    host: &H,
    checker: &mut Checker<'_>,
    dev: Dev,
    log: &mut dyn FnMut(&str),
) -> Vec<HeldEntry> {
    if checker.latest().is_none()
        && let Err(e) = checker.current(host)
    {
        log(&format!(
            "held_by_deleted_files: the process scan failed, the list is empty: {e}"
        ));
        return Vec::new();
    }
    let Some(scan) = checker.latest() else {
        return Vec::new();
    };
    for note in scan.notes() {
        log(&format!("process scan: {note}"));
    }
    scan.held_by_deleted_files(dev)
        .into_iter()
        .map(|h| HeldEntry {
            pid: h.pid.0,
            command: h.command.to_string_lossy().into_owned(),
            bytes: h.bytes,
        })
        .collect()
}

/// One reclaim of the tier, under the host lock.
struct Reclaim<'a, 'l, H: Delete> {
    host: &'a H,
    tier: &'a Tier,
    measured: &'a Measured<H::Fd>,
    deadline: Option<MonoTime>,
    tally_budget: Option<Duration>,
    command_ready: Option<Ready>,
    report: &'a mut Report,
    log: &'a mut (dyn FnMut(&str) + 'l),
    deadline_hit: bool,
}

impl<H: Delete> Reclaim<'_, '_, H> {
    fn skip_docker(&mut self, candidate: &Path, reason: &'static str, detail: &str) {
        (self.log)(&format!(
            "{}: skip Docker backend: {reason}: {detail}",
            self.tier.name
        ));
        self.report.skipped.push(SkipEntry {
            tier: self.tier.name.clone(),
            profile: self.tier.profile.clone(),
            candidate: candidate.to_owned(),
            count: 1,
            examples: Vec::new(),
            reason,
        });
    }

    fn walk_docker(&mut self) {
        let bound = match docker_backend::inspect_backend(
            self.host,
            self.measured.dev,
            docker_storage(self.tier),
        ) {
            Ok(Binding::Bound(bound)) => bound,
            Ok(Binding::Skipped(skip)) => {
                self.skip_docker(Path::new(""), skip.reason, &skip.detail);
                return;
            }
            Err(error) => {
                self.run_error(format!("bind Docker backend: {error}"));
                return;
            }
        };
        let candidate = docker_candidate(self.tier, &bound);
        let before = match sample(self.host, self.measured) {
            Ok(space) => space.avail_bytes,
            Err(error) => {
                self.run_error(error);
                return;
            }
        };
        if self.goal_reached(before) {
            return;
        }
        if self.past_deadline() {
            self.deadline_hit = true;
            return;
        }
        // The daemon and its storage can change while the host lock is held;
        // compare the full binding, including Docker's server ID, just before
        // the deleting child is started.
        match docker_backend::inspect_backend(
            self.host,
            self.measured.dev,
            docker_storage(self.tier),
        ) {
            Ok(Binding::Bound(rebound)) if rebound == bound => {}
            Ok(Binding::Bound(_)) => {
                self.skip_docker(
                    &candidate,
                    "changed",
                    "Docker daemon, context, builder or storage changed before prune",
                );
                return;
            }
            Ok(Binding::Skipped(skip)) => {
                self.skip_docker(&candidate, skip.reason, &skip.detail);
                return;
            }
            Err(error) => {
                self.run_error(format!("recheck Docker backend: {error}"));
                return;
            }
        }
        let cwd = match resolve(self.host, Path::new("/"), Want::Directory) {
            Ok(cwd) => cwd,
            Err(error) => {
                self.run_error(format!("open Docker command cwd: {error}"));
                return;
            }
        };
        let mut arguments: Vec<OsString> = vec!["--host=unix:///var/run/docker.sock".into()];
        match &self.tier.action {
            Action::DockerBuilderPrune => {
                arguments.extend(["builder", "prune", "--all", "--force"].map(OsString::from));
                if let Some(idle) = self.tier.idle_for {
                    arguments.push("--filter".into());
                    arguments.push(format!("until={}s", idle.0.as_secs()).into());
                }
            }
            Action::DockerImagePrune { all } => {
                arguments.extend(["image", "prune", "--force"].map(OsString::from));
                if *all {
                    arguments.push("--all".into());
                }
            }
            _ => unreachable!("Docker tier with non-Docker action admitted by ensure"),
        }
        let ready = self
            .command_ready
            .as_ref()
            .expect("Docker executable preflighted");
        let run = CommandRun {
            executable: ready.program.clone(),
            arguments,
            environment: vec![
                ("DOCKER_HOST".into(), "unix:///var/run/docker.sock".into()),
                ("DOCKER_CONTEXT".into(), "default".into()),
                // Docker CLI 29 forwards `docker builder` to Buildx unless
                // DOCKER_BUILDKIT=0 selects the Engine command. The latter
                // calls BuildCachePrune on the pinned daemon directly.
                ("DOCKER_BUILDKIT".into(), "0".into()),
            ],
            docker_info: Some(bound.info.clone()),
            output_prefix: format!("{}/{}", self.tier.name, self.tier.profile),
            deadline: self.deadline.unwrap_or(MonoTime(Duration::MAX)),
        };
        let started = self.host.now();
        let invocation = self.host.run_command(&cwd.fd, &run);
        let attempted = matches!(invocation, CommandInvocation::Started(_));
        let end = match invocation {
            CommandInvocation::NotStarted(error) if error.cause == Cause::SandboxGate => {
                ReclaimEnd::GateRefused {
                    path: candidate.clone(),
                    error,
                }
            }
            CommandInvocation::NotStarted(error) => ReclaimEnd::Failed {
                path: candidate.clone(),
                error,
            },
            CommandInvocation::DeadlineBeforeStart => ReclaimEnd::Stopped(Stop::DeadlinePassed {
                path: candidate.clone(),
            }),
            CommandInvocation::Started(Ok(CommandExit::Exited(0))) => ReclaimEnd::Complete,
            CommandInvocation::Started(Ok(CommandExit::Exited(code))) => ReclaimEnd::Failed {
                path: candidate.clone(),
                error: HostError::invalid(
                    "Docker prune",
                    candidate.display(),
                    format!("Docker client exited {code}"),
                ),
            },
            CommandInvocation::Started(Ok(CommandExit::Signaled(signal))) => ReclaimEnd::Failed {
                path: candidate.clone(),
                error: HostError::invalid(
                    "Docker prune",
                    candidate.display(),
                    format!("Docker client died from signal {signal}"),
                ),
            },
            CommandInvocation::Started(Ok(CommandExit::TimedOut)) => {
                ReclaimEnd::Stopped(Stop::DeadlinePassed {
                    path: candidate.clone(),
                })
            }
            CommandInvocation::Started(Err(error)) => ReclaimEnd::Failed {
                path: candidate.clone(),
                error,
            },
        };
        let _ = self.record(
            &candidate,
            Outcome {
                started,
                before,
                removed_entries: 0,
                attempted_command: attempted,
                end,
                note: String::new(),
            },
        );
    }

    fn skip(&mut self, candidate: &Path, skip: &Skip) {
        let profile = self.tier.profile.clone();
        self.skip_as(candidate, &profile, skip);
    }

    fn skip_as(&mut self, candidate: &Path, profile: &str, skip: &Skip) {
        log_skip(self.log, self.tier, skip);
        self.report.skipped.push(SkipEntry {
            tier: self.tier.name.clone(),
            profile: profile.to_owned(),
            candidate: candidate.to_owned(),
            count: 1,
            examples: Vec::new(),
            reason: skip.reason.as_str(),
        });
    }

    fn error_as(&mut self, candidate: &Path, profile: &str, message: String) {
        let entry = ErrorEntry {
            tier: self.tier.name.clone(),
            profile: profile.to_owned(),
            candidate: candidate.to_string_lossy().into_owned(),
            message,
        };
        record_error(self.report, self.log, entry);
    }

    /// An error of the run (no one candidate), logged.
    fn run_error(&mut self, message: String) {
        let entry = run_error(self.tier, message);
        record_error(self.report, self.log, entry);
    }

    /// An error of the run: recorded, and the run stops.
    fn stop(&mut self, stop: &Stop) {
        if matches!(stop, Stop::DeadlinePassed { .. }) {
            self.deadline_hit = true;
            (self.log)(&format!("{}: the deadline passed: {stop}", self.tier.name));
            return;
        }
        self.run_error(format!("the run stops: {stop}"));
    }

    fn past_deadline(&self) -> bool {
        self.deadline.is_some_and(|d| self.host.now() >= d)
    }

    fn run(&mut self, owners: &[u32]) {
        let rules = rules(self.tier, owners, self.log);
        let mut checker = Checker::new(&rules, REUSE_LIMIT);
        // No `_` arm: a new source kind must decide here (and in `ensure`'s
        // allowlist) how it is reclaimed. Each kind runs its own protocol
        // only: an fs tier never reaches the Cargo reclaim, a Cargo tier
        // never the fs one.
        match &self.tier.source {
            SourceKind::CargoTarget => self.walk_cargo(&rules, &mut checker),
            SourceKind::Fs(find) => {
                if self.tier.fs_union.is_empty() {
                    self.walk_fs(find, &rules, &mut checker);
                } else {
                    self.walk_fs_union(&rules, &mut checker);
                }
            }
            SourceKind::DockerBuildCache | SourceKind::DockerImages => self.walk_docker(),
        }
        self.report.held_by_deleted_files =
            held(self.host, &mut checker, self.measured.dev, self.log);
    }

    /// The end of the tally: the deadline and `--tally-budget`, whichever
    /// comes first.
    fn tally_until(&self) -> Option<MonoTime> {
        let budget_end = self
            .tally_budget
            .and_then(|b| self.host.now().checked_add(b));
        match (self.deadline, budget_end) {
            (Some(deadline), Some(budget)) => Some(deadline.min(budget)),
            (deadline, None) => deadline,
            (None, budget) => budget,
        }
    }

    /// Discovery's refusals of roots and directories, as skips; a
    /// misconfigured root (`unsafe_path`) is an error that excludes the
    /// tier: `false` then.
    fn refusals(&mut self, refused: &[Skip]) -> bool {
        let misconfigured: Vec<&Skip> = refused
            .iter()
            .filter(|skip| is_misconfigured(self.tier, skip))
            .collect();
        for skip in refused.iter().filter(|s| !is_misconfigured(self.tier, s)) {
            self.report.skipped.push(SkipEntry {
                tier: self.tier.name.clone(),
                profile: self.tier.profile.clone(),
                candidate: skip.path.clone(),
                count: 1,
                examples: Vec::new(),
                reason: skip.reason.as_str(),
            });
        }
        for skip in &misconfigured {
            self.run_error(excluded(skip));
        }
        misconfigured.is_empty()
    }

    /// The goal is observed: logged, `true`.
    fn goal_reached(&mut self, avail: u64) -> bool {
        if avail < self.measured.target {
            return false;
        }
        (self.log)(&format!(
            "{}: {} available, the goal (--target-free {}) is reached",
            self.tier.name,
            format_bytes(avail),
            format_bytes(self.measured.target)
        ));
        true
    }

    fn walk_cargo(&mut self, rules: &Rules<'_>, checker: &mut Checker<'_>) {
        let found = match discover(self.host, self.tier, self.deadline, self.log) {
            Ok(found) => found,
            Err(stop) => return self.stop(&stop),
        };
        if !self.refusals(&found.refused) {
            return;
        }
        let tally_until = self.tally_until();
        // Two groups, in this order (docs/reference.md, `--tally-budget`):
        // the tallied targets, oldest first, then those the budget left
        // untallied, in the order found (judged in full under their locks).
        // A target the tally refuses is skipped for this run and never
        // locked ([`cargo_target::tally`]).
        let mut tallied: Vec<(&Target, Tally)> = Vec::new();
        let mut untallied: Vec<&Target> = Vec::new();
        for target in &found.targets {
            if let Some(skip) = other_fs(target, self.measured.dev) {
                self.skip(&target.path, &skip);
                continue;
            }
            if !untallied.is_empty() {
                untallied.push(target);
                continue;
            }
            match cargo_target::tally(self.host, target, rules, tally_until) {
                Ok(Ok(tally)) => tallied.push((target, tally)),
                Ok(Err(skip)) => self.skip(&target.path, &skip),
                Err(Stop::DeadlinePassed { .. }) if !self.past_deadline() => {
                    self.budget_spent(&target.path);
                    untallied.push(target);
                }
                Err(stop) => return self.stop(&stop),
            }
        }
        oldest_first(&mut tallied, |(_, tally)| tally.last_build);
        let queue = tallied
            .into_iter()
            .map(|(target, tally)| (target, Some(tally)))
            .chain(untallied.into_iter().map(|t| (t, None)));
        let mut avail = match sample(self.host, self.measured) {
            Ok(space) => space.avail_bytes,
            Err(e) => {
                // Without a sample, the goal cannot be observed: reclaim
                // nothing.
                return self.run_error(e);
            }
        };
        for (target, tally) in queue {
            if self.goal_reached(avail) {
                return;
            }
            (self.log)(&match &tally {
                Some(tally) => format!(
                    "{}: reclaiming {} (est. {}, {} entries, last build {})",
                    self.tier.name,
                    target.path.display(),
                    format_bytes(tally.bytes),
                    tally.entries,
                    tally
                        .last_build
                        .map_or_else(|| "unknown".to_owned(), |t| t.secs.to_string())
                ),
                None => format!(
                    "{}: reclaiming {} (not tallied)",
                    self.tier.name,
                    target.path.display()
                ),
            });
            let started = self.host.now();
            // judged_last_build None: an accepted tally only orders; the
            // protocol's own recheck under the locks and the idle
            // requirement decide.
            let done = cargo_target::reclaim(
                self.host,
                target,
                None,
                rules,
                IdleWindow {
                    at_least: self.tier.idle_for,
                    before: self.tier.idle_before,
                },
                checker,
                self.deadline,
            );
            let held = format!(
                ", Cargo locks held {} ms",
                done.locks_held.map_or(0, |d| d.as_millis())
            );
            let outcome = Outcome {
                started,
                before: avail,
                removed_entries: done.removed_entries,
                attempted_command: false,
                end: done.end,
                note: held,
            };
            match self.record(&target.path, outcome) {
                Some(after) => avail = after,
                None => return,
            }
        }
    }

    /// The fs tier: discovery ([`fs_kind::discover`]), a read-only tally
    /// of each candidate for the `oldest` order within `--tally-budget`
    /// ([`fs_kind::tally`]), then the deletion protocol on each in that
    /// order ([`fs_kind::reclaim`], which judges every candidate again
    /// under its lock), re-sampling after each, until the goal is observed.
    fn walk_fs(&mut self, find: &Find, rules: &Rules<'_>, checker: &mut Checker<'_>) {
        let action = &self.tier.action;
        let judge = fs_kind::Judge {
            find,
            rules,
            idle: IdleWindow {
                at_least: self.tier.idle_for,
                before: self.tier.idle_before,
            },
            dev: self.measured.dev,
        };
        let (found, stopped) = fs_kind::discover(self.host, &self.tier.roots, find, self.deadline);
        (self.log)(&format!(
            "{}: `{}`: {} candidate(s) below {} ({} found again through another root or \
             pattern, {} systemd-private-* passed over)",
            self.tier.name,
            self.tier.select,
            found.candidates.len(),
            self.tier
                .roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            found.duplicates.len(),
            found.private_tmp,
        ));
        if let Some(stop) = stopped {
            // An incomplete discovery is not the tier's set: nothing is
            // reclaimed from it.
            return self.stop(&stop);
        }
        if !self.refusals(&found.refused) {
            return;
        }
        let tally_until = self.tally_until();
        let mut tallied: Vec<(&fs_kind::Found, fs_kind::Tally)> = Vec::new();
        let mut untallied: Vec<&fs_kind::Found> = Vec::new();
        for (path, candidate) in &found.candidates {
            let found = match candidate {
                Ok(found) => found,
                Err(skip) => {
                    self.skip(path, skip);
                    continue;
                }
            };
            if !untallied.is_empty() {
                untallied.push(found);
                continue;
            }
            match fs_kind::tally(self.host, &judge, found, tally_until) {
                Ok(Ok(tally)) => tallied.push((found, tally)),
                Ok(Err(skip)) => self.skip(path, &skip),
                Err(Stop::DeadlinePassed { .. }) if !self.past_deadline() => {
                    self.budget_spent(path);
                    untallied.push(found);
                }
                Err(stop) => return self.stop(&stop),
            }
        }
        oldest_first(&mut tallied, |(_, tally)| tally.newest_change);
        let queue = tallied
            .into_iter()
            .map(|(found, tally)| (found, Some(tally)))
            .chain(untallied.into_iter().map(|f| (f, None)));
        let mut avail = match sample(self.host, self.measured) {
            Ok(space) => space.avail_bytes,
            Err(e) => return self.run_error(e),
        };
        for (found, tally) in queue {
            if self.goal_reached(avail) {
                return;
            }
            if self.past_deadline() {
                self.deadline_hit = true;
                (self.log)(&format!(
                    "{}: the deadline passed before {}",
                    self.tier.name,
                    found.path.display()
                ));
                return;
            }
            (self.log)(&match &tally {
                Some(tally) => format!(
                    "{}: reclaiming {} (est. {}, {} entries, newest change {})",
                    self.tier.name,
                    found.path.display(),
                    format_bytes(tally.bytes),
                    tally.entries,
                    tally
                        .newest_change
                        .map_or_else(|| "unknown".to_owned(), |t| t.secs.to_string())
                ),
                None => format!(
                    "{}: reclaiming {} (not tallied)",
                    self.tier.name,
                    found.path.display()
                ),
            });
            let started = self.host.now();
            let outcome = match action {
                Action::Delete { make_writable } => {
                    let done = fs_kind::reclaim(
                        self.host,
                        &judge,
                        found,
                        *make_writable,
                        checker,
                        self.deadline,
                    );
                    Outcome {
                        started,
                        before: avail,
                        removed_entries: done.removed_entries,
                        attempted_command: false,
                        end: done.end,
                        note: String::new(),
                    }
                }
                Action::Command { argv, cwd } => {
                    let ready = self.command_ready.as_ref().expect("command preflighted");
                    let done = fs_kind::run_command(
                        self.host,
                        &judge,
                        found,
                        ready,
                        argv,
                        *cwd,
                        checker,
                        self.measured.target.saturating_sub(avail),
                        &self.measured.at.mount.mount_point,
                        &self.tier.profile,
                        format!("{}/{}", self.tier.name, self.tier.profile),
                        self.deadline,
                    );
                    Outcome {
                        started,
                        before: avail,
                        removed_entries: 0,
                        attempted_command: done.attempted,
                        end: done.end,
                        note: String::new(),
                    }
                }
                Action::CargoLockProtocol
                | Action::DockerBuilderPrune
                | Action::DockerImagePrune { .. } => unreachable!("fs action admitted by ensure"),
            };
            match self.record(&found.path, outcome) {
                Some(after) => avail = after,
                None => return,
            }
        }
    }

    /// Reclaim each identity once after the union's complete discovery.
    /// Terms remain attached to the candidate so their predicates can be
    /// tried again under the one candidate lock.
    fn walk_fs_union(&mut self, base_rules: &Rules<'_>, checker: &mut Checker<'_>) {
        let sources = self.tier.fs_sources();
        let (candidates, refused, stopped) = discover_fs_union(self.host, &sources, self.deadline);
        (self.log)(&format!(
            "{}: `{}`: {} distinct filesystem candidate(s) from {} source term(s)",
            self.tier.name,
            self.tier.select,
            candidates.len(),
            sources.len()
        ));
        if let Some(stop) = stopped {
            return self.stop(&stop);
        }
        let mut misconfigured = false;
        for (index, skip) in &refused {
            let source = &sources[*index];
            if is_misconfigured_source(source, skip) {
                let mut entry = run_error(self.tier, excluded(skip));
                entry.profile = source.profile.to_owned();
                record_error(self.report, self.log, entry);
                misconfigured = true;
            } else {
                self.skip_as(&skip.path, source.profile, skip);
            }
        }
        if misconfigured {
            return;
        }
        let tally_until = self.tally_until();
        let mut tallied: Vec<(&FsUnionCandidate, fs_kind::Tally)> = Vec::new();
        let mut untallied: Vec<&FsUnionCandidate> = Vec::new();
        for candidate in &candidates {
            if !untallied.is_empty() {
                untallied.push(candidate);
                continue;
            }
            let checkouts = union_checkouts(&sources, candidate);
            if let Some((index, path, skip)) = union_checkout_path(candidate, checkouts) {
                let profile = sources[index].profile;
                self.skip_as(path, profile, &skip);
                continue;
            }
            let rules = Rules {
                checkouts,
                ..*base_rules
            };
            let mut first_skip = None;
            let mut tally = None;
            let mut budget_exhausted = false;
            for (index, found) in &candidate.found_by {
                let source = &sources[*index];
                let judge = fs_kind::Judge {
                    find: source.find,
                    rules: &rules,
                    idle: source.idle,
                    dev: self.measured.dev,
                };
                match fs_kind::tally(self.host, &judge, found, tally_until) {
                    Ok(Ok(value)) => {
                        tally = Some(value);
                        break;
                    }
                    Ok(Err(skip)) => {
                        first_skip.get_or_insert((*index, found.path.as_path(), skip));
                    }
                    Err(Stop::DeadlinePassed { .. }) if !self.past_deadline() => {
                        self.budget_spent(&found.path);
                        untallied.push(candidate);
                        budget_exhausted = true;
                        break;
                    }
                    Err(stop) => return self.stop(&stop),
                }
            }
            if let Some(tally) = tally {
                tallied.push((candidate, tally));
            } else if !budget_exhausted {
                let (index, path, skip) =
                    first_skip.expect("a union candidate has a tally verdict");
                self.skip_as(path, sources[index].profile, &skip);
            }
        }
        oldest_first(&mut tallied, |(_, tally)| tally.newest_change);
        let queue = tallied
            .into_iter()
            .map(|(candidate, tally)| (candidate, Some(tally)))
            .chain(untallied.into_iter().map(|candidate| (candidate, None)));
        let mut avail = match sample(self.host, self.measured) {
            Ok(space) => space.avail_bytes,
            Err(error) => return self.run_error(error),
        };
        for (candidate, tally) in queue {
            if self.goal_reached(avail) {
                return;
            }
            let path = &candidate.found_by[0].1.path;
            if self.past_deadline() {
                self.deadline_hit = true;
                (self.log)(&format!(
                    "{}: deadline passed before {}",
                    self.tier.name,
                    path.display()
                ));
                return;
            }
            (self.log)(&format!(
                "{}: reclaiming {} ({}), found by {} source term(s)",
                self.tier.name,
                path.display(),
                tally
                    .as_ref()
                    .map_or_else(|| "not tallied".to_owned(), |t| format_bytes(t.bytes)),
                candidate.found_by.len(),
            ));
            let rules = Rules {
                checkouts: union_checkouts(&sources, candidate),
                ..*base_rules
            };
            let alternatives: Vec<_> = candidate
                .found_by
                .iter()
                .map(|(index, found)| {
                    let source = &sources[*index];
                    fs_kind::Alternative {
                        judge: fs_kind::Judge {
                            find: source.find,
                            rules: &rules,
                            idle: source.idle,
                            dev: self.measured.dev,
                        },
                        found,
                    }
                })
                .collect();
            let started = self.host.now();
            let Action::Delete { make_writable } = self.tier.action else {
                unreachable!("filesystem unions admit only delete")
            };
            let done = fs_kind::reclaim_alternatives(
                self.host,
                &alternatives,
                make_writable,
                checker,
                self.deadline,
            );
            let profile = done
                .selected
                .map(|selected| sources[candidate.found_by[selected].0].profile.to_owned())
                .unwrap_or_else(|| sources[candidate.found_by[0].0].profile.to_owned());
            let reported_path = done
                .selected
                .map(|selected| &candidate.found_by[selected].1.path)
                .unwrap_or(path);
            let outcome = Outcome {
                started,
                before: avail,
                removed_entries: done.removed_entries,
                attempted_command: false,
                end: done.end,
                note: String::new(),
            };
            match self.record_as(reported_path, &profile, outcome) {
                Some(after) => avail = after,
                None => return,
            }
        }
    }

    fn budget_spent(&mut self, at: &Path) {
        (self.log)(&format!(
            "{}: --tally-budget spent at {}: it and the candidates after it are taken after \
             the tallied ones, in the order found",
            self.tier.name,
            at.display()
        ));
    }

    /// Records the reclaim of `candidate` (any kind's protocol): samples
    /// the space after it, makes it an action when anything was deleted
    /// (with the observed change, `null` when unmeasured), and a skip or
    /// an error by how it ended. The available space after it, or `None`
    /// when the run must stop.
    fn record(&mut self, candidate: &Path, outcome: Outcome) -> Option<u64> {
        let profile = self.tier.profile.clone();
        self.record_as(candidate, &profile, outcome)
    }

    fn record_as(&mut self, candidate: &Path, profile: &str, outcome: Outcome) -> Option<u64> {
        let took = self.host.now().saturating_since(outcome.started);
        let after = match sample(self.host, self.measured) {
            Ok(space) => Some(space.avail_bytes),
            Err(e) => {
                // Unmeasured: the change is unknown (null), never 0, and
                // the goal can no longer be observed: the run stops below.
                self.run_error(e);
                None
            }
        };
        let delta = after
            .map(|a| i64::try_from(i128::from(a) - i128::from(outcome.before)).unwrap_or(i64::MAX));
        let complete = matches!(outcome.end, ReclaimEnd::Complete);
        if complete || outcome.removed_entries > 0 || outcome.attempted_command {
            if outcome.attempted_command {
                (self.log)(&format!(
                    "{}: {} {}: command finished with {:?}, observed change {}, {} ms",
                    self.tier.name,
                    profile,
                    candidate.display(),
                    outcome.end,
                    signed_bytes(delta),
                    took.as_millis()
                ));
            } else {
                (self.log)(&format!(
                    "{}: {} {}: {} {} entries, observed change {}, {} ms{}",
                    self.tier.name,
                    profile,
                    candidate.display(),
                    if complete {
                        "reclaimed"
                    } else {
                        "deleted part-way:"
                    },
                    outcome.removed_entries,
                    signed_bytes(delta),
                    took.as_millis(),
                    outcome.note
                ));
            }
            self.report.actions.push(ActionEntry {
                tier: self.tier.name.clone(),
                profile: profile.to_owned(),
                candidate: candidate.to_owned(),
                avail_delta_bytes: delta,
                duration_ms: u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
                count: 1,
            });
        }
        match outcome.end {
            ReclaimEnd::Complete => {}
            ReclaimEnd::Skipped(skip) => self.skip_as(candidate, profile, &skip),
            ReclaimEnd::Failed { path, error } => {
                // An error for this candidate only (an `ENOTEMPTY` from a
                // file created after its walk, ...): reported, never
                // retried in this run; the next candidate is still tried.
                self.error_as(candidate, profile, format!("{}: {error}", path.display()));
            }
            ReclaimEnd::GateRefused { path, error } => {
                self.error_as(candidate, profile, format!("{}: {error}", path.display()));
                (self.log)(&format!(
                    "{}: the sandbox gate refused: nothing more is deleted",
                    self.tier.name
                ));
                return None;
            }
            ReclaimEnd::Stopped(stop) => {
                self.stop(&stop);
                return None;
            }
        }
        after
    }
}

/// How one candidate's reclaim went, for [`Reclaim::record`].
struct Outcome {
    started: MonoTime,
    /// The available space before it.
    before: u64,
    removed_entries: usize,
    attempted_command: bool,
    end: ReclaimEnd,
    /// Appended to the action's log line.
    note: String,
}
