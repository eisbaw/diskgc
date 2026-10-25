//! The executor (docs/architecture.md, The reclaim walk): one selected tier
//! or configured ordered groups, with per-tier discovery and a shared host
//! lock. Ranking may use oldest, best-fit, or a seeded random order.
//! Deadline and signal handling use the [`Host`] clock and cancellation
//! state. Task-31's index is separate.
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
//!   first; `best-fit`: estimated adaptive order), eligible first.
//!
//! Both start with [`privileges`] and checked tiers built by the caller.
//!
//! Each [`ReclaimEnd`], whatever the kind, maps to the report as task-10's
//! note says (`Reclaim::record`): `Complete` is an action; `Skipped` a skip
//! (and an action too when entries were deleted part-way); `Failed` an
//! error of that candidate (the next one is still tried; an `fs`
//! candidate's `ENOTEMPTY` is never retried); `GateRefused` and
//! `Stopped(Failed)` errors of the run, which stop it;
//! `Stopped(DeadlinePassed)` the deadline path.

mod tier_policy;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cargo_target::{self, Discovery, ReclaimEnd, Stop, Tally, Target};
use crate::command_action::{self, Preflight, Ready};
use crate::config::{self, Order};
use crate::docker_backend::{self, Binding, Bound, Storage};
use crate::engine::{
    Checkouts, Identity, Reason, Rules, Skip, checkout_metadata_below, may_be_networked,
};
use crate::fs_kind::{self, EntryFilter, Find, NegatedMixedFilter};
use crate::host::{
    Cause, CommandExit, CommandInvocation, CommandRun, Delete, Dev, DevIno, Host, HostError,
    MonoTime, MountEntry, Space, Timestamp,
};
use crate::hostlock::{self, LockError};
use crate::inuse::{Checker, REUSE_LIMIT};
use crate::profile::{Action, Body, Finds, IdleFor, IdleWindow, Library, LivenessContract, Where};
use crate::report::{
    ActionEntry, BuildEvidence, ErrorCause, ErrorEntry, Final, HeldEntry, OverrunEntry, PlanEntry,
    Report, SkipEntry, Status, after_lock_timeout, conclude, human_space_values, signed_bytes,
};
use crate::resolve::{
    ResolveError, Resolved, Want, mount_for, resolve, resolve_in, screen_mount_chain,
};
use crate::select::{self, Factor};
use crate::units::{Size, format_age, format_bytes};
use tier_policy::{DeletePolicy, Protection};

fn signal_report<H: Host>(host: &H, mut report: Report) -> Report {
    // Usage errors precede a run. Once work has started, interruption wins
    // over the measured-space and deadline outcomes, including if a blocked
    // host call returned with an ordinary error after the handler ran.
    if report.status != Status::UsageError
        && let Some(signal) = host.interrupted()
    {
        report.status = Status::Interrupted(signal);
    }
    report
}

/// Measure an entire public run, including early returns and final signal
/// precedence. The wall clock is used only to label its start; elapsed time
/// comes from the host's monotonic clock.
fn timed_report<H: Host>(host: &H, run: impl FnOnce() -> Report) -> Report {
    let wall = host.wall_clock();
    let started = host.now();
    let mut report = run();
    report.started_at = i32::try_from(wall.nanos)
        .ok()
        .and_then(|nanos| jiff::Timestamp::new(wall.secs, nanos).ok())
        .map(|timestamp| timestamp.to_string());
    report.duration_ms = u64::try_from(host.now().saturating_since(started).as_millis()).ok();
    signal_report(host, report)
}

fn age_seconds(now: Timestamp, newest: Option<Timestamp>) -> Option<u64> {
    let newest = newest?;
    let nanos = (i128::from(now.secs) - i128::from(newest.secs)) * 1_000_000_000
        + i128::from(now.nanos)
        - i128::from(newest.nanos);
    (nanos >= 0)
        .then(|| u64::try_from(nanos / 1_000_000_000).ok())
        .flatten()
}

fn observed_age(now: Timestamp, newest: Option<Timestamp>) -> String {
    let Some(newest) = newest else {
        return "unknown".to_owned();
    };
    let Some(seconds) = age_seconds(now, Some(newest)) else {
        return "in the future".to_owned();
    };
    format!("{} ago", format_age(seconds))
}

/// Record the stage that was in progress when a cooperative deadline was
/// crossed. Its caller decides whether a final space sample is still needed.
fn record_deadline_overrun<H: Host>(
    host: &H,
    report: &mut Report,
    deadline: Option<MonoTime>,
    started: MonoTime,
    operation: &str,
    path: &Path,
) {
    let Some(deadline) = deadline else {
        return;
    };
    let ended = host.now();
    if started <= deadline && ended > deadline {
        report.overruns.push(OverrunEntry {
            operation: operation.to_owned(),
            path: path.to_owned(),
            over_ms: u64::try_from(ended.saturating_since(deadline).as_millis())
                .unwrap_or(u64::MAX),
        });
    }
}

fn deadline_passed<H: Host>(host: &H, deadline: Option<MonoTime>) -> bool {
    deadline.is_some_and(|end| host.now() >= end)
}

/// The name of the one group and tier `--select` makes, in the JSON and on
/// stderr alike (docs/reference.md, Commands). A word, not the flag: a
/// progress line `select: skip in_use ...` does not read as a complaint
/// about the command line (TASK-192). A configured group may also be called
/// `select`; a report never holds both, since `--select` replaces the
/// configuration's groups.
pub const SELECT_TIER: &str = "select";

/// One selected tier. Its ordinary fields hold the first source term;
/// [`Tier::fs_union`] holds any additional positive filesystem
/// terms. A same-profile Cargo union of positive idle filters is reduced
/// to its shortest bound. Built by [`Tier::select`] only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    pub group: String,
    pub name: String,
    /// Candidate order within this tier. An explicit `--select` uses oldest.
    pub order: Order,
    /// The expression, normalised (as `check` prints it).
    pub select: String,
    /// The source profile.
    pub profile: String,
    /// What the source finds, by kind.
    pub source: SourceKind,
    /// Where it looks: the bound roots, `{uid}` replaced.
    pub roots: Vec<PathBuf>,
    /// The term's idle requirement: the longest positive `filter:` bound
    /// and the source's mandatory `min_idle_for` floor, if present.
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
    /// Bound protected source matches, shared by every tier of a policy.
    pub protections: Vec<ProtectedSource>,
    /// Raw source matches excluded from this term by overlap.
    pub negated: Vec<NegatedSource>,
    /// The eligibility walk's entry budget per candidate (`too_many_entries`
    /// beyond it): always [`crate::engine::DEFAULT_MAX_ENTRIES`] in the
    /// binary. Only the model tests lower it, through
    /// [`crate::invocation::EnsureOptions::test_entry_budget`], so a vector
    /// can show the refusal with a small tree.
    pub max_entries: usize,
}

/// A protected source's raw find, with its own `where` deliberately omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedSource {
    pub profile: String,
    pub roots: Vec<PathBuf>,
    pub kind: ProtectedKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtectedKind {
    Fs(Box<Find>),
    CargoTarget,
}

/// Bind all protection roots before planning or reclaim. Root mode must not
/// silently expand a protected `{uid}` to uid 0.
pub fn bind_protections(
    config: &config::Config,
    uid: u32,
    allow_root: bool,
) -> Result<Vec<ProtectedSource>, String> {
    config.protect.iter().map(|entry| {
        let Body::Source(source) = &config.library.get(&entry.profile)
            .ok_or_else(|| format!("protect: unknown profile `{}`", entry.profile))?.body else {
            return Err(format!("protect: `{}` is not a source", entry.profile));
        };
        if allow_root {
            let uses_uid = entry.roots.iter().any(|root| root.contains("{uid}"))
                || matches!(&source.finds, Finds::Paths { patterns, exclude, .. }
                    if patterns.iter().chain(exclude).any(|path| path.contains("{uid}")));
            if uses_uid {
                return Err(format!("--allow-root refuses `{{uid}}` in protected profile `{}`: it would expand to root (uid 0), not to an owners: entry", entry.profile));
            }
        }
        let kind = match &source.finds {
            Finds::Paths { patterns, exclude, entry_type } => ProtectedKind::Fs(Box::new(
                Find::with_excludes(patterns, exclude, *entry_type, &Where::default(), uid),
            )),
            Finds::CargoTargets => ProtectedKind::CargoTarget,
            _ => return Err(format!("protect: `{}` has an unsupported kind", entry.profile)),
        };
        Ok(ProtectedSource {
            profile: entry.profile.clone(),
            roots: entry.roots.iter().map(|root| PathBuf::from(root.replace("{uid}", &uid.to_string()))).collect(),
            kind,
        })
    }).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegatedSource {
    pub profile: String,
    pub source: NegatedKind,
    pub roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegatedKind {
    Fs(Box<Find>),
    CargoTarget,
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
    pub negated: Vec<NegatedSource>,
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
    negated: &'a [NegatedSource],
}

impl Tier {
    /// The action, as the report names it (`operation`, human-readable).
    pub fn operation(&self) -> String {
        self.action.to_string()
    }

    /// Whether a source of this tier (its own or a union term) has a
    /// liveness contract: its in-use scans then also read what Owner PID
    /// needs (one more `status` read per process, docs/safety.md, Cost).
    pub fn pid_lookup(&self) -> bool {
        let own = matches!(&self.source, SourceKind::Fs(find) if find.liveness_contract.is_some());
        own || self
            .fs_union
            .iter()
            .any(|term| term.find.liveness_contract.is_some())
    }

    /// Whether a source of this tier (its own or a union term) has
    /// `liveness_contract: pytest`: its in-use scans then also read that
    /// contract's process facts (docs/safety.md, Producer liveness, Cost).
    pub fn pytest_facts(&self) -> bool {
        let pytest = |find: &Find| find.liveness_contract == Some(LivenessContract::Pytest);
        let own = matches!(&self.source, SourceKind::Fs(find) if pytest(find));
        own || self.fs_union.iter().any(|term| pytest(&term.find))
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
            negated: &self.negated,
        }];
        sources.extend(self.fs_union.iter().map(|source| FsUnionSourceRef {
            profile: &source.profile,
            find: &source.find,
            roots: &source.roots,
            idle: source.idle,
            checkouts: source.checkouts,
            negated: &source.negated,
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

/// An explicit caller selection failed before it became a runnable tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionFailure {
    Parse(String),
    Usage(String),
}

impl SelectionFailure {
    pub fn into_message(self) -> String {
        match self {
            Self::Parse(message) | Self::Usage(message) => message,
        }
    }
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
        Self::select_with_warnings(expr, library, roots, uid)
            .map(|(tier, _)| tier)
            .map_err(SelectionFailure::into_message)
    }

    /// Like `select`, returning the parser's nonfatal redundancy warnings.
    pub fn select_with_warnings(
        expr: &str,
        library: &Library,
        roots: Option<&[String]>,
        uid: u32,
    ) -> Result<(Tier, Vec<String>), SelectionFailure> {
        let checked = select::compile(expr, library)
            .map_err(|error| SelectionFailure::Parse(error.to_string()))?;
        let bound = library.bind_roots(&checked, roots).map_err(|error| {
            let hint = if matches!(&error, crate::profile::RootsError::Missing { .. }) {
                "; add --roots DIR"
            } else {
                ""
            };
            SelectionFailure::Usage(format!("--select {expr:?}: {error}{hint}"))
        })?;
        let tier = Self::from_checked(expr, &checked, library, &bound, uid)
            .map_err(SelectionFailure::Usage)?;
        Ok((tier, checked.warnings().to_vec()))
    }

    /// Compile a statically checked config tier using its already-bound
    /// roots. Rebinding would lose the parser's decision about fixed and
    /// required roots, especially when several sources form a union.
    pub fn configured(
        group: &str,
        tier: &config::Tier,
        library: &Library,
        uid: u32,
    ) -> Result<Self, String> {
        let expr = tier.select.expr().to_string();
        let mut selected = Self::from_checked(&expr, &tier.select, library, &tier.roots, uid)
            .map_err(|e| format!("group `{group}` tier `{}`: {e}", tier.name))?;
        selected.group = group.to_owned();
        selected.name = tier.name.clone();
        selected.order = tier.order;
        Ok(selected)
    }

    fn from_checked(
        expr: &str,
        checked: &select::Checked,
        library: &Library,
        bound: &std::collections::BTreeMap<String, Vec<String>>,
        uid: u32,
    ) -> Result<Self, String> {
        let diagnostic_expr = format!("{expr:?}");
        let not_yet = |what: &str| {
            format!(
                "--select {diagnostic_expr}: {what} is not supported by ensure in this version \
                 yet; select one source profile (`profile:NAME`, kind cargo-target, or \
                 kind fs), optionally with supported filters, a positive fs union with one action, or a \
                 same-source Cargo union with only positive idle filters"
            )
        };
        let mut terms = Vec::new();
        for term in &checked.expr().terms {
            terms.push(Self::select_term(
                expr, term, library, bound, uid, &not_yet,
            )?);
        }
        let mut first = terms.remove(0);
        if !matches!(
            first.action,
            Action::Delete { .. } | Action::Command { .. } | Action::CargoLockProtocol
        ) && (!first.negated.is_empty() || terms.iter().any(|term| !term.negated.is_empty()))
        {
            return Err(not_yet("!profile: with this source action"));
        }
        if !terms.is_empty() {
            // For one Cargo source, positive idle filters are lower age
            // bounds on the same candidates. Their union is the weakest
            // bound, including the source's mandatory minimum when one
            // member has no tier filter. Keep the
            // existing Cargo discovery and lock-time protocol as the one
            // authority for the reduced selection.
            if matches!(first.source, SourceKind::CargoTarget)
                && checked.expr().terms.iter().all(|term| {
                    term.source == first.profile
                        && term
                            .factors
                            .iter()
                            .all(|factor| matches!(factor, select::Factor::Filter(_)))
                })
            {
                first.idle_for = terms
                    .iter()
                    .fold(first.idle_for, |bound, term| bound.min(term.idle_for));
                first.select = checked.expr().to_string();
                return Ok(first);
            }
            if !matches!(first.source, SourceKind::Fs(_))
                || !matches!(first.action, Action::Delete { .. } | Action::Command { .. })
            {
                return Err(not_yet("a union of this kind or action"));
            }
            if terms.iter().any(|term| term.action != first.action) {
                return Err(format!(
                    "--select {diagnostic_expr}: filesystem union sources must have exactly the same action and options"
                ));
            }
            let mut extra = Vec::with_capacity(terms.len());
            for tier in &terms {
                let SourceKind::Fs(find) = &tier.source else {
                    return Err(not_yet("a union of non-filesystem sources"));
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
                    negated: tier.negated.clone(),
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
        not_yet: &impl Fn(&str) -> String,
    ) -> Result<Tier, String> {
        let diagnostic_expr = format!("{expr:?}");
        let Some(Body::Source(source)) = library.get(&term.source).map(|p| &p.body) else {
            return Err(format!(
                "--select {diagnostic_expr}: no source profile `{}`",
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
        let mut negated = Vec::new();
        for factor in &term.factors {
            let (name, is_negated) = match factor {
                Factor::Filter(name) => (name, false),
                Factor::NotFilter(name) => (name, true),
                Factor::NotProfile(name) => {
                    let Some(Body::Source(other)) = library.get(name).map(|p| &p.body) else {
                        return Err(format!(
                            "--select {diagnostic_expr}: no negated source profile `{name}`"
                        ));
                    };
                    let source = match &other.finds {
                        Finds::Paths {
                            patterns,
                            exclude,
                            entry_type,
                        } => NegatedKind::Fs(Box::new(Find::with_excludes(
                            patterns,
                            exclude,
                            *entry_type,
                            &crate::profile::Where::default(),
                            uid,
                        ))),
                        Finds::CargoTargets => NegatedKind::CargoTarget,
                        _ => return Err(not_yet(&format!("`!profile:{name}`"))),
                    };
                    let roots = bound
                        .get(name)
                        .ok_or_else(|| {
                            format!(
                                "--select {diagnostic_expr}: no bound roots for `!profile:{name}`"
                            )
                        })?
                        .iter()
                        .map(|root| PathBuf::from(root.replace("{uid}", &uid.to_string())))
                        .collect();
                    negated.push(NegatedSource {
                        profile: name.clone(),
                        source,
                        roots,
                    });
                    continue;
                }
            };
            let Some(Body::Filter(where_)) = library.get(name).map(|p| &p.body) else {
                return Err(format!(
                    "--select {diagnostic_expr}: no filter profile `{name}`"
                ));
            };
            if is_negated && where_.predicates() == [crate::kind::PredicateName::IdleFor] {
                let bound = where_.idle_for.expect("idle predicate present");
                idle_before = Some(idle_before.map_or(bound, |old| old.min(bound)));
                continue;
            }
            // A positive filter is a conjunction: its entry checks, idle
            // requirement and version guards judge the same candidate.
            // A negated mixed filter complements the entire conjunction,
            // using the walk's newest-change measure under the lock.
            let entry_count =
                usize::from(where_.has.is_some()) + usize::from(where_.has_not.is_some());
            let entry_and_idle_only = where_.predicates().iter().all(|predicate| {
                matches!(
                    predicate,
                    crate::kind::PredicateName::Has
                        | crate::kind::PredicateName::HasNot
                        | crate::kind::PredicateName::IdleFor
                )
            });
            if matches!(kind, SourceKind::Fs(_))
                && entry_count > 0
                && (!is_negated || entry_and_idle_only)
            {
                let SourceKind::Fs(find) = &mut kind else {
                    unreachable!()
                };
                if is_negated && (entry_count > 1 || where_.idle_for.is_some()) {
                    find.negated_mixed_filters.push(NegatedMixedFilter {
                        name: name.clone(),
                        has: where_.has.clone(),
                        has_not: where_.has_not.clone(),
                        idle_for: where_.idle_for,
                    });
                    continue;
                }
                for (entry, has) in [(&where_.has, true), (&where_.has_not, false)] {
                    if let Some(entry) = entry {
                        find.entry_filters.push(EntryFilter {
                            name: name.clone(),
                            entry: entry.clone(),
                            has,
                            negated: is_negated,
                        });
                    }
                }
                // Entry checks narrow this term after discovery; version
                // guards are independent retention checks. A filter can
                // specify both, so neither may disappear when compiling it.
                if !is_negated {
                    find.apply_version_filter(where_).map_err(|error| {
                        format!("--select {diagnostic_expr}: filter:{name}: {error}")
                    })?;
                }
                continue;
            }
            if is_negated {
                return Err(not_yet(&format!(
                    "`!filter:{name}` (unsupported predicates)"
                )));
            }
            if where_.has.is_some() || where_.has_dir.is_some() || where_.has_not.is_some() {
                return Err(not_yet(&format!(
                    "`filter:{name}` (a `has` / `has_dir` / `has_not` filter)"
                )));
            }
            if let SourceKind::Fs(find) = &mut kind {
                find.apply_version_filter(where_).map_err(|error| {
                    format!("--select {diagnostic_expr}: filter:{name}: {error}")
                })?;
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
            order: Order::Oldest,
            select: term.to_string(),
            profile: term.source.clone(),
            source: kind,
            roots,
            idle_for,
            idle_before,
            checkouts: source.checkouts,
            action: source.action.clone(),
            fs_union: Vec::new(),
            protections: Vec::new(),
            negated,
            max_entries: crate::engine::DEFAULT_MAX_ENTRIES,
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

impl Request {
    /// The report a run of this request starts from: its path and the goal
    /// as far as it is known before anything is measured.
    fn report(&self, status: Status, dry_run: bool) -> Report {
        Report::new(status, Some(&self.path), dry_run).with_goal(self.min_free, self.target_free)
    }
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

/// A tier's remaining units. `oldest` consumes a deque in linear time;
/// `best-fit` indexes each known estimate for logarithmic re-picks after
/// every observed space sample. Unknown estimates retain discovery order
/// after all known estimates. The key ranks singles before equal-size
/// buckets by deletion count, then preserves discovery order.
struct CandidateQueue<T> {
    order: Order,
    oldest: VecDeque<T>,
    sized: BTreeMap<(u64, usize, usize), T>,
    unknown: VecDeque<T>,
}

impl<T> CandidateQueue<T> {
    fn new(
        order: Order,
        items: impl IntoIterator<Item = T>,
        estimate: impl Fn(&T) -> Option<(u64, usize)>,
    ) -> Self {
        let mut queue = Self {
            order,
            oldest: VecDeque::new(),
            sized: BTreeMap::new(),
            unknown: VecDeque::new(),
        };
        for (index, item) in items.into_iter().enumerate() {
            if order == Order::Oldest {
                queue.oldest.push_back(item);
            } else if let Some((bytes, deletions)) = estimate(&item) {
                queue.sized.insert((bytes, deletions, index), item);
            } else {
                queue.unknown.push_back(item);
            }
        }
        queue
    }

    fn is_empty(&self) -> bool {
        self.oldest.is_empty() && self.sized.is_empty() && self.unknown.is_empty()
    }

    fn pop(&mut self, need: u64) -> Option<T> {
        if self.order == Order::Oldest {
            return self.oldest.pop_front();
        }
        let covering = self.sized.range((need, 0, 0)..).next().map(|(key, _)| *key);
        let key = covering.or_else(|| {
            let largest_size = self.sized.last_key_value()?.0.0;
            self.sized
                .range((largest_size, 0, 0)..)
                .next()
                .map(|(key, _)| *key)
        });
        key.and_then(|key| self.sized.remove(&key))
            .or_else(|| self.unknown.pop_front())
    }
}

/// Appends `entry` to the report's errors and logs it: no error reaches
/// the report without reaching stderr too.
fn record_error(report: &mut Report, log: &mut dyn FnMut(&str), entry: ErrorEntry) {
    let at: Vec<&str> = [entry.tier.as_deref(), entry.candidate.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    if at.is_empty() {
        log(&format!("error: {}", entry.message));
    } else {
        log(&format!("{}: error: {}", at.join(" "), entry.message));
    }
    report.errors.push(entry);
}

/// A usage error (exit 2), logged.
fn usage(report: Report, log: &mut dyn FnMut(&str), message: String) -> Report {
    log(&format!("error: {message}"));
    report.usage_error(message)
}

/// A host lock that could not be taken for a reason other than the
/// deadline or a signal: exit 2. A lock the installation did not provide
/// (missing, or not the regular file the tmpfiles line makes) is
/// `provisioning`, so a harness tells "alert ops" from "my call is wrong"
/// without parsing the message (TASK-151).
fn lock_refusal(report: Report, log: &mut dyn FnMut(&str), error: LockError) -> Report {
    let cause = match &error {
        LockError::Missing { .. } | LockError::Unusable { .. } => ErrorCause::Provisioning,
        // Unchanged contract (exit 2, `usage`) for a host failure while
        // locking; a TASK-151 follow-up asks whether it should be `host`/3.
        LockError::Failed(_) => ErrorCause::Usage,
        LockError::DeadlinePassed | LockError::Interrupted(_) => {
            unreachable!("the caller handles the deadline and signals: {error}")
        }
    };
    let message = error.to_string();
    log(&format!("error: {message}"));
    report.usage_error_with_cause(cause, message)
}

enum MeasureFailure {
    Usage(String),
    Host(String),
}

fn measure_failure(report: Report, log: &mut dyn FnMut(&str), failure: MeasureFailure) -> Report {
    match failure {
        MeasureFailure::Usage(message) => usage(report, log, message),
        MeasureFailure::Host(message) => {
            log(&format!("error: {message}"));
            let mut report = report;
            report.status = Status::Error;
            report
                .errors
                .push(ErrorEntry::run(ErrorCause::Host, message));
            report
        }
    }
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
    path: PathBuf,
    dev: Dev,
    min: u64,
    target: u64,
}

/// Reaches `PATH`, samples its filesystem and turns the sizes into bytes;
/// fills the report's `mount`, sizes and `avail_before_bytes`. Host sampling
/// failures remain distinct from invalid path and size requests.
fn measure<H: Host>(
    host: &H,
    request: &Request,
    report: &mut Report,
) -> Result<Measured<H::Fd>, MeasureFailure> {
    let at = resolve(host, &request.path, Want::Directory).map_err(|error| {
        let message = format!("PATH {}: {error}", request.path.display());
        match error {
            ResolveError::Host(_) => MeasureFailure::Host(message),
            ResolveError::UnsafePath { .. }
            | ResolveError::UnsupportedFs { .. }
            | ResolveError::Missing { .. } => MeasureFailure::Usage(message),
        }
    })?;
    report.mount = Some(at.mount.mount_point.clone());
    let space = host
        .statvfs(&at.fd)
        .map_err(|e| MeasureFailure::Host(format!("PATH {}: {e}", request.path.display())))?;
    report.avail_before_bytes = Some(space.avail_bytes);
    let min = request
        .min_free
        .bytes(space.size_bytes)
        .map_err(|e| MeasureFailure::Usage(format!("--min-free {}: {e}", request.min_free)))?;
    report.min_free_bytes = Some(min);
    let target = match request.target_free {
        None => min,
        Some(size) => size
            .bytes(space.size_bytes)
            .map_err(|e| MeasureFailure::Usage(format!("--target-free {size}: {e}")))?,
    };
    report.target_free_bytes = Some(target);
    if target < min {
        return Err(MeasureFailure::Usage(format!(
            "--target-free ({target} bytes) is below --min-free ({min} bytes)"
        )));
    }
    Ok(Measured {
        dev: at.stat.dev,
        at,
        path: request.path.clone(),
        min,
        target,
    })
}

fn measure_until<H: Host>(
    host: &H,
    request: &Request,
    report: &mut Report,
    deadline: Option<MonoTime>,
) -> Result<Measured<H::Fd>, MeasureFailure> {
    let started = host.now();
    let result = measure(host, request, report);
    record_deadline_overrun(host, report, deadline, started, "measure", &request.path);
    result
}

/// Setup can consume the whole call deadline before its first space sample.
/// Take that sample before choosing 0/124. A failed host sample is a run
/// error even when no prior measurement exists.
fn deadline_before_first_sample<H: Host>(
    host: &H,
    request: &Request,
    deadline: Option<MonoTime>,
    mut report: Report,
    log: &mut dyn FnMut(&str),
) -> Report {
    let measured = match measure_until(host, request, &mut report, deadline) {
        Ok(measured) => measured,
        Err(_) if host.interrupted().is_some() => return report,
        Err(error) => return measure_failure(report, log, error),
    };
    report.avail_after_bytes = report.avail_before_bytes;
    report.status = conclude(
        Final::of(report.avail_after_bytes, measured.min),
        true,
        false,
    );
    report
}

/// The deadline `timeout` from now.
fn deadline_of<H: Host>(host: &H, timeout: Option<Duration>) -> Option<MonoTime> {
    timeout.map(|t| host.now().checked_add(t).unwrap_or(MonoTime(Duration::MAX)))
}

fn warn_if_host_lock_unready<H: Host>(host: &H, log: &mut dyn FnMut(&str)) {
    if let Err(error) = hostlock::readiness(host) {
        log(&format!("warning: host lock is not ready: {error}"));
    }
}

fn warn_if_host_lock_unready_timed<H: Host>(
    host: &H,
    report: &mut Report,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
) {
    let started = host.now();
    warn_if_host_lock_unready(host, log);
    record_deadline_overrun(
        host,
        report,
        deadline,
        started,
        "host_lock_readiness",
        &hostlock::host_lock_path(),
    );
}

/// A foreign root cannot free space on PATH. Omit it from every source and
/// negated source before discovery, including union terms. Fatal root errors
/// still exclude the whole tier because discovery may be incomplete.
struct RootPreflight {
    errors: Vec<ErrorEntry>,
    skipped: Vec<SkipEntry>,
}

impl RootPreflight {
    fn filtered_tier(&self, tier: &Tier) -> Tier {
        let foreign: HashSet<&Path> = self
            .skipped
            .iter()
            .filter_map(|s| s.candidate.as_deref())
            .collect();
        let mut filtered = tier.clone();
        filtered
            .roots
            .retain(|root| !foreign.contains(root.as_path()));
        for source in &mut filtered.negated {
            source
                .roots
                .retain(|root| !foreign.contains(root.as_path()));
        }
        for source in &mut filtered.fs_union {
            source
                .roots
                .retain(|root| !foreign.contains(root.as_path()));
            for negated in &mut source.negated {
                negated
                    .roots
                    .retain(|root| !foreign.contains(root.as_path()));
            }
        }
        filtered
    }
}

/// A tier's roots for its log line.
fn roots_text(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The log line of a tier left without a root: each was on another
/// filesystem than PATH's ([`preflight_roots`] skips those as `other_fs`
/// before the tier runs), so it looked nowhere. It replaces the count line,
/// which read `judged below  (` with an empty root list (TASK-209).
fn nothing_searched(tier: &Tier) -> String {
    format!(
        "{}: `{}`: nothing searched: no root of the tier is on PATH's filesystem (skipped as \
         other_fs)",
        tier.name, tier.select
    )
}

/// Check every source root against PATH's device before enumerating candidates.
/// A missing root remains an empty source. A foreign root is reported and
/// skipped while same-device roots proceed: once per tier, however many of
/// its sources (a union's terms, a negated profile) share that root, so
/// that every tier reports it exactly once (TASK-210, decided for 0.1: a
/// skipped `other_fs` row, no new JSON field). An unsafe or unreadable root
/// excludes its tier; all roots are checked before another tier may delete.
/// Compare devices, not mount ids: a bind mount of the requested filesystem
/// can free space on PATH.
fn preflight_roots<H: Host>(host: &H, tier: &Tier, requested_dev: Dev) -> RootPreflight {
    let mut roots = Vec::new();
    roots.extend(tier.roots.iter().map(|root| (tier.profile.as_str(), root)));
    for source in &tier.negated {
        roots.extend(
            source
                .roots
                .iter()
                .map(|root| (source.profile.as_str(), root)),
        );
    }
    for source in &tier.fs_union {
        roots.extend(
            source
                .roots
                .iter()
                .map(|root| (source.profile.as_str(), root)),
        );
        for negated in &source.negated {
            roots.extend(
                negated
                    .roots
                    .iter()
                    .map(|root| (negated.profile.as_str(), root)),
            );
        }
    }
    if roots.is_empty() {
        return RootPreflight {
            errors: Vec::new(),
            skipped: Vec::new(),
        };
    }
    // The root itself may be inside systemd's PrivateTmp. Reject it from
    // its path components before resolve_in can open or stat any ancestor.
    let mut errors = Vec::new();
    let mut skipped = Vec::new();
    roots.retain(|(profile, root)| {
        if !fs_kind::path_in_private_tmp(root) {
            return true;
        }
        errors.push(ErrorEntry {
            tier: Some(tier.name.clone()),
            profile: Some((*profile).to_owned()),
            candidate: Some(root.display().to_string()),
            cause: ErrorCause::Safety,
            message: format!(
                "root {}: inside systemd PrivateTmp (systemd-private-*); misconfigured root; the tier is excluded",
                root.display()
            ),
        });
        false
    });
    if roots.is_empty() {
        return RootPreflight { errors, skipped };
    }
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            errors.push(host_error(
                tier,
                format!("cannot read mountinfo for root preflight: {error}; the tier is excluded"),
            ));
            return RootPreflight { errors, skipped };
        }
    };
    for (profile, root) in roots {
        let mut error = |cause: ErrorCause, message: String| {
            errors.push(ErrorEntry {
                tier: Some(tier.name.clone()),
                profile: Some(profile.to_owned()),
                candidate: Some(root.display().to_string()),
                cause,
                message,
            })
        };
        // A local bind mount can sit below an unsafe ancestor. Screen the
        // full mount chain before resolve_in opens any of it.
        if let Err(why) = screen_mount_chain(&mounts, root) {
            error(
                ErrorCause::Safety,
                format!("root {}: {why}; the tier is excluded", root.display()),
            );
            continue;
        }
        match resolve_in(host, &mounts, root, Want::Directory) {
            Ok(resolved) if resolved.stat.dev != requested_dev => {
                if !skipped
                    .iter()
                    .any(|skip: &SkipEntry| skip.candidate.as_ref() == Some(root))
                {
                    skipped.push(SkipEntry {
                        tier: tier.name.clone(),
                        profile: profile.to_owned(),
                        candidate: Some(root.clone()),
                        count: 1,
                        examples: Vec::new(),
                        reason: Reason::OtherFs,
                        detail: Some(crate::report::OTHER_FS_ROOT_DETAIL.to_owned()),
                        process_blockers: None,
                        estimated_bytes: None,
                    });
                }
            }
            Ok(_) | Err(ResolveError::Missing { .. }) => {}
            Err(ResolveError::UnsafePath { why, .. }) => error(
                ErrorCause::Safety,
                format!(
                    "root {}: unsafe_path (misconfigured, or the mounts changed): {why}; the tier is excluded",
                    root.display()
                ),
            ),
            Err(ResolveError::Host(host_error)) => error(
                ErrorCause::Host,
                format!(
                    "root {}: {host_error}; the tier is excluded",
                    root.display()
                ),
            ),
            Err(ResolveError::UnsupportedFs { missing, .. }) => error(
                ErrorCause::Safety,
                format!(
                    "root {}: unsupported filesystem check {missing}; the tier is excluded",
                    root.display()
                ),
            ),
        }
    }
    RootPreflight { errors, skipped }
}

/// Discover the complete raw match set. An unreadable or unsafe branch can
/// hide a protected descendant, so it excludes the run instead of yielding
/// a partial allow decision. A missing root is an empty match set.
fn scan_protections<H: Host>(
    host: &H,
    protections: &[ProtectedSource],
    deadline: Option<MonoTime>,
) -> Result<Vec<PathBuf>, String> {
    let mut matches = Vec::new();
    for protection in protections {
        let mut refused = Vec::new();
        match &protection.kind {
            ProtectedKind::Fs(find) => {
                let (found, stopped) = fs_kind::discover(host, &protection.roots, find, deadline);
                if let Some(stop) = stopped {
                    return Err(format!(
                        "protect `{}` discovery stopped: {stop}",
                        protection.profile
                    ));
                }
                refused.extend(found.refused);
                for (path, candidate) in found.candidates {
                    match candidate {
                        Ok(_) => matches.push(path),
                        Err(skip) => refused.push(skip),
                    }
                }
            }
            ProtectedKind::CargoTarget => {
                let found =
                    cargo_target::discover(host, &protection.roots, deadline).map_err(|stop| {
                        format!("protect `{}` discovery stopped: {stop}", protection.profile)
                    })?;
                refused.extend(found.refused);
                matches.extend(found.targets.into_iter().map(|target| target.path));
            }
        }
        for skip in refused {
            if skip.reason == Reason::Missing && protection.roots.contains(&skip.path) {
                continue;
            }
            return Err(format!(
                "protect `{}` cannot prove matches at {}: {}",
                protection.profile,
                skip.path.display(),
                skip
            ));
        }
    }
    Ok(matches)
}

/// Compare names and object identity. A same-device bind mount can expose a
/// protected subtree under a different name, so lexical prefixes alone are
/// insufficient. Each equal ancestor object supplies a pair of relative
/// paths; those paths overlap exactly when the underlying subtrees overlap.
fn overlaps_checked<H: Host>(
    host: &H,
    candidate: &Path,
    matches: &[PathBuf],
) -> Result<bool, String> {
    overlaps_checked_until(host, candidate, matches, None)
}

fn overlaps_checked_until<H: Host>(
    host: &H,
    candidate: &Path,
    matches: &[PathBuf],
    deadline: Option<MonoTime>,
) -> Result<bool, String> {
    if matches.is_empty() {
        return Ok(false);
    }
    if matches.iter().any(|path| select::overlaps(candidate, path)) {
        return Ok(true);
    }
    // Preflight every ancestor before opening even the deepest path: a
    // local bind mount can sit below a network or FUSE mount point.
    let mounts = host
        .mountinfo()
        .map_err(|error| format!("cannot read mountinfo to prove overlap: {error}"))?;
    let check_time = || {
        if deadline.is_some_and(|end| host.now() >= end) {
            Err(format!(
                "overlap check timed out at {}",
                candidate.display()
            ))
        } else {
            Ok(())
        }
    };
    screen_overlap_mounts(&mounts, candidate, matches, check_time)?;
    overlaps_with(candidate, matches, |path| {
        check_time()?;
        let at = resolve_in(host, &mounts, path, Want::Any)
            .map_err(|error| format!("cannot prove overlap at {}: {error}", path.display()))?;
        at.stat.dev_ino().ok_or_else(|| {
            format!(
                "cannot prove overlap at {}: statx omitted inode",
                path.display()
            )
        })
    })
}

fn screen_overlap_mounts(
    mounts: &[MountEntry],
    candidate: &Path,
    matches: &[PathBuf],
    mut check_time: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    for path in std::iter::once(candidate).chain(matches.iter().map(PathBuf::as_path)) {
        for ancestor in path.ancestors() {
            check_time()?;
            let mount = mount_for(mounts, ancestor).map_err(|error| {
                format!("cannot prove overlap at {}: {error}", ancestor.display())
            })?;
            if may_be_networked(&mount.fs_type) {
                return Err(format!(
                    "cannot prove overlap at {}: filesystem type {} may block on I/O",
                    ancestor.display(),
                    mount.fs_type
                ));
            }
            if fs_kind::path_in_private_tmp(&mount.fs_root) {
                return Err(format!(
                    "cannot prove overlap at {}: mount {} is bound from systemd PrivateTmp root {}",
                    ancestor.display(),
                    mount.mount_point.display(),
                    mount.fs_root.display()
                ));
            }
        }
    }
    Ok(())
}

fn overlaps_with(
    candidate: &Path,
    matches: &[PathBuf],
    mut identity: impl FnMut(&Path) -> Result<DevIno, String>,
) -> Result<bool, String> {
    if matches.is_empty() {
        return Ok(false);
    }
    if matches.iter().any(|path| select::overlaps(candidate, path)) {
        return Ok(true);
    }
    let mut cache = HashMap::<PathBuf, DevIno>::new();
    let mut cached_identity = |path: &Path| {
        if let Some(id) = cache.get(path) {
            return Ok(*id);
        }
        let id = identity(path)?;
        cache.insert(path.to_owned(), id);
        Ok(id)
    };
    let candidate_ancestors = ancestor_identities(candidate, &mut cached_identity)?;
    let mut by_identity = HashMap::<DevIno, Vec<&PathBuf>>::new();
    for (prefix, id) in &candidate_ancestors {
        by_identity.entry(*id).or_default().push(prefix);
    }
    for protected in matches {
        for (protected_prefix, protected_id) in
            ancestor_identities(protected, &mut cached_identity)?
        {
            if let Some(candidate_prefixes) = by_identity.get(&protected_id) {
                for candidate_prefix in candidate_prefixes {
                    let candidate_suffix =
                        candidate.strip_prefix(candidate_prefix).expect("ancestor");
                    let protected_suffix =
                        protected.strip_prefix(&protected_prefix).expect("ancestor");
                    if select::overlaps(candidate_suffix, protected_suffix) {
                        return Ok(true);
                    }
                }
            }
        }
    }
    Ok(false)
}

fn ancestor_identities(
    path: &Path,
    identity: &mut impl FnMut(&Path) -> Result<DevIno, String>,
) -> Result<Vec<(PathBuf, DevIno)>, String> {
    path.ancestors()
        .map(|ancestor| Ok((ancestor.to_owned(), identity(ancestor)?)))
        .collect()
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
    timed_report(host, || ensure_inner(host, request, owners, log))
}

fn ensure_inner<H: Delete>(
    host: &H,
    request: &Request,
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    let deadline = deadline_of(host, request.timeout);
    // Not a success until a path below says so.
    let report = request.report(Status::Error, false);
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
    let started = host.now();
    let gate = host.may_delete();
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "sandbox_gate",
        &request.path,
    );
    if let Err(gate) = gate {
        return usage(
            report,
            log,
            format!(
                "a reclaiming ensure cannot run here: {gate} (use --dry-run for a read-only plan)"
            ),
        );
    }
    if host.interrupted().is_some() {
        return report;
    }
    if deadline_passed(host, deadline) {
        return deadline_before_first_sample(host, request, deadline, report, log);
    }
    let docker_argv = ["docker".to_owned()];
    let command_argv = match &request.tier.action {
        Action::Command { argv, .. } => Some(argv.as_slice()),
        Action::DockerBuilderPrune | Action::DockerImagePrune { .. } => {
            Some(docker_argv.as_slice())
        }
        _ => None,
    };
    let command_ready = if let Some(argv) = command_argv {
        let started = host.now();
        let preflight = command_action::preflight(host, argv);
        record_deadline_overrun(
            host,
            &mut report,
            deadline,
            started,
            "preflight",
            &request.path,
        );
        if deadline_passed(host, deadline) {
            return deadline_before_first_sample(host, request, deadline, report, log);
        }
        match preflight {
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
                let measured = match measure_until(host, request, &mut report, deadline) {
                    Ok(measured) => measured,
                    Err(_) if host.interrupted().is_some() => return report,
                    Err(error) => return measure_failure(report, log, error),
                };
                if deadline_passed(host, deadline) {
                    return deadline_before_reclaim(
                        host,
                        &measured,
                        deadline,
                        report,
                        log,
                        &request.tier,
                    );
                }
                log(&format!(
                    "{}: profile `{}` is missing executable {program:?}",
                    request.tier.name, request.tier.profile
                ));
                report.skipped.push(SkipEntry {
                    tier: request.tier.name.clone(),
                    profile: request.tier.profile.clone(),
                    candidate: None,
                    count: 1,
                    examples: Vec::new(),
                    reason: Reason::Missing,
                    detail: None,
                    process_blockers: None,
                    estimated_bytes: None,
                });
                report.avail_after_bytes = Some(report.avail_before_bytes.expect("measured"));
                report.status = conclude(
                    Final::of(report.avail_after_bytes, measured.min),
                    false,
                    false,
                );
                if report.status == Status::Met {
                    warn_if_host_lock_unready_timed(host, &mut report, deadline, log);
                }
                return report;
            }
            Err(error) => {
                let measured = match measure_until(host, request, &mut report, deadline) {
                    Ok(measured) => measured,
                    Err(_) if host.interrupted().is_some() => return report,
                    Err(message) => return measure_failure(report, log, message),
                };
                if deadline_passed(host, deadline) {
                    return deadline_before_reclaim(
                        host,
                        &measured,
                        deadline,
                        report,
                        log,
                        &request.tier,
                    );
                }
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
    let measured = match measure_until(host, request, &mut report, deadline) {
        Ok(m) => m,
        Err(_) if host.interrupted().is_some() => return report,
        Err(e) => return measure_failure(report, log, e),
    };
    if deadline_passed(host, deadline) {
        return deadline_before_reclaim(host, &measured, deadline, report, log, &request.tier);
    }
    let avail = report.avail_before_bytes.expect("measured");
    if avail >= measured.min {
        report.status = Status::Met;
        report.avail_after_bytes = Some(avail);
        warn_if_host_lock_unready_timed(host, &mut report, deadline, log);
        return report;
    }
    let [avail_text, min_text, _] = human_space_values([Some(avail), Some(measured.min), None]);
    log(&format!(
        "{} available on {}, below --min-free {}: taking the host lock",
        avail_text,
        measured.at.mount.mount_point.display(),
        min_text
    ));
    let started = host.now();
    let acquired = hostlock::acquire(host, deadline);
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "host_lock",
        &hostlock::host_lock_path(),
    );
    let lock = match acquired {
        Ok(lock) => lock,
        Err(LockError::DeadlinePassed) => {
            log(&LockError::DeadlinePassed.to_string());
            // A fresh sample decides: the other holder may have freed enough.
            report.avail_after_bytes =
                final_sample(host, &measured, deadline, &mut report, log, &request.tier);
            report.status = after_lock_timeout(Final::of(report.avail_after_bytes, measured.min));
            return report;
        }
        Err(LockError::Interrupted(signal)) => {
            report.status = Status::Interrupted(signal);
            return report;
        }
        Err(e) => {
            if let Some(signal) = host.interrupted() {
                report.status = Status::Interrupted(signal);
                return report;
            }
            return lock_refusal(report, log, e);
        }
    };
    log(&format!(
        "host lock {} taken after {} ms",
        hostlock::host_lock_path().display(),
        lock.waited.as_millis()
    ));
    if deadline_passed(host, deadline) {
        return deadline_before_reclaim(host, &measured, deadline, report, log, &request.tier);
    }
    // Someone else (the reclaim we waited for) may have freed it meanwhile.
    let started = host.now();
    let sampled = sample(host, &measured);
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "statvfs",
        &measured.path,
    );
    match sampled {
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
            record_error(&mut report, log, host_error(&request.tier, e));
            return report;
        }
    }
    if deadline_passed(host, deadline) {
        return deadline_before_reclaim(host, &measured, deadline, report, log, &request.tier);
    }
    let started = host.now();
    let roots = preflight_roots(host, &request.tier, measured.dev);
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "preflight",
        &request.path,
    );
    if deadline_passed(host, deadline) {
        return deadline_before_reclaim(host, &measured, deadline, report, log, &request.tier);
    }
    report.skipped.extend(roots.skipped.iter().cloned());
    for error in roots.errors.iter().cloned() {
        record_error(&mut report, log, error);
    }
    if !report.errors.is_empty() {
        report.avail_after_bytes =
            final_sample(host, &measured, deadline, &mut report, log, &request.tier);
        report.status = conclude(
            Final::of(report.avail_after_bytes, measured.min),
            false,
            true,
        );
        return report;
    }
    let tier = roots.filtered_tier(&request.tier);
    let started = host.now();
    let protections = Protection::enumerate(host, &request.tier.protections, deadline);
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "protect",
        &request.path,
    );
    if deadline_passed(host, deadline) {
        return deadline_before_reclaim(host, &measured, deadline, report, log, &request.tier);
    }
    let protection = match protections {
        Ok(protection) => protection,
        Err(error) => {
            record_error(&mut report, log, run_error(&request.tier, error));
            report.avail_after_bytes =
                final_sample(host, &measured, deadline, &mut report, log, &request.tier);
            return report;
        }
    };
    let mut run = Reclaim {
        host,
        tier: &tier,
        measured: &measured,
        deadline,
        tally_budget: request.tally_budget,
        command_ready,
        docker_bound: None,
        protection: &protection,
        report: &mut report,
        log,
        deadline_hit: false,
        run_stopped: false,
        in_use_unknown_in_a_row: 0,
    };
    run.run(owners);
    let deadline_hit = run.deadline_hit;
    report.avail_after_bytes =
        final_sample(host, &measured, deadline, &mut report, log, &request.tier);
    report.status = conclude(
        Final::of(report.avail_after_bytes, measured.min),
        deadline_hit || deadline_passed(host, deadline),
        !report.errors.is_empty(),
    );
    // Released only here, by closing, after the final sample.
    drop(lock);
    report
}

/// Reclaim configured tiers in policy order under one host lock. All command
/// and Docker environment checks finish before the first deletion. A tier
/// whose environment fails is excluded and reported; the remaining tiers
/// still run. The caller supplies the tiers in group/ranking order.
pub fn ensure_grouped<H: Delete>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    timed_report(host, || {
        ensure_grouped_inner(host, request, tiers, tier_timeouts, owners, log)
    })
}

fn ensure_grouped_inner<H: Delete>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    let mut notes = NotesOncePerRun::new(log);
    let log: &mut dyn FnMut(&str) = &mut |line| notes.line(line);
    let deadline = deadline_of(host, request.timeout);
    let mut report = request.report(Status::Error, false);
    if tiers.is_empty() || tiers.len() != tier_timeouts.len() {
        return usage(report, log, "configuration has no tiers to run".to_owned());
    }
    if tiers
        .iter()
        .any(|tier| tier.protections != tiers[0].protections)
    {
        return usage(
            report,
            log,
            "configured tiers have different protection policies".to_owned(),
        );
    }
    for tier in tiers {
        match (&tier.source, &tier.action) {
            (SourceKind::CargoTarget, Action::CargoLockProtocol)
            | (SourceKind::Fs(_), Action::Delete { .. } | Action::Command { .. })
            | (SourceKind::DockerBuildCache, Action::DockerBuilderPrune)
            | (SourceKind::DockerImages, Action::DockerImagePrune { .. }) => {}
            (_, action) => {
                return usage(
                    report,
                    log,
                    format!(
                        "tier `{}`: profile `{}` removes with unsupported action `{action}`",
                        tier.name, tier.profile
                    ),
                );
            }
        }
    }
    let started = host.now();
    let gate = host.may_delete();
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "sandbox_gate",
        &request.path,
    );
    if let Err(gate) = gate {
        return usage(
            report,
            log,
            format!(
                "a reclaiming ensure cannot run here: {gate} (use --dry-run for a read-only plan)"
            ),
        );
    }
    if host.interrupted().is_some() {
        return report;
    }
    if deadline_passed(host, deadline) {
        return deadline_before_first_sample(host, request, deadline, report, log);
    }
    let measured = match measure_until(host, request, &mut report, deadline) {
        Ok(measured) => measured,
        Err(_) if host.interrupted().is_some() => return report,
        Err(error) => return measure_failure(report, log, error),
    };
    let first = &tiers[0];
    if deadline_passed(host, deadline) {
        return deadline_before_reclaim(host, &measured, deadline, report, log, first);
    }
    let avail = report.avail_before_bytes.expect("measured");
    if avail >= measured.min {
        report.status = Status::Met;
        report.avail_after_bytes = Some(avail);
        warn_if_host_lock_unready_timed(host, &mut report, deadline, log);
        return report;
    }
    let started = host.now();
    let acquired = hostlock::acquire(host, deadline);
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "host_lock",
        &hostlock::host_lock_path(),
    );
    let lock = match acquired {
        Ok(lock) => lock,
        Err(LockError::DeadlinePassed) => {
            report.avail_after_bytes =
                final_sample(host, &measured, deadline, &mut report, log, first);
            report.status = after_lock_timeout(Final::of(report.avail_after_bytes, measured.min));
            return report;
        }
        Err(LockError::Interrupted(signal)) => {
            report.status = Status::Interrupted(signal);
            return report;
        }
        Err(error) => {
            if let Some(signal) = host.interrupted() {
                report.status = Status::Interrupted(signal);
                return report;
            }
            return lock_refusal(report, log, error);
        }
    };
    log(&format!(
        "host lock {} taken after {} ms",
        hostlock::host_lock_path().display(),
        lock.waited.as_millis()
    ));
    if deadline_passed(host, deadline) {
        return deadline_before_reclaim(host, &measured, deadline, report, log, first);
    }
    let started = host.now();
    let sampled = sample(host, &measured);
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "statvfs",
        &measured.path,
    );
    match sampled {
        Ok(space) if space.avail_bytes >= measured.min => {
            report.status = Status::Met;
            report.avail_after_bytes = Some(space.avail_bytes);
            return report;
        }
        Ok(_) => {}
        Err(error) => {
            record_error(&mut report, log, host_error(first, error));
            return report;
        }
    }
    if deadline_passed(host, deadline) {
        return deadline_before_reclaim(host, &measured, deadline, report, log, first);
    }
    let started = host.now();
    let protections = Protection::enumerate(host, &first.protections, deadline);
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "protect",
        &request.path,
    );
    if deadline_passed(host, deadline) {
        return deadline_before_reclaim(host, &measured, deadline, report, log, first);
    }
    // Enumerated before any tier may delete (a protect that cannot be
    // enumerated stops the run first), then again before each later tier:
    // a tier's delete actions judge against its own (TASK-200).
    let mut protection = match protections {
        Ok(protection) => protection,
        Err(error) => {
            record_error(&mut report, log, run_error(first, error));
            report.avail_after_bytes =
                final_sample(host, &measured, deadline, &mut report, log, first);
            return report;
        }
    };
    let mut prepared = Vec::with_capacity(tiers.len());
    for tier in tiers {
        if deadline.is_some_and(|end| host.now() >= end) {
            report.avail_after_bytes =
                final_sample(host, &measured, deadline, &mut report, log, tier);
            report.status = conclude(
                Final::of(report.avail_after_bytes, measured.min),
                true,
                !report.errors.is_empty(),
            );
            return report;
        }
        let started = host.now();
        let roots = preflight_roots(host, tier, measured.dev);
        record_deadline_overrun(
            host,
            &mut report,
            deadline,
            started,
            "preflight",
            &request.path,
        );
        if deadline_passed(host, deadline) {
            return deadline_before_reclaim(host, &measured, deadline, report, log, tier);
        }
        report.skipped.extend(roots.skipped.iter().cloned());
        if !roots.errors.is_empty() {
            for error in roots.errors.iter().cloned() {
                record_error(&mut report, log, error);
            }
            prepared.push(None);
            continue;
        }
        let filtered_tier = roots.filtered_tier(tier);
        let tier = &filtered_tier;
        // Environmental checks of every tier finish under the host lock
        // before any tier may delete. The action rechecks volatile binding.
        let docker_argv = ["docker".to_owned()];
        let argv = match &tier.action {
            Action::Command { argv, .. } => Some(argv.as_slice()),
            Action::DockerBuilderPrune | Action::DockerImagePrune { .. } => {
                Some(docker_argv.as_slice())
            }
            _ => None,
        };
        let ready = if let Some(argv) = argv {
            let started = host.now();
            let preflight = command_action::preflight(host, argv);
            record_deadline_overrun(
                host,
                &mut report,
                deadline,
                started,
                "preflight",
                &request.path,
            );
            if deadline_passed(host, deadline) {
                return deadline_before_reclaim(host, &measured, deadline, report, log, tier);
            }
            match preflight {
                Ok(Preflight::Ready(ready)) => {
                    log(&format!(
                        "{}: command executables: {}",
                        tier.name,
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
                    log(&format!(
                        "{}: profile `{}` is missing executable {program:?}",
                        tier.name, tier.profile
                    ));
                    report.skipped.push(SkipEntry {
                        tier: tier.name.clone(),
                        profile: tier.profile.clone(),
                        candidate: None,
                        count: 1,
                        examples: Vec::new(),
                        reason: Reason::Missing,
                        detail: None,
                        process_blockers: None,
                        estimated_bytes: None,
                    });
                    prepared.push(None);
                    continue;
                }
                Err(error) => {
                    record_error(&mut report, log, run_error(tier, error.to_string()));
                    prepared.push(None);
                    continue;
                }
            }
        } else {
            None
        };
        let mut docker_bound = None;
        if matches!(
            tier.source,
            SourceKind::DockerBuildCache | SourceKind::DockerImages
        ) {
            let started = host.now();
            let inspected =
                docker_backend::inspect_backend(host, measured.dev, docker_storage(tier));
            record_deadline_overrun(
                host,
                &mut report,
                deadline,
                started,
                "preflight",
                &request.path,
            );
            if deadline_passed(host, deadline) {
                return deadline_before_reclaim(host, &measured, deadline, report, log, tier);
            }
            match inspected {
                Ok(Binding::Bound(bound)) => {
                    log(&format!(
                        "{}: Docker backend bound to {}",
                        tier.name,
                        docker_candidate(tier, &bound).display()
                    ));
                    docker_bound = Some(bound);
                }
                Ok(Binding::Skipped(skip)) => {
                    log(&format!(
                        "{}: skip Docker backend: {}: {}",
                        tier.name, skip.reason, skip.detail
                    ));
                    report.skipped.push(SkipEntry {
                        tier: tier.name.clone(),
                        profile: tier.profile.clone(),
                        candidate: None,
                        count: 1,
                        examples: Vec::new(),
                        reason: skip.reason,
                        detail: Some(skip.detail.clone()),
                        process_blockers: None,
                        estimated_bytes: None,
                    });
                    prepared.push(None);
                    continue;
                }
                Err(error) => {
                    record_error(&mut report, log, run_error(tier, error.to_string()));
                    prepared.push(None);
                    continue;
                }
            }
        }
        prepared.push(Some((filtered_tier, ready, docker_bound)));
    }
    let mut deadline_hit = false;
    let mut a_tier_ran = false;
    for ((_, tier_timeout), ready) in tiers.iter().zip(tier_timeouts).zip(prepared) {
        if host.interrupted().is_some() {
            break;
        }
        let Some((tier, ready, docker_bound)) = ready else {
            continue;
        };
        let tier = &tier;
        let started = host.now();
        let sampled = sample(host, &measured);
        record_deadline_overrun(
            host,
            &mut report,
            deadline,
            started,
            "statvfs",
            &measured.path,
        );
        let avail = match sampled {
            Ok(space) => space.avail_bytes,
            Err(error) => {
                record_error(&mut report, log, host_error(tier, error));
                break;
            }
        };
        if avail >= measured.target {
            break;
        }
        if deadline.is_some_and(|end| host.now() >= end) {
            deadline_hit = true;
            break;
        }
        let own_deadline =
            (!tier_timeout.is_zero()).then(|| deadline_of(host, Some(*tier_timeout)).unwrap());
        let tier_deadline = match (deadline, own_deadline) {
            (Some(run), Some(own)) => Some(run.min(own)),
            (run, None) => run,
            (None, own) => own,
        };
        if a_tier_ran {
            let started = host.now();
            let enumerated = Protection::enumerate(host, &first.protections, deadline);
            record_deadline_overrun(
                host,
                &mut report,
                deadline,
                started,
                "protect",
                &request.path,
            );
            if deadline_passed(host, deadline) {
                deadline_hit = true;
                break;
            }
            match enumerated {
                Ok(enumerated) => protection = enumerated,
                Err(error) => {
                    record_error(&mut report, log, run_error(tier, error));
                    break;
                }
            }
        }
        a_tier_ran = true;
        let mut run = Reclaim {
            host,
            tier,
            measured: &measured,
            deadline: tier_deadline,
            tally_budget: request.tally_budget,
            command_ready: ready,
            docker_bound,
            protection: &protection,
            report: &mut report,
            log,
            deadline_hit: false,
            run_stopped: false,
            in_use_unknown_in_a_row: 0,
        };
        run.run(owners);
        let timed_out = run.deadline_hit;
        let stopped = run.run_stopped;
        if timed_out {
            if deadline.is_some_and(|end| host.now() >= end) {
                deadline_hit = true;
                break;
            }
            record_error(
                &mut report,
                log,
                run_error(tier, "tier timeout passed; the tier is excluded"),
            );
            continue;
        }
        if stopped {
            break;
        }
    }
    report.avail_after_bytes = final_sample(host, &measured, deadline, &mut report, log, first);
    report.status = conclude(
        Final::of(report.avail_after_bytes, measured.min),
        deadline_hit || deadline_passed(host, deadline),
        !report.errors.is_empty(),
    );
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
    timed_report(host, || {
        let mut report = dry_run_inner(host, request, owners, log);
        if report.status == Status::DryRun && !report.errors.is_empty() {
            report.status = Status::Error;
        }
        report
    })
}

fn dry_run_inner<H: Host>(
    host: &H,
    request: &Request,
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    let deadline = deadline_of(host, request.timeout);
    let mut report = request.report(Status::DryRun, true);
    if host.interrupted().is_some() {
        return report;
    }
    let measured = match measure_until(host, request, &mut report, deadline) {
        Ok(m) => m,
        Err(_) if host.interrupted().is_some() => return report,
        Err(e) => return measure_failure(report, log, e),
    };
    if host.interrupted().is_some() {
        return report;
    }
    let tier = &request.tier;
    if deadline_passed(host, deadline) {
        return deadline_before_plan(host, &measured, deadline, report, log, tier);
    }
    let started = host.now();
    let roots = preflight_roots(host, tier, measured.dev);
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "preflight",
        &request.path,
    );
    if deadline_passed(host, deadline) {
        return deadline_before_plan(host, &measured, deadline, report, log, tier);
    }
    report
        .plan
        .extend(roots.skipped.iter().map(|skip| PlanEntry {
            candidate: skip.candidate.clone(),
            profile: skip.profile.clone(),
            detail: skip.detail.clone(),
            ..plan_tier_row(tier, Some(skip.reason))
        }));
    if !roots.errors.is_empty() {
        for error in roots.errors.iter().cloned() {
            record_error(&mut report, log, error);
        }
        report.avail_after_bytes = final_sample(host, &measured, deadline, &mut report, log, tier);
        return report;
    }
    let tier = roots.filtered_tier(tier);
    let tier = &tier;
    let started = host.now();
    let protections = scan_protections(host, &tier.protections, deadline);
    record_deadline_overrun(
        host,
        &mut report,
        deadline,
        started,
        "protect",
        &request.path,
    );
    if deadline_passed(host, deadline) {
        return deadline_before_plan(host, &measured, deadline, report, log, tier);
    }
    let protection_matches = match protections {
        Ok(matches) => matches,
        Err(error) => {
            record_error(&mut report, log, run_error(tier, error));
            report.status = Status::Error;
            return report;
        }
    };
    if matches!(
        tier.source,
        SourceKind::DockerBuildCache | SourceKind::DockerImages
    ) {
        let started = host.now();
        let inspected = docker_backend::inspect_backend(host, measured.dev, docker_storage(tier));
        record_deadline_overrun(
            host,
            &mut report,
            deadline,
            started,
            "preflight",
            &request.path,
        );
        if deadline_passed(host, deadline) {
            return deadline_before_plan(host, &measured, deadline, report, log, tier);
        }
        match inspected {
            Ok(Binding::Bound(bound)) => {
                let candidate = docker_candidate(tier, &bound);
                log(&format!(
                    "{}: Docker backend bound to {}",
                    tier.name,
                    candidate.display()
                ));
                let protected = match overlaps_checked(host, &candidate, &protection_matches) {
                    Ok(protected) => protected,
                    Err(error) => {
                        record_error(&mut report, log, run_error(tier, error));
                        report.status = Status::Error;
                        return report;
                    }
                };
                report.plan.push(plan_entry(
                    tier,
                    &candidate,
                    protected.then_some(Reason::Protected),
                    None,
                ));
            }
            Ok(Binding::Skipped(skip)) => {
                log(&format!(
                    "{}: skip Docker backend: {}: {}",
                    tier.name, skip.reason, skip.detail
                ));
                report.plan.push(plan_tier_row(tier, Some(skip.reason)));
            }
            Err(error) => record_error(&mut report, log, run_error(tier, error.to_string())),
        }
        report.avail_after_bytes = final_sample(host, &measured, deadline, &mut report, log, tier);
        return report;
    }
    if let Action::Command { argv, .. } = &tier.action {
        let started = host.now();
        let preflight = command_action::preflight(host, argv);
        record_deadline_overrun(
            host,
            &mut report,
            deadline,
            started,
            "preflight",
            &request.path,
        );
        if deadline_passed(host, deadline) {
            return deadline_before_plan(host, &measured, deadline, report, log, tier);
        }
        match preflight {
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
                report.plan.push(plan_tier_row(tier, Some(Reason::Missing)));
                report.avail_after_bytes =
                    final_sample(host, &measured, deadline, &mut report, log, tier);
                return report;
            }
            Err(error) => {
                record_error(&mut report, log, run_error(tier, error.to_string()));
                report.avail_after_bytes =
                    final_sample(host, &measured, deadline, &mut report, log, tier);
                return report;
            }
        }
    }
    let rules = rules(tier, owners);
    let mut checker = Checker::for_plan(&rules)
        .with_pid_lookup(tier.pid_lookup())
        .with_pytest_facts(tier.pytest_facts());
    let started = host.now();
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
            if tier.fs_union.is_empty() && tier.negated.is_empty() {
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
    record_deadline_overrun(host, &mut report, deadline, started, "plan", &request.path);
    if deadline_passed(host, deadline) {
        return deadline_before_plan(host, &measured, deadline, report, log, tier);
    }
    let Planned {
        mut eligible,
        skipped,
        stopped,
    } = planned;
    if host.interrupted().is_some() {
        // A partial judgement is not a trustworthy plan. Keep the root
        // refusals already recorded, but publish no unverified candidate.
        return report;
    }
    if tier.order == Order::Oldest {
        oldest_first(&mut eligible, |(last, _)| *last);
    } else {
        // A dry run cannot observe what reclamation actually frees. Order
        // the complete plan with estimated bytes and the initial sample.
        log(&format!(
            "{}: best-fit dry-run order uses estimates; actual reclaim order may differ",
            tier.name
        ));
        let mut remaining = measured
            .target
            .saturating_sub(report.avail_before_bytes.expect("measured"));
        let mut ordered = Vec::with_capacity(eligible.len());
        let mut queue =
            CandidateQueue::new(Order::BestFit, eligible, |(_, entry): &(_, PlanEntry)| {
                entry.estimated_bytes.map(|bytes| (bytes, 1))
            });
        while let Some(item) = queue.pop(remaining) {
            remaining = remaining.saturating_sub(item.1.estimated_bytes.unwrap_or(0));
            ordered.push(item);
        }
        eligible = ordered;
    }
    let root_skips = std::mem::take(&mut report.plan);
    report.plan = eligible
        .into_iter()
        .map(|(_, e)| e)
        .chain(skipped)
        .collect();
    for index in 0..report.plan.len() {
        let Some(candidate) = report.plan[index].candidate.clone() else {
            continue;
        };
        let started = host.now();
        let overlap = overlaps_checked(host, &candidate, &protection_matches);
        record_deadline_overrun(host, &mut report, deadline, started, "protect", &candidate);
        if deadline_passed(host, deadline) {
            return deadline_before_plan(host, &measured, deadline, report, log, tier);
        }
        let protected = match overlap {
            Ok(protected) => protected,
            Err(error) => {
                record_error(&mut report, log, run_error(tier, error));
                report.status = Status::Error;
                report.plan.extend(root_skips);
                return report;
            }
        };
        if protected {
            let entry = &mut report.plan[index];
            entry.reason = Some(Reason::Protected);
            entry.estimated_bytes = None;
            entry.detail = Some("overlaps a protected source match".to_owned());
        }
    }
    report.plan.extend(root_skips);
    if let Some(stop) = stopped {
        if matches!(stop, Stop::DeadlinePassed { .. }) {
            report.status = Status::Deadline;
        }
        record_error(
            &mut report,
            log,
            stop_error(tier, &stop, format!("stopped: {stop}")),
        );
    }
    // Unmeasured: unknown, not the earlier sample.
    report.avail_after_bytes = final_sample(host, &measured, deadline, &mut report, log, tier);
    if !deadline_passed(host, deadline) && host.interrupted().is_none() {
        let started = host.now();
        report.held_by_deleted_files = held(
            host,
            &mut checker,
            measured.dev,
            &measured.at.mount,
            deadline,
            log,
        );
        record_deadline_overrun(
            host,
            &mut report,
            deadline,
            started,
            "in_use",
            &request.path,
        );
    }
    if deadline_passed(host, deadline) {
        report.status = Status::Deadline;
    }
    report
}

/// Plan every configured tier in its policy order without taking a host
/// lock. Each plan remains tagged with its configured group and tier.
pub fn dry_run_grouped<H: Host>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    timed_report(host, || {
        dry_run_grouped_inner(host, request, tiers, tier_timeouts, owners, log)
    })
}

fn dry_run_grouped_inner<H: Host>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    let mut notes = NotesOncePerRun::new(log);
    let log: &mut dyn FnMut(&str) = &mut |line| notes.line(line);
    let Some(first) = tiers.first().filter(|_| tiers.len() == tier_timeouts.len()) else {
        return usage(
            request.report(Status::DryRun, true),
            log,
            "configuration has no tiers to plan".to_owned(),
        );
    };
    let run_deadline = deadline_of(host, request.timeout);
    let mut for_tier = request.clone();
    for_tier.tier = first.clone();
    for_tier.timeout = bounded_tier_timeout(host, run_deadline, tier_timeouts[0]);
    let mut report = dry_run(host, &for_tier, owners, log);
    if matches!(report.status, Status::UsageError | Status::Error) {
        return report;
    }
    if report.status == Status::Deadline {
        if run_deadline.is_some_and(|end| host.now() >= end) {
            return report;
        }
        // Only this tier's budget expired. Keep planning later tiers, but
        // the partial plan is an error, never a successful dry run.
        report.status = Status::Error;
    }
    for (tier, timeout) in tiers.iter().zip(tier_timeouts).skip(1) {
        if host.interrupted().is_some() {
            break;
        }
        if run_deadline.is_some_and(|end| host.now() >= end) {
            report.status = Status::Deadline;
            break;
        }
        for_tier.tier = tier.clone();
        for_tier.timeout = bounded_tier_timeout(host, run_deadline, *timeout);
        let part = dry_run(host, &for_tier, owners, log);
        report.overruns.extend(part.overruns.iter().cloned());
        if matches!(part.status, Status::UsageError | Status::Error) {
            report.errors.extend(part.errors);
            report.status = part.status;
            report.avail_after_bytes = part.avail_after_bytes;
            return report;
        }
        report.plan.extend(part.plan);
        report.errors.extend(part.errors);
        report.held_by_deleted_files = part.held_by_deleted_files;
        report.avail_after_bytes = part.avail_after_bytes;
        if part.status == Status::Deadline && run_deadline.is_some_and(|end| host.now() >= end) {
            report.status = Status::Deadline;
            break;
        }
        if part.status == Status::Deadline {
            // A tier deadline alone does not stop the overall plan.
            report.status = Status::Error;
        }
    }
    report
}

fn bounded_tier_timeout<H: Host>(
    host: &H,
    run_deadline: Option<MonoTime>,
    tier_timeout: Duration,
) -> Option<Duration> {
    let remaining = run_deadline.map(|end| end.saturating_since(host.now()));
    match (remaining, tier_timeout.is_zero()) {
        (Some(remaining), false) => Some(remaining.min(tier_timeout)),
        (Some(remaining), true) => Some(remaining),
        (None, false) => Some(tier_timeout),
        (None, true) => None,
    }
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
    reason: Option<Reason>,
    bytes: Option<u64>,
) -> PlanEntry {
    assert!(
        !candidate.as_os_str().is_empty(),
        "a plan row of the tier as a whole is plan_tier_row's"
    );
    PlanEntry {
        candidate: Some(candidate.to_owned()),
        estimated_bytes: bytes,
        ..plan_tier_row(tier, reason)
    }
}

/// The plan row of the tier as a whole, refused (`reason`) before it had a
/// candidate: a missing executable, an unavailable backend.
fn plan_tier_row(tier: &Tier, reason: Option<Reason>) -> PlanEntry {
    PlanEntry {
        group: tier.group.clone(),
        tier: tier.name.clone(),
        profile: tier.profile.clone(),
        candidate: None,
        reason,
        estimated_bytes: None,
        operation: tier.operation(),
        checkouts: tier.checkouts,
        build_evidence: None,
        detail: None,
        process_blockers: None,
    }
}

/// The plan row of a candidate `skip` refused. A dry run's verdicts are its
/// plan (stdout), so they are not logged as well.
fn plan_skip(tier: &Tier, candidate: &Path, skip: &Skip) -> PlanEntry {
    PlanEntry {
        detail: Some(skip.detail.clone()),
        process_blockers: skip.process_blockers.clone(),
        ..plan_entry(tier, candidate, Some(skip.reason), None)
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
    let entry = |candidate: &Path, reason: Option<Reason>, bytes: Option<u64>| {
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
                    skipped.push(plan_skip(tier, &skip.path, skip));
                }
            }
            for skip in &misconfigured {
                record_error(report, log, safety_error(tier, excluded(skip)));
            }
            let negated_matches = match scan_negated(host, &tier.negated, deadline) {
                Ok(matches) => matches,
                Err(error) => {
                    record_error(report, log, run_error(tier, error));
                    return Planned {
                        eligible,
                        skipped,
                        stopped,
                    };
                }
            };
            let targets = if misconfigured.is_empty() {
                found.targets.as_slice()
            } else {
                &[]
            };
            for target in targets {
                if host.interrupted().is_some() {
                    break;
                }
                let excluded =
                    match excluded_by(host, &tier.negated, &negated_matches, &target.path) {
                        Ok(excluded) => excluded,
                        Err(error) => {
                            record_error(report, log, run_error(tier, error));
                            return Planned {
                                eligible,
                                skipped,
                                stopped,
                            };
                        }
                    };
                if let Some(profile) = excluded {
                    let skip = Skip::new(
                        Reason::Excluded,
                        target.path.clone(),
                        format!("overlaps !profile:{profile}"),
                    );
                    skipped.push(plan_skip(tier, &target.path, &skip));
                    continue;
                }
                if let Some(skip) = other_fs(target, dev) {
                    skipped.push(plan_skip(tier, &target.path, &skip));
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
                        eligible.push((e.last_build, entry(&target.path, None, Some(e.bytes))));
                    }
                    Ok(Err(skip)) => {
                        skipped.push(plan_skip(tier, &target.path, &skip));
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
    if tier.roots.is_empty() {
        assert!(
            found.judged.is_empty(),
            "a tier without roots finds nothing"
        );
        log(&nothing_searched(tier));
    } else {
        log(&format!(
            "{}: `{}`: {} candidate(s) judged below {} ({} found again through another root \
             or pattern, {} systemd-private-* passed over{partial})",
            tier.name,
            tier.select,
            found.judged.len(),
            roots_text(&tier.roots),
            found.duplicates.len(),
            found.private_tmp,
        ));
    }
    let misconfigured: Vec<&Skip> = found
        .refused
        .iter()
        .filter(|skip| is_misconfigured(tier, skip))
        .collect();
    for skip in &misconfigured {
        record_error(report, log, safety_error(tier, excluded(skip)));
    }
    for skip in found.refused.iter().filter(|s| !is_misconfigured(tier, s)) {
        planned.skipped.push(plan_skip(tier, &skip.path, skip));
    }
    if !misconfigured.is_empty() {
        return planned;
    }
    for (path, source_evidence, verdict) in &found.judged {
        match verdict {
            Ok(e) => {
                let mut entry = plan_entry(tier, path, None, Some(e.bytes));
                entry.build_evidence =
                    eligible_build_evidence(host, e.source_evidence.as_ref(), e.newest_change);
                planned.eligible.push((e.newest_change, entry));
            }
            Err(skip) => {
                let mut entry = plan_skip(tier, path, skip);
                entry.build_evidence = skipped_build_evidence(source_evidence.as_ref());
                planned.skipped.push(entry);
            }
        }
    }
    planned
}

/// An eligible plan row's `build_evidence` (docs/reference.md, Output): the
/// source its judgement verified, with the idle age of the newest change
/// the eligibility walk measured; none without both.
fn eligible_build_evidence<H: Host>(
    host: &H,
    source: Option<&fs_kind::SourceEvidence>,
    newest: Option<Timestamp>,
) -> Option<BuildEvidence> {
    let (source, newest) = (source?, newest?);
    let now = host.wall_clock();
    let age_ns = (i128::from(now.secs) - i128::from(newest.secs)) * 1_000_000_000
        + i128::from(now.nanos)
        - i128::from(newest.nanos);
    Some(BuildEvidence {
        marker: source.marker,
        source: source.source.clone(),
        idle_age_seconds: Some(u64::try_from(age_ns.max(0) / 1_000_000_000).unwrap_or(u64::MAX)),
    })
}

/// A skipped plan row's `build_evidence`: the source established at
/// discovery, with no age (the skip stopped the walk before one was
/// established).
fn skipped_build_evidence(source: Option<&fs_kind::SourceEvidence>) -> Option<BuildEvidence> {
    source.map(|source| BuildEvidence {
        marker: source.marker,
        source: source.source.clone(),
        idle_age_seconds: None,
    })
}

/// One inode admitted by at least one filesystem source. `found_by` supplies
/// action choices; `guard_by` also retains structural matches whose source
/// evidence failed, so each positive guard has its own root and path for
/// lock-time rechecks. Discovery order chooses the report path.
struct FsUnionCandidate {
    identity: Identity,
    found_by: Vec<(usize, fs_kind::Found)>,
    guard_by: Vec<(usize, fs_kind::Found)>,
    invalid: bool,
}

impl FsUnionCandidate {
    /// The build source `source` (an index into the tier's sources: the
    /// row's profile) established when it discovered this candidate, for a
    /// skipped plan row's `build_evidence` (docs/reference.md, Output:
    /// discovery's, which may not have survived later rechecks). Only the
    /// row's own term: a row never reports another profile's anchor.
    fn discovered(&self, source: usize) -> Option<&fs_kind::SourceEvidence> {
        fn evidence((_, found): &(usize, fs_kind::Found)) -> Option<&fs_kind::SourceEvidence> {
            found.source_evidence.as_ref()
        }
        self.found_by
            .iter()
            .chain(&self.guard_by)
            .filter(|(index, _)| *index == source)
            .find_map(evidence)
    }

    fn contributors(&self) -> Vec<(usize, &fs_kind::Found, bool)> {
        let mut contributors: Vec<_> = self
            .found_by
            .iter()
            .map(|(index, found)| (*index, found, true))
            .collect();
        for (index, found) in &self.guard_by {
            if !self.found_by.iter().any(|(admitted_index, admitted)| {
                admitted_index == index
                    && admitted.root == found.root
                    && admitted.path == found.path
            }) {
                contributors.push((*index, found, false));
            }
        }
        contributors
    }
}

fn merge_fs_union_found(
    candidates: &mut Vec<FsUnionCandidate>,
    by_inode: &mut HashMap<DevIno, usize>,
    refused: &mut Vec<(usize, Skip)>,
    source_index: usize,
    found: fs_kind::Found,
    admitted: bool,
) {
    if let Some(&index) = by_inode.get(&found.identity.id) {
        let candidate = &mut candidates[index];
        if candidate.identity != found.identity {
            refused.push((
                source_index,
                Skip::new(
                    Reason::Changed,
                    found.path,
                    "the inode changed identity between source discoveries",
                ),
            ));
            candidate.invalid = true;
            candidate.found_by.clear();
            candidate.guard_by.clear();
        } else if !candidate.invalid {
            if admitted {
                candidate.found_by.push((source_index, found));
            } else {
                candidate.guard_by.push((source_index, found));
            }
        }
    } else {
        by_inode.insert(found.identity.id, candidates.len());
        let mut candidate = FsUnionCandidate {
            identity: found.identity,
            found_by: Vec::new(),
            guard_by: Vec::new(),
            invalid: false,
        };
        if admitted {
            candidate.found_by.push((source_index, found));
        } else {
            candidate.guard_by.push((source_index, found));
        }
        candidates.push(candidate);
    }
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
        let (discovery, stopped) =
            fs_kind::discover_union(host, source.roots, source.find, deadline);
        for found in discovery.guard_candidates {
            merge_fs_union_found(
                &mut candidates,
                &mut by_inode,
                &mut refused,
                source_index,
                found,
                false,
            );
        }
        refused.extend(
            discovery
                .refused
                .into_iter()
                .map(|skip| (source_index, skip)),
        );
        for (_, found) in discovery.candidates {
            let found = match found {
                Ok(found) => found,
                Err(skip) => {
                    refused.push((source_index, skip));
                    continue;
                }
            };
            merge_fs_union_found(
                &mut candidates,
                &mut by_inode,
                &mut refused,
                source_index,
                found,
                true,
            );
        }
        if let Some(stop) = stopped {
            return (candidates, refused, Some(stop));
        }
    }
    candidates.retain(|candidate| !candidate.invalid && !candidate.found_by.is_empty());
    (candidates, refused, None)
}

/// Enumerate a negated source's raw matches. Its own `where` was removed
/// during binding; any incomplete scan is an error, never an empty set.
fn scan_negated<H: Host>(
    host: &H,
    sources: &[NegatedSource],
    deadline: Option<MonoTime>,
) -> Result<Vec<Vec<PathBuf>>, String> {
    let mut all = Vec::with_capacity(sources.len());
    for source in sources {
        let mut paths = Vec::new();
        match &source.source {
            NegatedKind::Fs(find) => {
                let (discovery, stopped) = fs_kind::discover(host, &source.roots, find, deadline);
                if let Some(stop) = stopped {
                    return Err(format!(
                        "!profile:{} discovery stopped: {stop}",
                        source.profile
                    ));
                }
                if discovery.private_tmp != 0 {
                    return Err(format!(
                        "!profile:{} discovery skipped {} matching PrivateTmp path(s)",
                        source.profile, discovery.private_tmp
                    ));
                }
                for skip in discovery.refused {
                    if skip.reason != Reason::Missing || !source.roots.contains(&skip.path) {
                        return Err(format!(
                            "!profile:{} discovery uncertain: {}",
                            source.profile, skip.detail
                        ));
                    }
                }
                for (path, found) in discovery.candidates {
                    match found {
                        Ok(_) => paths.push(path),
                        Err(skip) => {
                            return Err(format!(
                                "!profile:{} discovery uncertain: {}",
                                source.profile, skip.detail
                            ));
                        }
                    }
                }
            }
            NegatedKind::CargoTarget => {
                let discovery =
                    cargo_target::discover(host, &source.roots, deadline).map_err(|stop| {
                        format!("!profile:{} discovery stopped: {stop}", source.profile)
                    })?;
                for skip in discovery.refused {
                    if skip.reason != Reason::Missing || !source.roots.contains(&skip.path) {
                        return Err(format!(
                            "!profile:{} discovery uncertain: {}",
                            source.profile, skip.detail
                        ));
                    }
                }
                paths.extend(discovery.targets.into_iter().map(|target| target.path));
            }
        }
        all.push(paths);
    }
    Ok(all)
}

fn excluded_by<H: Host>(
    host: &H,
    sources: &[NegatedSource],
    matches: &[Vec<PathBuf>],
    candidate: &Path,
) -> Result<Option<String>, String> {
    excluded_by_until(host, sources, matches, candidate, None)
}

fn excluded_by_until<H: Host>(
    host: &H,
    sources: &[NegatedSource],
    matches: &[Vec<PathBuf>],
    candidate: &Path,
    deadline: Option<MonoTime>,
) -> Result<Option<String>, String> {
    for (source, paths) in sources.iter().zip(matches) {
        if overlaps_checked_until(host, candidate, paths, deadline)? {
            return Ok(Some(source.profile.clone()));
        }
    }
    Ok(None)
}

/// Called by each kind while it still holds its candidate lock and before
/// its final action, enumerating every `protect` and `!profile:` match
/// again: before a command action, which may touch anything in the
/// candidate (a delete action asks [`tier_policy::DeletePolicy`] instead).
/// Protection is global; negation belongs to the selected source term.
/// Uncertain discovery or identity stops the run.
fn policy_skip<H: Host>(
    host: &H,
    protections: &[ProtectedSource],
    negated: &[NegatedSource],
    deadline: Option<MonoTime>,
    path: &Path,
) -> Result<Option<Skip>, Stop> {
    let protected = scan_protections(host, protections, deadline)
        .map_err(|error| policy_failed(path, "protect recheck", error))?;
    if let Some(skip) = protected_verdict(host, &protected, deadline, path)? {
        return Ok(Some(skip));
    }
    let negated_matches = scan_negated(host, negated, deadline)
        .map_err(|error| policy_failed(path, "!profile recheck", error))?;
    excluded_verdict(host, negated, &negated_matches, deadline, path)
}

/// `path` judged against enumerated `protect` and `!profile:` matches (one
/// list per `negated` source) at the final action check. Uncertain identity
/// stops the run.
fn policy_verdict<H: Host>(
    host: &H,
    protected: &[PathBuf],
    negated: &[NegatedSource],
    negated_matches: &[Vec<PathBuf>],
    deadline: Option<MonoTime>,
    path: &Path,
) -> Result<Option<Skip>, Stop> {
    if let Some(skip) = protected_verdict(host, protected, deadline, path)? {
        return Ok(Some(skip));
    }
    excluded_verdict(host, negated, negated_matches, deadline, path)
}

fn protected_verdict<H: Host>(
    host: &H,
    protected: &[PathBuf],
    deadline: Option<MonoTime>,
    path: &Path,
) -> Result<Option<Skip>, Stop> {
    let overlaps = overlaps_checked_until(host, path, protected, deadline)
        .map_err(|error| policy_failed(path, "protect overlap", error))?;
    Ok(overlaps.then(|| {
        Skip::new(
            Reason::Protected,
            path.to_owned(),
            "overlaps a protected match at final action check".to_owned(),
        )
    }))
}

fn excluded_verdict<H: Host>(
    host: &H,
    negated: &[NegatedSource],
    negated_matches: &[Vec<PathBuf>],
    deadline: Option<MonoTime>,
    path: &Path,
) -> Result<Option<Skip>, Stop> {
    Ok(
        excluded_by_until(host, negated, negated_matches, path, deadline)
            .map_err(|error| policy_failed(path, "!profile overlap", error))?
            .map(|profile| {
                Skip::new(
                    Reason::Excluded,
                    path.to_owned(),
                    format!("overlaps !profile:{profile} at final action check"),
                )
            }),
    )
}

fn policy_failed(path: &Path, what: &'static str, detail: String) -> Stop {
    Stop::Failed {
        path: path.to_owned(),
        error: HostError::invalid(what, path.display(), detail),
    }
}

fn scan_union_negated<H: Host>(
    host: &H,
    sources: &[FsUnionSourceRef<'_>],
    deadline: Option<MonoTime>,
) -> Result<Vec<Vec<Vec<PathBuf>>>, String> {
    sources
        .iter()
        .map(|source| scan_negated(host, source.negated, deadline))
        .collect()
}

fn union_checkouts(sources: &[FsUnionSourceRef<'_>], admitted: &[usize]) -> Checkouts {
    if admitted
        .iter()
        .any(|index| sources[*index].checkouts == Checkouts::Refuse)
    {
        Checkouts::Refuse
    } else {
        Checkouts::Allow
    }
}

fn union_checkout_path<'a>(
    sources: &[FsUnionSourceRef<'_>],
    candidate: &'a FsUnionCandidate,
    admitted: &[usize],
) -> Option<(usize, &'a Path, Skip)> {
    candidate
        .found_by
        .iter()
        .filter(|(index, _)| {
            admitted.contains(index) && sources[*index].checkouts == Checkouts::Refuse
        })
        .find_map(|(index, found)| {
            checkout_metadata_below(Checkouts::Refuse, &found.root, &found.path)
                .map(|skip| (*index, found.path.as_path(), skip))
        })
}

/// The checkout refusal of a union candidate whose admitted sources do not
/// all allow checkouts ([`union_checkouts`] is `refuse`): a refusing
/// source's checkout metadata on the path, else a checkout its walk found
/// (`checkout_marker`). When an admitted source allows checkouts, the
/// union is what refused it, and the detail says so in the profiles'
/// terms. That replaces a warning logged on every union run with an
/// `allow` source, though most unions (the two 0.1 `/tmp` built-ins) can
/// never find one candidate twice (TASK-222).
fn union_checkout_refusal(
    sources: &[FsUnionSourceRef<'_>],
    candidate: &FsUnionCandidate,
    admitted: &[usize],
    checkout_marker: Option<(usize, &Path)>,
) -> Option<(usize, Skip)> {
    let (refusing, mut skip) = union_checkout_path(sources, candidate, admitted)
        .map(|(index, _, skip)| (index, skip))
        .or_else(|| {
            checkout_marker.map(|(index, marker_path)| {
                (
                    index,
                    Skip::new(
                        Reason::ContainsCheckout,
                        marker_path.to_path_buf(),
                        "an admitted source refuses a version-control checkout in this candidate",
                    ),
                )
            })
        })?;
    if let Some(allowing) = admitted
        .iter()
        .map(|index| &sources[*index])
        .find(|source| source.checkouts == Checkouts::Allow)
    {
        skip.detail = format!(
            "{}; `{}` allows checkouts, but `{}` also selected this candidate and refuses them",
            skip.detail, allowing.profile, sources[refusing].profile
        );
    }
    Some((refusing, skip))
}

/// Retention belongs to every source that discovered this inode, regardless
/// of which source's evidence eventually admits it. Checkout policy remains
/// a separate decision over admitted terms only.
fn union_guard_veto<H: Host>(
    host: &H,
    candidate: &FsUnionCandidate,
    sources: &[FsUnionSourceRef<'_>],
    rules: &Rules<'_>,
    dev: Dev,
    deadline: Option<MonoTime>,
) -> Result<Option<(usize, Skip)>, Stop> {
    let evaluation_rules = Rules {
        checkouts: Checkouts::Allow,
        ..*rules
    };
    let alternatives: Vec<_> = candidate
        .guard_by
        .iter()
        .map(|(index, found)| {
            let source = &sources[*index];
            fs_kind::Alternative {
                judge: fs_kind::Judge {
                    find: source.find,
                    rules: &evaluation_rules,
                    idle: source.idle,
                    dev,
                },
                found,
                checkouts: source.checkouts,
                admitted: false,
            }
        })
        .collect();
    fs_kind::retention_guard_veto(host, &alternatives, deadline)
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
    if sources.iter().all(|source| source.roots.is_empty()) {
        log(&nothing_searched(tier));
    } else {
        log(&format!(
            "{}: `{}`: {} distinct filesystem candidate(s) from {} source term(s)",
            tier.name,
            tier.select,
            candidates.len(),
            sources.len()
        ));
    }
    if planned.stopped.is_some() {
        return planned;
    }
    let misconfigured: Vec<_> = refused
        .iter()
        .filter(|(index, skip)| is_misconfigured_source(&sources[*index], skip))
        .collect();
    for (index, skip) in &misconfigured {
        let mut entry = safety_error(tier, excluded(skip));
        entry.profile = Some(sources[*index].profile.to_owned());
        record_error(report, log, entry);
    }
    for (index, skip) in refused
        .iter()
        .filter(|(index, skip)| !is_misconfigured_source(&sources[*index], skip))
    {
        let mut entry = plan_skip(tier, &skip.path, skip);
        entry.profile = sources[*index].profile.to_owned();
        entry.checkouts = sources[*index].checkouts;
        planned.skipped.push(entry);
    }
    if !misconfigured.is_empty() {
        return planned;
    }
    let negated_matches = match scan_union_negated(host, &sources, deadline) {
        Ok(matches) => matches,
        Err(error) => {
            record_error(report, log, run_error(tier, error));
            return planned;
        }
    };
    let mut checker = Checker::for_plan(rules)
        .with_pid_lookup(tier.pid_lookup())
        .with_pytest_facts(tier.pytest_facts());
    for candidate in &candidates {
        if host.interrupted().is_some() {
            return planned;
        }
        match union_guard_veto(host, candidate, &sources, rules, dev, deadline) {
            Ok(Some((index, skip))) => {
                let (source_index, found) = &candidate.guard_by[index];
                let mut entry = plan_skip(tier, &skip.path, &skip);
                entry.profile = sources[*source_index].profile.to_owned();
                entry.checkouts = sources[*source_index].checkouts;
                entry.build_evidence = skipped_build_evidence(found.source_evidence.as_ref());
                planned.skipped.push(entry);
                continue;
            }
            Ok(None) => {}
            Err(stop) => {
                planned.stopped = Some(stop);
                return planned;
            }
        }
        let evaluation_rules = Rules {
            checkouts: Checkouts::Allow,
            ..*rules
        };
        let mut first_skip = None;
        let mut selected = None;
        let mut selected_record = None;
        let mut admitted = Vec::new();
        let mut checkout_marker = None;
        for (index, found) in &candidate.found_by {
            let source = &sources[*index];
            let excluded =
                match excluded_by(host, source.negated, &negated_matches[*index], &found.path) {
                    Ok(excluded) => excluded,
                    Err(error) => {
                        record_error(report, log, run_error(tier, error));
                        return planned;
                    }
                };
            if let Some(profile) = excluded {
                first_skip.get_or_insert((
                    *index,
                    found.path.as_path(),
                    Skip::new(
                        Reason::Excluded,
                        found.path.clone(),
                        format!("overlaps !profile:{profile}"),
                    ),
                ));
                continue;
            }
            let judge = fs_kind::Judge {
                find: source.find,
                rules: &evaluation_rules,
                idle: source.idle,
                dev,
            };
            match fs_kind::judge_walked(host, &judge, found, &mut checker, deadline) {
                Ok(Ok((tally, record))) => {
                    admitted.push(*index);
                    if source.checkouts == Checkouts::Refuse && tally.contains_checkout {
                        checkout_marker.get_or_insert((*index, found.path.as_path()));
                    }
                    if selected.is_none() {
                        selected = Some((*index, found.path.as_path(), tally));
                        selected_record = Some(record);
                    }
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
        if let Some((index, path, _)) = &selected {
            // Liveness contracts veto across the union, as in the reclaim.
            // The sources that found it by their own evidence (`found_by`;
            // guard-only matches are `guard_by`), whatever each term's own
            // judgement said: the reclaim's admitted alternatives.
            let claimed: Vec<(usize, &fs_kind::Found)> = candidate
                .found_by
                .iter()
                .map(|(source, found)| (*source, found))
                .collect();
            let contracts: Vec<(&Find, &fs_kind::Found)> = claimed
                .iter()
                .map(|(source, found)| (sources[*source].find, *found))
                .collect();
            let record = selected_record
                .as_ref()
                .expect("the selected term walked the candidate");
            match fs_kind::contract_veto(host, &contracts, &mut checker, deadline, record) {
                Ok(Ok(())) => {}
                Ok(Err((veto, skip))) => {
                    let vetoing = claimed.get(veto).map_or(*index, |(source, _)| *source);
                    let mut entry = plan_skip(tier, path, &skip);
                    entry.profile = sources[vetoing].profile.to_owned();
                    entry.checkouts = sources[vetoing].checkouts;
                    entry.build_evidence = skipped_build_evidence(candidate.discovered(vetoing));
                    planned.skipped.push(entry);
                    continue;
                }
                Err(stop) => {
                    planned.stopped = Some(stop);
                    return planned;
                }
            }
        }
        if let Some((index, path, tally)) = selected {
            let checkouts = union_checkouts(&sources, &admitted);
            if checkouts == Checkouts::Refuse
                && let Some((refusing_index, skip)) =
                    union_checkout_refusal(&sources, candidate, &admitted, checkout_marker)
            {
                let mut entry = plan_skip(tier, path, &skip);
                entry.profile = sources[refusing_index].profile.to_owned();
                entry.checkouts = checkouts;
                entry.build_evidence = skipped_build_evidence(candidate.discovered(refusing_index));
                planned.skipped.push(entry);
                continue;
            }
            let source = &sources[index];
            let mut entry = plan_entry(tier, path, None, Some(tally.bytes));
            entry.profile = source.profile.to_owned();
            entry.checkouts = checkouts;
            // Only the source the selected term's judgement verified: an
            // eligible row's evidence survived its rechecks (a skipped
            // row's records discovery, docs/reference.md, Output).
            entry.build_evidence =
                eligible_build_evidence(host, tally.source_evidence.as_ref(), tally.newest_change);
            planned.eligible.push((tally.newest_change, entry));
        } else {
            let (index, path, skip) =
                first_skip.expect("a discovered union candidate has a verdict");
            let mut entry = plan_skip(tier, path, &skip);
            entry.profile = sources[index].profile.to_owned();
            entry.checkouts = sources[index].checkouts;
            entry.build_evidence = skipped_build_evidence(candidate.discovered(index));
            planned.skipped.push(entry);
        }
    }
    planned
}

/// The one set of walk rules of `tier`'s run: every entry one of
/// `owners`', any socket or FIFO refuses (the default policy), the default
/// entry budget (the tier's `max_entries`), and the source profile's `checkouts`
/// setting. Operator checkout allowances are warned about by config loading;
/// a union's refusal of a candidate an `allow` source admitted says so in
/// its detail ([`union_checkout_refusal`]).
fn rules<'o>(tier: &Tier, owners: &'o [u32]) -> Rules<'o> {
    Rules {
        checkouts: tier.checkouts,
        max_entries: tier.max_entries,
        ..Rules::new(owners)
    }
}

/// The final sample of a run: `None` (unknown: never met) when it fails,
/// which is a logged run error.
fn final_sample<H: Host>(
    host: &H,
    measured: &Measured<H::Fd>,
    deadline: Option<MonoTime>,
    report: &mut Report,
    log: &mut dyn FnMut(&str),
    tier: &Tier,
) -> Option<u64> {
    let started = host.now();
    let result = sample(host, measured);
    record_deadline_overrun(host, report, deadline, started, "statvfs", &measured.path);
    match result {
        Ok(space) => Some(space.avail_bytes),
        Err(e) => {
            record_error(report, log, host_error(tier, e));
            None
        }
    }
}

fn deadline_before_reclaim<H: Host>(
    host: &H,
    measured: &Measured<H::Fd>,
    deadline: Option<MonoTime>,
    mut report: Report,
    log: &mut dyn FnMut(&str),
    tier: &Tier,
) -> Report {
    report.avail_after_bytes = final_sample(host, measured, deadline, &mut report, log, tier);
    report.status = conclude(
        Final::of(report.avail_after_bytes, measured.min),
        true,
        !report.errors.is_empty(),
    );
    report
}

fn deadline_before_plan<H: Host>(
    host: &H,
    measured: &Measured<H::Fd>,
    deadline: Option<MonoTime>,
    mut report: Report,
    log: &mut dyn FnMut(&str),
    tier: &Tier,
) -> Report {
    report.plan.clear();
    report.avail_after_bytes = final_sample(host, measured, deadline, &mut report, log, tier);
    record_error(
        &mut report,
        log,
        run_error(tier, "deadline passed before the plan was complete"),
    );
    report.status = Status::Deadline;
    report
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
    if tier.roots.is_empty() {
        assert!(
            found.targets.is_empty(),
            "a tier without roots finds nothing"
        );
        log(&nothing_searched(tier));
    } else {
        log(&format!(
            "{}: `{}`: {} target(s) below {}",
            tier.name,
            tier.select,
            found.targets.len(),
            roots_text(&tier.roots)
        ));
    }
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
    if skip.reason == Reason::InUse {
        // The detail is human text, not a list format: process arguments can
        // themselves contain `; `. Bound the line without guessing a count.
        let mut chars = skip.detail.chars();
        let preview: String = chars.by_ref().take(240).collect();
        let suffix = if chars.next().is_some() {
            "… (more detail omitted)"
        } else {
            ""
        };
        log(&format!(
            "{}: skip {}: {}: {}{}",
            tier.name,
            skip.reason,
            skip.path.display(),
            preview,
            suffix
        ));
    } else {
        log(&format!("{}: skip {skip}", tier.name));
    }
}

/// An error of the tier or the run (no one candidate).
fn run_error(tier: &Tier, message: impl Into<String>) -> ErrorEntry {
    ErrorEntry {
        tier: Some(tier.name.clone()),
        profile: Some(tier.profile.clone()),
        candidate: None,
        cause: ErrorCause::Operation,
        message: message.into(),
    }
}

fn host_error(tier: &Tier, message: impl Into<String>) -> ErrorEntry {
    let mut entry = run_error(tier, message);
    entry.cause = ErrorCause::Host;
    entry
}

fn safety_error(tier: &Tier, message: impl Into<String>) -> ErrorEntry {
    let mut entry = run_error(tier, message);
    entry.cause = ErrorCause::Safety;
    entry
}

fn stop_error(tier: &Tier, stop: &Stop, message: impl Into<String>) -> ErrorEntry {
    match stop {
        Stop::Failed { .. } => host_error(tier, message),
        Stop::DeadlinePassed { .. } | Stop::Interrupted { .. } => run_error(tier, message),
    }
}

/// How a process scan's notes start on stderr ([`held`]).
const SCAN_NOTE_PREFIX: &str = "process scan: ";

/// A grouped run's log: each tier ends with its process scan's notes, and
/// on a shared host those are the same every tier ("3 processes of other
/// users not inspected", three times for three tiers: TASK-211). A note is
/// logged the first time its exact text appears in the run; a note that
/// changes (another count, another process) is logged again. Every other
/// line passes through. Bounded by the distinct notes of one run.
struct NotesOncePerRun<'l> {
    log: &'l mut dyn FnMut(&str),
    seen: BTreeSet<String>,
}

impl<'l> NotesOncePerRun<'l> {
    fn new(log: &'l mut dyn FnMut(&str)) -> Self {
        Self {
            log,
            seen: BTreeSet::new(),
        }
    }

    fn line(&mut self, line: &str) {
        if line.starts_with(SCAN_NOTE_PREFIX) && !self.seen.insert(line.to_owned()) {
            return;
        }
        (self.log)(line);
    }
}

/// `held_by_deleted_files` (docs/reference.md, Output) from the run's
/// latest process scan; if the run made none, from one made now (stopping
/// at `deadline`). `None` (unknown, JSON `null`) when that scan failed or
/// stopped, or its process list did not settle: never an empty list, which
/// says that nothing is held (TASK-187).
fn held<H: Host>(
    host: &H,
    checker: &mut Checker<'_>,
    dev: Dev,
    mount: &MountEntry,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
) -> Option<Vec<HeldEntry>> {
    if checker.latest().is_none()
        && let Err(e) = checker.current(host, deadline)
    {
        log(&format!(
            "held_by_deleted_files: the process scan did not complete, unknown: {e}"
        ));
        return None;
    }
    let scan = checker.latest().expect("kept or made above");
    for note in scan.notes() {
        log(&format!("{SCAN_NOTE_PREFIX}{note}"));
    }
    if !scan.settled() {
        log("held_by_deleted_files: the process scan did not settle, unknown");
        return None;
    }
    let mut devices = vec![dev];
    if mount.fs_type == "btrfs" {
        match host.mountinfo() {
            Ok(mounts) => {
                devices.extend(mounts.into_iter().filter_map(|other| {
                    (other.fs_type == "btrfs" && other.source == mount.source).then_some(other.dev)
                }));
            }
            Err(e) => log(&format!(
                "held_by_deleted_files: cannot identify other btrfs subvolumes: {e}"
            )),
        }
    }
    Some(
        scan.held_by_deleted_files_on(&devices)
            .into_iter()
            .map(|h| HeldEntry {
                pid: h.pid.0,
                command: Some(h.command.to_string_lossy().into_owned()).filter(|c| !c.is_empty()),
                bytes: h.bytes,
            })
            .collect(),
    )
}

/// One reclaim of the tier, under the host lock.
struct Reclaim<'a, 'l, H: Delete> {
    host: &'a H,
    tier: &'a Tier,
    measured: &'a Measured<H::Fd>,
    deadline: Option<MonoTime>,
    tally_budget: Option<Duration>,
    command_ready: Option<Ready>,
    docker_bound: Option<Bound>,
    /// The tier's `protect` matches ([`DeletePolicy`]).
    protection: &'a Protection,
    report: &'a mut Report,
    log: &'a mut (dyn FnMut(&str) + 'l),
    deadline_hit: bool,
    run_stopped: bool,
    /// Candidates in a row, in the order taken, whose reclaim ended
    /// `in_use_unknown` ([`IN_USE_UNKNOWN_IN_A_ROW_MAX`]).
    in_use_unknown_in_a_row: usize,
}

/// After this many candidates in a row of one tier end `in_use_unknown`
/// (each spent its [`crate::inuse::FRESH_ATTEMPTS`]), the host is taken to
/// be too busy for this run: the tier's remaining candidates are skipped
/// `in_use_unknown` unchecked rather than each spending its own scans and
/// pauses (docs/safety.md, In use: Freshness). Three: one unlucky
/// candidate, or two, is a spike; three in a row (at least nine stale
/// scans) is the load the run is in.
const IN_USE_UNKNOWN_IN_A_ROW_MAX: usize = 3;

impl<H: Delete> Reclaim<'_, '_, H> {
    fn stop_if_interrupted(&mut self) -> bool {
        if self.host.interrupted().is_some() {
            self.run_stopped = true;
            return true;
        }
        false
    }

    /// Whether the tier's action deletes only entries its eligibility walk
    /// listed (`delete`, the Cargo protocol), so that its candidates can be
    /// judged against the tier's enumerated `protect` and `!profile:`
    /// matches ([`DeletePolicy`]). A command, or a Docker prune, acts on
    /// what it finds when it runs: every check enumerates them again.
    fn unlinks_only_walked_entries(&self) -> bool {
        match self.tier.action {
            Action::Delete { .. } | Action::CargoLockProtocol => true,
            Action::Command { .. }
            | Action::DockerBuilderPrune
            | Action::DockerImagePrune { .. } => false,
        }
    }

    /// Recheck immediately before entering a kind's reclaim protocol. A
    /// changed or unreadable protected tree stops the entire run.
    fn protected_before_reclaim(&mut self, candidate: &Path) -> bool {
        let started = self.host.now();
        let blocked = self.protected_before_reclaim_inner(candidate);
        self.record_overrun("protect", candidate, started, self.host.now());
        if self.past_deadline() {
            self.deadline_hit = true;
            self.run_stopped = true;
            return true;
        }
        blocked
    }

    fn protected_before_reclaim_inner(&mut self, candidate: &Path) -> bool {
        if self.tier.protections.is_empty() {
            return false;
        }
        match overlaps_checked_until(
            self.host,
            candidate,
            &self.protection.matches,
            self.deadline,
        ) {
            Ok(true) => {
                self.skip(
                    candidate,
                    &Skip::new(
                        Reason::Protected,
                        candidate.to_owned(),
                        "overlaps a protected match found before reclaim".to_owned(),
                    ),
                );
                return true;
            }
            Ok(false) => {
                if self.unlinks_only_walked_entries() {
                    // Judged against the tier's matches again under the
                    // lock and after the walk ([`DeletePolicy`]):
                    // enumerating them here too would only repeat the
                    // tier's enumeration per candidate.
                    return false;
                }
            }
            // An enumerated match removed or moved since: the enumeration
            // below decides (TASK-200).
            Err(_) => {}
        }
        match scan_protections(self.host, &self.tier.protections, self.deadline) {
            Ok(matches) => {
                match overlaps_checked_until(self.host, candidate, &matches, self.deadline) {
                    Ok(true) => {
                        self.skip(
                            candidate,
                            &Skip::new(
                                Reason::Protected,
                                candidate.to_owned(),
                                "overlaps a protected match at deletion recheck".to_owned(),
                            ),
                        );
                        true
                    }
                    Ok(false) => false,
                    Err(error) => {
                        self.run_error(error);
                        true
                    }
                }
            }
            Err(error) => {
                self.run_error(error);
                true
            }
        }
    }
    /// `candidate` `None`: the backend as a whole was skipped.
    fn skip_docker(&mut self, candidate: Option<&Path>, reason: Reason, detail: &str) {
        (self.log)(&format!(
            "{}: skip Docker backend: {reason}: {detail}",
            self.tier.name
        ));
        self.report.skipped.push(SkipEntry {
            tier: self.tier.name.clone(),
            profile: self.tier.profile.clone(),
            candidate: candidate.map(Path::to_owned),
            count: 1,
            examples: Vec::new(),
            reason,
            detail: Some(detail.to_owned()),
            process_blockers: None,
            estimated_bytes: None,
        });
    }

    fn walk_docker(&mut self) {
        if self.stop_if_interrupted() {
            return;
        }
        let started = self.host.now();
        let binding = if let Some(bound) = &self.docker_bound {
            Ok(Binding::Bound(bound.clone()))
        } else {
            docker_backend::inspect_backend(self.host, self.measured.dev, docker_storage(self.tier))
        };
        self.record_overrun("preflight", &self.measured.path, started, self.host.now());
        if self.past_deadline() {
            self.deadline_hit = true;
            return;
        }
        let bound = match binding {
            Ok(Binding::Bound(bound)) => bound,
            Ok(Binding::Skipped(skip)) => {
                self.skip_docker(None, skip.reason, &skip.detail);
                return;
            }
            Err(error) => {
                self.tier_error(format!("bind Docker backend: {error}"));
                return;
            }
        };
        let candidate = docker_candidate(self.tier, &bound);
        if self.protected_before_reclaim(&candidate) {
            return;
        }
        if self.stop_if_interrupted() {
            return;
        }
        let started = self.host.now();
        let sampled = sample(self.host, self.measured);
        self.record_overrun("statvfs", &self.measured.path, started, self.host.now());
        let before = match sampled {
            Ok(space) => space.avail_bytes,
            Err(error) => {
                self.host_failure(error);
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
        let started = self.host.now();
        let rebound = docker_backend::inspect_backend(
            self.host,
            self.measured.dev,
            docker_storage(self.tier),
        );
        self.record_overrun("preflight", &candidate, started, self.host.now());
        if self.past_deadline() {
            self.deadline_hit = true;
            return;
        }
        match rebound {
            Ok(Binding::Bound(rebound)) if rebound == bound => {}
            Ok(Binding::Bound(_)) => {
                self.skip_docker(
                    Some(&candidate),
                    Reason::Changed,
                    "Docker daemon, context, builder or storage changed before prune",
                );
                return;
            }
            Ok(Binding::Skipped(skip)) => {
                self.skip_docker(Some(&candidate), skip.reason, &skip.detail);
                return;
            }
            Err(error) => {
                self.tier_error(format!("recheck Docker backend: {error}"));
                return;
            }
        }
        if self.protected_before_reclaim(&candidate) {
            return;
        }
        if self.stop_if_interrupted() {
            return;
        }
        let started = self.host.now();
        let resolved = resolve(self.host, Path::new("/"), Want::Directory);
        self.record_overrun("resolve", Path::new("/"), started, self.host.now());
        if self.past_deadline() {
            self.deadline_hit = true;
            return;
        }
        let cwd = match resolved {
            Ok(cwd) => cwd,
            Err(error) => {
                self.tier_error(format!("open Docker command cwd: {error}"));
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
                expected_bytes: None,
                tallied_bytes: None,
                idle_age_seconds: None,
                removed_entries: 0,
                attempted_command: attempted,
                end,
                note: String::new(),
            },
        );
    }

    /// Whether the tier has given up checking (see
    /// [`IN_USE_UNKNOWN_IN_A_ROW_MAX`]); then `candidate` is skipped
    /// `in_use_unknown` unchecked, and `true`.
    fn too_busy_to_check(&mut self, candidate: &Path, profile: &str) -> bool {
        if self.in_use_unknown_in_a_row < IN_USE_UNKNOWN_IN_A_ROW_MAX {
            return false;
        }
        let skip = Skip::new(
            Reason::InUseUnknown,
            candidate.to_owned(),
            format!(
                "not checked: the host was too busy to check the {} candidates before it in \
                 this tier; kept, run again when the host is less busy",
                self.in_use_unknown_in_a_row
            ),
        );
        self.skip_as(candidate, profile, &skip);
        true
    }

    fn skip(&mut self, candidate: &Path, skip: &Skip) {
        let profile = self.tier.profile.clone();
        self.skip_as(candidate, &profile, skip);
    }

    fn skip_as(&mut self, candidate: &Path, profile: &str, skip: &Skip) {
        self.skip_as_tallied(candidate, profile, skip, None);
    }

    fn skip_as_tallied(
        &mut self,
        candidate: &Path,
        profile: &str,
        skip: &Skip,
        estimated_bytes: Option<u64>,
    ) {
        // Reclaimed Cargo targets keep their profile directories and lock
        // files. They remain discoverable, but have no work left to skip on
        // each later ensure, so the result omits them (docs/reference.md,
        // Output). A dry-run records them in `plan` separately. Logged, so
        // that the tier's "N target(s) below" adds up (TASK-213).
        if matches!(self.tier.source, SourceKind::CargoTarget) && skip.reason == Reason::Empty {
            (self.log)(&format!(
                "{}: {}: {}; nothing to reclaim, not listed in the result",
                self.tier.name,
                skip.path.display(),
                skip.detail
            ));
            return;
        }
        log_skip(self.log, self.tier, skip);
        self.report.skipped.push(SkipEntry {
            tier: self.tier.name.clone(),
            profile: profile.to_owned(),
            candidate: Some(candidate.to_owned()),
            count: 1,
            examples: Vec::new(),
            reason: skip.reason,
            detail: Some(skip.detail.clone()),
            process_blockers: skip.process_blockers.clone(),
            estimated_bytes,
        });
    }

    fn error_as(&mut self, candidate: &Path, profile: &str, message: String) {
        let entry = ErrorEntry {
            tier: Some(self.tier.name.clone()),
            profile: Some(profile.to_owned()),
            candidate: Some(candidate.to_string_lossy().into_owned()),
            cause: ErrorCause::Operation,
            message,
        };
        record_error(self.report, self.log, entry);
    }

    /// A tier-local environment error: this tier is excluded while other
    /// preflighted tiers may still run.
    fn tier_error(&mut self, message: String) {
        record_error(self.report, self.log, run_error(self.tier, message));
    }

    /// An error of the run (no one candidate): the group walk stops.
    fn run_error(&mut self, message: String) {
        self.run_stopped = true;
        self.tier_error(message);
    }

    fn host_failure(&mut self, message: String) {
        self.run_stopped = true;
        record_error(self.report, self.log, host_error(self.tier, message));
    }

    /// An error of the run: recorded, and the run stops. A deadline or a
    /// signal is no error: the run's status says it.
    fn stop(&mut self, stop: &Stop) {
        self.run_stopped = true;
        match stop {
            Stop::DeadlinePassed { .. } => {
                self.deadline_hit = true;
                (self.log)(&format!("{}: the deadline passed: {stop}", self.tier.name));
            }
            Stop::Interrupted { .. } => {
                (self.log)(&format!("{}: {stop}", self.tier.name));
            }
            Stop::Failed { .. } => self.host_failure(format!("the run stops: {stop}")),
        }
    }

    fn past_deadline(&self) -> bool {
        self.deadline.is_some_and(|d| self.host.now() >= d)
    }

    fn run(&mut self, owners: &[u32]) {
        // An earlier tier's list is not this run's last word: unknown until
        // this tier's end-of-run scan answers (null if it stops first).
        self.report.held_by_deleted_files = None;
        if self.stop_if_interrupted() {
            return;
        }
        let rules = rules(self.tier, owners);
        let mut checker = Checker::new(&rules, REUSE_LIMIT)
            .with_pid_lookup(self.tier.pid_lookup())
            .with_pytest_facts(self.tier.pytest_facts());
        // No `_` arm: a new source kind must decide here (and in `ensure`'s
        // allowlist) how it is reclaimed. Each kind runs its own protocol
        // only: an fs tier never reaches the Cargo reclaim, a Cargo tier
        // never the fs one.
        match &self.tier.source {
            SourceKind::CargoTarget => self.walk_cargo(&rules, &mut checker),
            SourceKind::Fs(find) => {
                if self.tier.fs_union.is_empty() && self.tier.negated.is_empty() {
                    self.walk_fs(find, &rules, &mut checker);
                } else {
                    self.walk_fs_union(&rules, &mut checker);
                }
            }
            SourceKind::DockerBuildCache | SourceKind::DockerImages => self.walk_docker(),
        }
        if !self.deadline_hit && !self.run_stopped && !self.stop_if_interrupted() {
            let started = self.host.now();
            let held_files = held(
                self.host,
                &mut checker,
                self.measured.dev,
                &self.measured.at.mount,
                self.deadline,
                self.log,
            );
            self.record_overrun("in_use", &self.measured.path, started, self.host.now());
            self.report.held_by_deleted_files = held_files;
            if self.past_deadline() {
                self.deadline_hit = true;
                self.run_stopped = true;
            }
        }
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
                candidate: Some(skip.path.clone()),
                count: 1,
                examples: Vec::new(),
                reason: skip.reason,
                detail: Some(skip.detail.clone()),
                process_blockers: skip.process_blockers.clone(),
                estimated_bytes: None,
            });
        }
        for skip in &misconfigured {
            // A bad configured root excludes this tier only. Other tiers
            // were independently preflighted and may still run.
            record_error(
                self.report,
                self.log,
                safety_error(self.tier, excluded(skip)),
            );
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
        let started = self.host.now();
        let discovered = discover(self.host, self.tier, self.deadline, self.log);
        self.record_overrun("discover", &self.measured.path, started, self.host.now());
        let found = match discovered {
            Ok(found) => found,
            Err(stop) => return self.stop(&stop),
        };
        if self.past_deadline() {
            return self.stop(&Stop::DeadlinePassed {
                path: self.measured.path.clone(),
            });
        }
        if !self.refusals(&found.refused) {
            return;
        }
        let started = self.host.now();
        let scanned = scan_negated(self.host, &self.tier.negated, self.deadline);
        self.record_overrun("exclude", &self.measured.path, started, self.host.now());
        if self.past_deadline() {
            return self.stop(&Stop::DeadlinePassed {
                path: self.measured.path.clone(),
            });
        }
        let negated_matches = match scanned {
            Ok(matches) => matches,
            Err(error) => return self.run_error(error),
        };
        if !self.tier.negated.is_empty() {
            (self.log)(&format!(
                "{}: {} negated profile(s), {} raw match(es) scanned before reclaim",
                self.tier.name,
                self.tier.negated.len(),
                negated_matches.iter().map(Vec::len).sum::<usize>()
            ));
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
            if self.stop_if_interrupted() {
                return;
            }
            let started = self.host.now();
            let overlap = overlaps_checked(self.host, &target.path, &self.protection.matches);
            self.record_overrun("protect", &target.path, started, self.host.now());
            if self.past_deadline() {
                return self.stop(&Stop::DeadlinePassed {
                    path: target.path.clone(),
                });
            }
            let protected = match overlap {
                Ok(protected) => protected,
                Err(error) => return self.run_error(error),
            };
            if protected {
                self.skip(
                    &target.path,
                    &Skip::new(
                        Reason::Protected,
                        target.path.clone(),
                        "overlaps a protected match".to_owned(),
                    ),
                );
                continue;
            }
            let started = self.host.now();
            let exclusion = excluded_by(
                self.host,
                &self.tier.negated,
                &negated_matches,
                &target.path,
            );
            self.record_overrun("exclude", &target.path, started, self.host.now());
            if self.past_deadline() {
                return self.stop(&Stop::DeadlinePassed {
                    path: target.path.clone(),
                });
            }
            let excluded = match exclusion {
                Ok(excluded) => excluded,
                Err(error) => return self.run_error(error),
            };
            if let Some(profile) = excluded {
                self.skip(
                    &target.path,
                    &Skip::new(
                        Reason::Excluded,
                        target.path.clone(),
                        format!("overlaps !profile:{profile}"),
                    ),
                );
                continue;
            }
            if let Some(skip) = other_fs(target, self.measured.dev) {
                self.skip(&target.path, &skip);
                continue;
            }
            if !untallied.is_empty() {
                untallied.push(target);
                continue;
            }
            let started = self.host.now();
            let result = cargo_target::tally(self.host, target, rules, tally_until);
            self.record_overrun("tally", &target.path, started, self.host.now());
            if self.past_deadline() {
                return self.stop(&Stop::DeadlinePassed {
                    path: target.path.clone(),
                });
            }
            match result {
                Ok(Ok(tally)) => tallied.push((target, tally)),
                Ok(Err(skip)) => self.skip(&target.path, &skip),
                Err(Stop::DeadlinePassed { .. }) if !self.past_deadline() => {
                    self.budget_spent(&target.path);
                    untallied.push(target);
                }
                Err(stop) => return self.stop(&stop),
            }
        }
        if self.tier.order == Order::Oldest {
            oldest_first(&mut tallied, |(_, tally)| tally.last_build);
        }
        let mut queue = CandidateQueue::new(
            self.tier.order,
            tallied
                .into_iter()
                .map(|(target, tally)| (target, Some(tally)))
                .chain(untallied.into_iter().map(|t| (t, None))),
            |(_, tally): &(_, Option<Tally>)| tally.as_ref().map(|t| (t.bytes, 1)),
        );
        let started = self.host.now();
        let sampled = sample(self.host, self.measured);
        self.record_overrun("statvfs", &self.measured.path, started, self.host.now());
        if self.past_deadline() {
            return self.stop(&Stop::DeadlinePassed {
                path: self.measured.path.clone(),
            });
        }
        let mut avail = match sampled {
            Ok(space) => space.avail_bytes,
            Err(e) => {
                // Without a sample, the goal cannot be observed: reclaim
                // nothing.
                return self.host_failure(e);
            }
        };
        while !queue.is_empty() {
            if self.stop_if_interrupted() {
                return;
            }
            if self.goal_reached(avail) {
                return;
            }
            let (target, tally) = queue
                .pop(self.measured.target.saturating_sub(avail))
                .expect("nonempty candidate queue");
            if self.protected_before_reclaim(&target.path) {
                if self.run_stopped {
                    return;
                }
                continue;
            }
            if self.stop_if_interrupted() {
                return;
            }
            let idle = IdleWindow {
                at_least: self.tier.idle_for,
                before: self.tier.idle_before,
            };
            if let Some(tally) = &tally
                && let Err(skip) = tally.judge_idle(self.host, &target.path, idle)
            {
                self.skip_as_tallied(
                    &target.path,
                    &self.tier.profile.clone(),
                    &skip,
                    Some(tally.bytes),
                );
                continue;
            }
            if self.too_busy_to_check(&target.path, &self.tier.profile.clone()) {
                continue;
            }
            (self.log)(&match &tally {
                Some(tally) => format!(
                    "{}: checking {} (est. {}, {} entries, last build {})",
                    self.tier.name,
                    target.path.display(),
                    format_bytes(tally.bytes),
                    tally.entries,
                    observed_age(self.host.wall_clock(), tally.last_build)
                ),
                None => format!(
                    "{}: checking {} (not tallied)",
                    self.tier.name,
                    target.path.display()
                ),
            });
            let started = self.host.now();
            let idle_age_seconds = age_seconds(
                self.host.wall_clock(),
                tally.as_ref().and_then(|t| t.last_build),
            );
            // An accepted tally's age is a precheck, not a freshness bound:
            // the protocol still measures the target under its locks and
            // applies the idle requirement again before deleting.
            let done = cargo_target::reclaim_checked(
                self.host,
                target,
                cargo_target::ReclaimPolicy {
                    judged_last_build: None,
                    rules,
                    idle,
                    deadline: self.deadline,
                },
                checker,
                &mut |path, at| {
                    DeletePolicy {
                        protections: &self.tier.protections,
                        protected: &self.protection.matches,
                        negated: &self.tier.negated,
                        negated_matches: &negated_matches,
                        enumerated: self.protection.enumerated,
                    }
                    .skip(self.host, self.deadline, path, at)
                },
            );
            let held = format!(
                ", Cargo locks held {} ms",
                done.locks_held.map_or(0, |d| d.as_millis())
            );
            let outcome = Outcome {
                started,
                before: avail,
                expected_bytes: tally.as_ref().map(|t| t.bytes),
                tallied_bytes: tally.as_ref().map(|t| t.bytes),
                idle_age_seconds,
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
        let started = self.host.now();
        let (found, stopped) = fs_kind::discover(self.host, &self.tier.roots, find, self.deadline);
        self.record_overrun("discover", &self.measured.path, started, self.host.now());
        if self.tier.roots.is_empty() {
            assert!(
                found.candidates.is_empty(),
                "a tier without roots finds nothing"
            );
            (self.log)(&nothing_searched(self.tier));
        } else {
            (self.log)(&format!(
                "{}: `{}`: {} candidate(s) below {} ({} found again through another root or \
                 pattern, {} systemd-private-* passed over)",
                self.tier.name,
                self.tier.select,
                found.candidates.len(),
                roots_text(&self.tier.roots),
                found.duplicates.len(),
                found.private_tmp,
            ));
        }
        if let Some(stop) = stopped {
            // An incomplete discovery is not the tier's set: nothing is
            // reclaimed from it.
            return self.stop(&stop);
        }
        if self.past_deadline() {
            return self.stop(&Stop::DeadlinePassed {
                path: self.measured.path.clone(),
            });
        }
        if !self.refusals(&found.refused) {
            return;
        }
        let tally_until = self.tally_until();
        let mut tallied: Vec<(&fs_kind::Found, fs_kind::Tally)> = Vec::new();
        let mut untallied: Vec<&fs_kind::Found> = Vec::new();
        for (path, candidate) in &found.candidates {
            if self.stop_if_interrupted() {
                return;
            }
            let started = self.host.now();
            let overlap = overlaps_checked(self.host, path, &self.protection.matches);
            self.record_overrun("protect", path, started, self.host.now());
            if self.past_deadline() {
                return self.stop(&Stop::DeadlinePassed { path: path.clone() });
            }
            let protected = match overlap {
                Ok(protected) => protected,
                Err(error) => return self.run_error(error),
            };
            if protected {
                self.skip(
                    path,
                    &Skip::new(
                        Reason::Protected,
                        path.clone(),
                        "overlaps a protected match".to_owned(),
                    ),
                );
                continue;
            }
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
            let started = self.host.now();
            let result = fs_kind::tally(self.host, &judge, found, tally_until);
            self.record_overrun("tally", path, started, self.host.now());
            if self.past_deadline() {
                return self.stop(&Stop::DeadlinePassed { path: path.clone() });
            }
            match result {
                Ok(Ok(tally)) => tallied.push((found, tally)),
                Ok(Err(skip)) => self.skip(path, &skip),
                Err(Stop::DeadlinePassed { .. }) if !self.past_deadline() => {
                    self.budget_spent(path);
                    untallied.push(found);
                }
                Err(stop) => return self.stop(&stop),
            }
        }
        if self.tier.order == Order::Oldest {
            oldest_first(&mut tallied, |(_, tally)| tally.newest_change);
        }
        let mut queue = CandidateQueue::new(
            self.tier.order,
            tallied
                .into_iter()
                .map(|(found, tally)| (found, Some(tally)))
                .chain(untallied.into_iter().map(|f| (f, None))),
            |(_, tally): &(_, Option<fs_kind::Tally>)| tally.as_ref().map(|t| (t.bytes, 1)),
        );
        let started = self.host.now();
        let sampled = sample(self.host, self.measured);
        self.record_overrun("statvfs", &self.measured.path, started, self.host.now());
        if self.past_deadline() {
            return self.stop(&Stop::DeadlinePassed {
                path: self.measured.path.clone(),
            });
        }
        let mut avail = match sampled {
            Ok(space) => space.avail_bytes,
            Err(e) => return self.host_failure(e),
        };
        while !queue.is_empty() {
            if self.stop_if_interrupted() {
                return;
            }
            if self.goal_reached(avail) {
                return;
            }
            let (found, tally) = queue
                .pop(self.measured.target.saturating_sub(avail))
                .expect("nonempty candidate queue");
            if self.protected_before_reclaim(&found.path) {
                if self.run_stopped {
                    return;
                }
                continue;
            }
            if self.stop_if_interrupted() {
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
            if self.too_busy_to_check(&found.path, &self.tier.profile.clone()) {
                continue;
            }
            (self.log)(&match &tally {
                Some(tally) => format!(
                    "{}: checking {} (est. {}, {} entries, newest change {})",
                    self.tier.name,
                    found.path.display(),
                    format_bytes(tally.bytes),
                    tally.entries,
                    observed_age(self.host.wall_clock(), tally.newest_change)
                ),
                None => format!(
                    "{}: checking {} (not tallied)",
                    self.tier.name,
                    found.path.display()
                ),
            });
            let started = self.host.now();
            let idle_age_seconds = age_seconds(
                self.host.wall_clock(),
                tally.as_ref().and_then(|t| t.newest_change),
            );
            let outcome = match action {
                Action::Delete { make_writable } => {
                    let done = fs_kind::reclaim_alternatives_checked(
                        self.host,
                        &[fs_kind::Alternative {
                            judge,
                            found,
                            checkouts: judge.rules.checkouts,
                            admitted: true,
                        }],
                        *make_writable,
                        checker,
                        self.deadline,
                        &mut |_, path, at| {
                            DeletePolicy {
                                protections: &self.tier.protections,
                                protected: &self.protection.matches,
                                negated: &[],
                                negated_matches: &[],
                                enumerated: self.protection.enumerated,
                            }
                            .skip(self.host, self.deadline, path, at)
                        },
                    );
                    Outcome {
                        started,
                        before: avail,
                        expected_bytes: tally.as_ref().map(|t| t.bytes),
                        tallied_bytes: tally.as_ref().map(|t| t.bytes),
                        idle_age_seconds,
                        removed_entries: done.removed_entries,
                        attempted_command: false,
                        end: done.end,
                        note: String::new(),
                    }
                }
                Action::Command { argv, cwd } => {
                    let ready = self.command_ready.as_ref().expect("command preflighted");
                    let done = fs_kind::run_command_checked(
                        self.host,
                        &judge,
                        found,
                        checker,
                        fs_kind::CommandSpec {
                            ready,
                            argv,
                            cwd: *cwd,
                            need_bytes: self.measured.target.saturating_sub(avail),
                            mount: &self.measured.at.mount.mount_point,
                            sources: vec![fs_kind::CommandSource {
                                profile: &self.tier.profile,
                                output_prefix: format!("{}/{}", self.tier.name, self.tier.profile),
                            }],
                            deadline: self.deadline,
                        },
                        &mut |path| {
                            policy_skip(self.host, &self.tier.protections, &[], self.deadline, path)
                        },
                    );
                    Outcome {
                        started,
                        before: avail,
                        expected_bytes: Some(self.measured.target.saturating_sub(avail)),
                        tallied_bytes: tally.as_ref().map(|t| t.bytes),
                        idle_age_seconds,
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
        let started = self.host.now();
        let (candidates, refused, stopped) = discover_fs_union(self.host, &sources, self.deadline);
        self.record_overrun("discover", &self.measured.path, started, self.host.now());
        if sources.iter().all(|source| source.roots.is_empty()) {
            (self.log)(&nothing_searched(self.tier));
        } else {
            (self.log)(&format!(
                "{}: `{}`: {} distinct filesystem candidate(s) from {} source term(s)",
                self.tier.name,
                self.tier.select,
                candidates.len(),
                sources.len()
            ));
        }
        if let Some(stop) = stopped {
            return self.stop(&stop);
        }
        if self.past_deadline() {
            return self.stop(&Stop::DeadlinePassed {
                path: self.measured.path.clone(),
            });
        }
        let mut misconfigured = false;
        for (index, skip) in &refused {
            let source = &sources[*index];
            if is_misconfigured_source(source, skip) {
                let mut entry = safety_error(self.tier, excluded(skip));
                entry.profile = Some(source.profile.to_owned());
                record_error(self.report, self.log, entry);
                misconfigured = true;
            } else {
                self.skip_as(&skip.path, source.profile, skip);
            }
        }
        if misconfigured {
            return;
        }
        let started = self.host.now();
        let scanned = scan_union_negated(self.host, &sources, self.deadline);
        self.record_overrun("exclude", &self.measured.path, started, self.host.now());
        if self.past_deadline() {
            return self.stop(&Stop::DeadlinePassed {
                path: self.measured.path.clone(),
            });
        }
        let negated_matches = match scanned {
            Ok(matches) => matches,
            Err(error) => return self.run_error(error),
        };
        let negated_count: usize = sources.iter().map(|source| source.negated.len()).sum();
        if negated_count != 0 {
            (self.log)(&format!(
                "{}: {} negated profile(s), {} raw match(es) scanned before reclaim",
                self.tier.name,
                negated_count,
                negated_matches
                    .iter()
                    .flatten()
                    .map(Vec::len)
                    .sum::<usize>()
            ));
        }
        let tally_until = self.tally_until();
        let mut tallied: Vec<(&FsUnionCandidate, fs_kind::Tally)> = Vec::new();
        let mut untallied: Vec<&FsUnionCandidate> = Vec::new();
        for candidate in &candidates {
            let path = &candidate.found_by[0].1.path;
            let started = self.host.now();
            let overlap = overlaps_checked(self.host, path, &self.protection.matches);
            self.record_overrun("protect", path, started, self.host.now());
            if self.past_deadline() {
                return self.stop(&Stop::DeadlinePassed { path: path.clone() });
            }
            let protected = match overlap {
                Ok(protected) => protected,
                Err(error) => return self.run_error(error),
            };
            if protected {
                self.skip(
                    path,
                    &Skip::new(
                        Reason::Protected,
                        path.clone(),
                        "overlaps a protected match".to_owned(),
                    ),
                );
                continue;
            }
            if !untallied.is_empty() {
                untallied.push(candidate);
                continue;
            }
            let started = self.host.now();
            let guard = union_guard_veto(
                self.host,
                candidate,
                &sources,
                base_rules,
                self.measured.dev,
                tally_until,
            );
            self.record_overrun("retention", path, started, self.host.now());
            if self.past_deadline() {
                return self.stop(&Stop::DeadlinePassed { path: path.clone() });
            }
            match guard {
                Ok(Some((index, skip))) => {
                    let source_index = candidate.guard_by[index].0;
                    self.skip_as(&skip.path, sources[source_index].profile, &skip);
                    continue;
                }
                Ok(None) => {}
                Err(Stop::DeadlinePassed { .. }) if !self.past_deadline() => {
                    self.budget_spent(path);
                    untallied.push(candidate);
                    continue;
                }
                Err(stop) => return self.stop(&stop),
            }
            let evaluation_rules = Rules {
                checkouts: Checkouts::Allow,
                ..*base_rules
            };
            let mut first_skip = None;
            let mut tally = None;
            let mut admitted = Vec::new();
            let mut checkout_marker = None;
            let mut budget_exhausted = false;
            for (index, found) in &candidate.found_by {
                let source = &sources[*index];
                let started = self.host.now();
                let exclusion = excluded_by(
                    self.host,
                    source.negated,
                    &negated_matches[*index],
                    &found.path,
                );
                self.record_overrun("exclude", &found.path, started, self.host.now());
                if self.past_deadline() {
                    return self.stop(&Stop::DeadlinePassed {
                        path: found.path.clone(),
                    });
                }
                let excluded = match exclusion {
                    Ok(excluded) => excluded,
                    Err(error) => return self.run_error(error),
                };
                if let Some(profile) = excluded {
                    first_skip.get_or_insert((
                        *index,
                        found.path.as_path(),
                        Skip::new(
                            Reason::Excluded,
                            found.path.clone(),
                            format!("overlaps !profile:{profile}"),
                        ),
                    ));
                    continue;
                }
                let judge = fs_kind::Judge {
                    find: source.find,
                    rules: &evaluation_rules,
                    idle: source.idle,
                    dev: self.measured.dev,
                };
                let started = self.host.now();
                let result = fs_kind::tally_for_union(self.host, &judge, found, tally_until);
                self.record_overrun("tally", &found.path, started, self.host.now());
                if self.past_deadline() {
                    return self.stop(&Stop::DeadlinePassed {
                        path: found.path.clone(),
                    });
                }
                match result {
                    Ok(Ok(value)) => {
                        admitted.push(*index);
                        if source.checkouts == Checkouts::Refuse && value.contains_checkout {
                            checkout_marker.get_or_insert((*index, found.path.as_path()));
                        }
                        if tally.is_none() {
                            tally = Some(value);
                        }
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
            if budget_exhausted {
                continue;
            }
            if let Some(tally) = tally {
                if union_checkouts(&sources, &admitted) == Checkouts::Refuse
                    && let Some((refusing_index, skip)) =
                        union_checkout_refusal(&sources, candidate, &admitted, checkout_marker)
                {
                    self.skip_as(path, sources[refusing_index].profile, &skip);
                    continue;
                }
                tallied.push((candidate, tally));
            } else {
                let (index, path, skip) =
                    first_skip.expect("a union candidate has a tally verdict");
                self.skip_as(path, sources[index].profile, &skip);
            }
        }
        if self.tier.order == Order::Oldest {
            oldest_first(&mut tallied, |(_, tally)| tally.newest_change);
        }
        let mut queue = CandidateQueue::new(
            self.tier.order,
            tallied
                .into_iter()
                .map(|(candidate, tally)| (candidate, Some(tally)))
                .chain(untallied.into_iter().map(|candidate| (candidate, None))),
            |(_, tally): &(_, Option<fs_kind::Tally>)| tally.as_ref().map(|t| (t.bytes, 1)),
        );
        let started = self.host.now();
        let sampled = sample(self.host, self.measured);
        self.record_overrun("statvfs", &self.measured.path, started, self.host.now());
        if self.past_deadline() {
            return self.stop(&Stop::DeadlinePassed {
                path: self.measured.path.clone(),
            });
        }
        let mut avail = match sampled {
            Ok(space) => space.avail_bytes,
            Err(error) => return self.host_failure(error),
        };
        while !queue.is_empty() {
            if self.stop_if_interrupted() {
                return;
            }
            if self.goal_reached(avail) {
                return;
            }
            let (candidate, tally) = queue
                .pop(self.measured.target.saturating_sub(avail))
                .expect("nonempty candidate queue");
            let path = &candidate.found_by[0].1.path;
            if self.protected_before_reclaim(path) {
                if self.run_stopped {
                    return;
                }
                continue;
            }
            if self.stop_if_interrupted() {
                return;
            }
            if self.past_deadline() {
                self.deadline_hit = true;
                (self.log)(&format!(
                    "{}: deadline passed before {}",
                    self.tier.name,
                    path.display()
                ));
                return;
            }
            let first_profile = sources[candidate.found_by[0].0].profile;
            if self.too_busy_to_check(path, first_profile) {
                continue;
            }
            (self.log)(&format!(
                "{}: checking {} ({}), found by {} source term(s)",
                self.tier.name,
                path.display(),
                tally
                    .as_ref()
                    .map_or_else(|| "not tallied".to_owned(), |t| format_bytes(t.bytes)),
                candidate.found_by.len(),
            ));
            let evaluation_rules = Rules {
                checkouts: Checkouts::Allow,
                ..*base_rules
            };
            let contributors = candidate.contributors();
            let alternatives: Vec<_> = contributors
                .iter()
                .map(|(index, found, admitted)| {
                    let source = &sources[*index];
                    fs_kind::Alternative {
                        judge: fs_kind::Judge {
                            find: source.find,
                            rules: &evaluation_rules,
                            idle: source.idle,
                            dev: self.measured.dev,
                        },
                        found,
                        checkouts: source.checkouts,
                        admitted: *admitted,
                    }
                })
                .collect();
            let started = self.host.now();
            let idle_age_seconds = age_seconds(
                self.host.wall_clock(),
                tally.as_ref().and_then(|t| t.newest_change),
            );
            let (selected, removed_entries, attempted_command, end) = match &self.tier.action {
                Action::Delete { make_writable } => {
                    let done = fs_kind::reclaim_alternatives_checked(
                        self.host,
                        &alternatives,
                        *make_writable,
                        checker,
                        self.deadline,
                        &mut |selected, path, at| {
                            let index = contributors[selected].0;
                            DeletePolicy {
                                protections: &self.tier.protections,
                                protected: &self.protection.matches,
                                negated: sources[index].negated,
                                negated_matches: &negated_matches[index],
                                enumerated: self.protection.enumerated,
                            }
                            .skip(self.host, self.deadline, path, at)
                        },
                    );
                    (done.selected, done.removed_entries, false, done.end)
                }
                Action::Command { argv, cwd } => {
                    let command_sources: Vec<_> = contributors
                        .iter()
                        .map(|(index, _, _)| {
                            let profile = sources[*index].profile;
                            fs_kind::CommandSource {
                                profile,
                                output_prefix: format!("{}/{}", self.tier.name, profile),
                            }
                        })
                        .collect();
                    let done = fs_kind::run_command_alternatives_checked(
                        self.host,
                        &alternatives,
                        checker,
                        fs_kind::CommandSpec {
                            ready: self.command_ready.as_ref().expect("command preflighted"),
                            argv,
                            cwd: *cwd,
                            need_bytes: self.measured.target.saturating_sub(avail),
                            mount: &self.measured.at.mount.mount_point,
                            sources: command_sources,
                            deadline: self.deadline,
                        },
                        &mut |selected, path| {
                            let source = &sources[contributors[selected].0];
                            policy_skip(
                                self.host,
                                &self.tier.protections,
                                source.negated,
                                self.deadline,
                                path,
                            )
                        },
                    );
                    (done.selected, 0, done.attempted, done.end)
                }
                _ => unreachable!("filesystem unions admit delete or command"),
            };
            let profile = selected
                .map(|selected| sources[contributors[selected].0].profile.to_owned())
                .unwrap_or_else(|| sources[candidate.found_by[0].0].profile.to_owned());
            let reported_path = selected
                .map(|selected| &contributors[selected].1.path)
                .unwrap_or(path);
            let outcome = Outcome {
                started,
                before: avail,
                expected_bytes: if attempted_command {
                    Some(self.measured.target.saturating_sub(avail))
                } else {
                    tally.as_ref().map(|t| t.bytes)
                },
                tallied_bytes: tally.as_ref().map(|t| t.bytes),
                idle_age_seconds,
                removed_entries,
                attempted_command,
                end,
                note: String::new(),
            };
            match self.record_as(reported_path, &profile, outcome) {
                Some(after) => avail = after,
                None => return,
            }
        }
    }

    fn budget_spent(&mut self, at: &Path) {
        self.report.tally_cut_short = true;
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

    fn record_overrun(&mut self, operation: &str, path: &Path, started: MonoTime, ended: MonoTime) {
        let Some(deadline) = self.deadline else {
            return;
        };
        if started <= deadline && ended > deadline {
            self.report.overruns.push(OverrunEntry {
                operation: operation.to_owned(),
                path: path.to_owned(),
                over_ms: u64::try_from(ended.saturating_since(deadline).as_millis())
                    .unwrap_or(u64::MAX),
            });
        }
    }

    fn record_as(&mut self, candidate: &Path, profile: &str, outcome: Outcome) -> Option<u64> {
        let action_finished = self.host.now();
        let took = action_finished.saturating_since(outcome.started);
        self.record_overrun("reclaim", candidate, outcome.started, action_finished);
        let sample_started = self.host.now();
        let mut after = match sample(self.host, self.measured) {
            Ok(space) => Some(space.avail_bytes),
            Err(e) => {
                // Unmeasured: the change is unknown (null), never 0, and
                // the goal can no longer be observed: the run stops below.
                self.host_failure(e);
                None
            }
        };
        self.record_overrun(
            "statvfs",
            &self.measured.path,
            sample_started,
            self.host.now(),
        );
        // Filesystems may release blocks after unlink returns. Recheck for at
        // most two seconds when this action has freed less than its tally (or
        // the remaining target for an action without a tally). The goal is
        // decided only by the last observed sample.
        let acted = outcome.removed_entries > 0 || outcome.attempted_command;
        if acted && !self.run_stopped && !self.past_deadline() && !self.stop_if_interrupted() {
            let expected = outcome
                .expected_bytes
                .unwrap_or_else(|| self.measured.target.saturating_sub(outcome.before));
            let window_end = self.host.now().checked_add(Duration::from_secs(2));
            if let (Some(mut observed), Some(window_end)) = (after, window_end) {
                while observed < self.measured.target
                    && observed.saturating_sub(outcome.before) < expected
                    && self.host.now() < window_end
                {
                    if self.stop_if_interrupted() || self.past_deadline() {
                        break;
                    }
                    let remaining = window_end.saturating_since(self.host.now());
                    let wait = Duration::from_millis(250).min(remaining);
                    let wait = self.deadline.map_or(wait, |deadline| {
                        wait.min(deadline.saturating_since(self.host.now()))
                    });
                    if wait.is_zero() {
                        break;
                    }
                    self.host.sleep(wait);
                    if self.stop_if_interrupted() || self.past_deadline() {
                        break;
                    }
                    let retry_started = self.host.now();
                    match sample(self.host, self.measured) {
                        Ok(space) => {
                            observed = space.avail_bytes;
                            after = Some(observed);
                        }
                        Err(error) => {
                            self.host_failure(error);
                            after = None;
                            break;
                        }
                    }
                    self.record_overrun(
                        "statvfs",
                        &self.measured.path,
                        retry_started,
                        self.host.now(),
                    );
                }
            }
        }
        if self.past_deadline() {
            self.deadline_hit = true;
            self.run_stopped = true;
        }
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
                idle_age_seconds: outcome.idle_age_seconds,
                count: 1,
            });
        }
        let in_use_unknown = matches!(
            &outcome.end,
            ReclaimEnd::Skipped(skip) if skip.reason == Reason::InUseUnknown
        );
        match outcome.end {
            ReclaimEnd::Complete => {}
            ReclaimEnd::Skipped(skip) => {
                self.skip_as_tallied(candidate, profile, &skip, outcome.tallied_bytes)
            }
            ReclaimEnd::Failed { path, error } => {
                // An error for this candidate only (an `ENOTEMPTY` from a
                // file created after its walk, ...): reported, never
                // retried in this run; the next candidate is still tried.
                self.error_as(candidate, profile, format!("{}: {error}", path.display()));
            }
            ReclaimEnd::GateRefused { path, error } => {
                self.run_stopped = true;
                self.error_as(candidate, profile, format!("{}: {error}", path.display()));
                (self.log)(&format!(
                    "{}: the sandbox gate refused: nothing more is deleted",
                    self.tier.name
                ));
                return None;
            }
            ReclaimEnd::Stopped(stop) => {
                self.run_stopped = true;
                self.stop(&stop);
                return None;
            }
        }
        // After the skip's own log line, so the give-up line follows it.
        if in_use_unknown {
            self.in_use_unknown_in_a_row += 1;
            if self.in_use_unknown_in_a_row == IN_USE_UNKNOWN_IN_A_ROW_MAX {
                (self.log)(&format!(
                    "{}: {} candidates in a row could not be checked (the host is too busy): \
                     the rest of this tier is skipped unchecked",
                    self.tier.name, self.in_use_unknown_in_a_row
                ));
            }
        } else {
            self.in_use_unknown_in_a_row = 0;
        }
        if self.run_stopped { None } else { after }
    }
}

/// How one candidate's reclaim went, for [`Reclaim::record`].
struct Outcome {
    started: MonoTime,
    /// The available space before it.
    before: u64,
    /// Allocated bytes this action was expected to release, if tallied.
    expected_bytes: Option<u64>,
    /// Candidate size only after a complete tally; a command's expected
    /// target delta is deliberately separate from this.
    tallied_bytes: Option<u64>,
    idle_age_seconds: Option<u64>,
    removed_entries: usize,
    attempted_command: bool,
    end: ReclaimEnd,
    /// Appended to the action's log line.
    note: String,
}

#[cfg(test)]
mod operator_message_tests {
    use super::{Reason, Skip, Tier, Timestamp, log_skip, observed_age};
    use crate::profile::Library;
    use std::path::PathBuf;

    #[test]
    fn age_is_relative_and_future_time_is_explicit() {
        let now = Timestamp {
            secs: 200_000,
            nanos: 0,
        };
        assert_eq!(observed_age(now, None), "unknown");
        assert_eq!(
            observed_age(
                now,
                Some(Timestamp {
                    secs: 113_600,
                    nanos: 0
                })
            ),
            "1 d ago"
        );
        assert_eq!(
            observed_age(
                now,
                Some(Timestamp {
                    secs: 200_001,
                    nanos: 0
                })
            ),
            "in the future"
        );
    }

    #[test]
    fn in_use_log_is_bounded_without_guessing_evidence_count() {
        let library = Library::builtin().expect("built-ins");
        let tier = Tier::select(
            "profile:cargo-target",
            &library,
            Some(&["/tmp".to_owned()]),
            1000,
        )
        .expect("selection");
        let detail = format!(
            "pid 1 (foo; bar): {}; pid 2 has a target fd",
            "x".repeat(300)
        );
        let skip = Skip::new(Reason::InUse, PathBuf::from("/tmp/target"), detail.clone());
        let mut messages = Vec::new();
        log_skip(
            &mut |message| messages.push(message.to_owned()),
            &tier,
            &skip,
        );
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("pid 1 (foo; bar): "));
        assert!(messages[0].ends_with("… (more detail omitted)"));
        assert!(!messages[0].contains("pid 2 has a target fd"));
        assert_eq!(
            skip.detail, detail,
            "log shortening preserves source evidence"
        );
    }

    #[test]
    fn missing_roots_names_the_cli_remedy() {
        let library = Library::builtin().expect("built-ins");
        let error = Tier::select("profile:cargo-target", &library, None, 1000)
            .expect_err("cargo target requires roots");
        assert!(
            error.contains("no roots were given; add --roots DIR"),
            "{error}"
        );
    }
}

#[cfg(test)]
mod best_fit_tests {
    use super::{
        CandidateQueue, Dev, DevIno, MountEntry, Order, overlaps_with, screen_overlap_mounts,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn covering_fallback_and_bucket_ties() {
        let estimates = [Some((8, 2)), Some((4, 1)), Some((8, 1)), None];
        let mut queue = CandidateQueue::new(Order::BestFit, estimates, |unit| *unit);
        assert_eq!(queue.pop(3), Some(Some((4, 1))));
        assert_eq!(queue.pop(6), Some(Some((8, 1))));
        assert_eq!(queue.pop(9), Some(Some((8, 2))));
        assert_eq!(queue.pop(1), Some(None));
        assert!(queue.is_empty());
    }

    #[test]
    fn oldest_and_unknown_estimates_keep_their_input_order() {
        let units = [(0, None), (1, Some((4, 1))), (2, None), (3, Some((4, 1)))];
        let mut oldest = CandidateQueue::new(Order::Oldest, units, |(_, estimate)| *estimate);
        let oldest_ids: Vec<_> = (0..4).map(|_| oldest.pop(100).unwrap().0).collect();
        assert_eq!(oldest_ids, [0, 1, 2, 3]);

        let mut best_fit = CandidateQueue::new(Order::BestFit, units, |(_, estimate)| *estimate);
        let fit_ids: Vec<_> = (0..4).map(|_| best_fit.pop(100).unwrap().0).collect();
        assert_eq!(fit_ids, [1, 3, 0, 2]);
    }

    #[test]
    fn overlap_follows_same_device_bind_aliases() {
        let id = |path: &Path| -> Result<DevIno, String> {
            let ino = match path.to_str().unwrap() {
                "/" => 1,
                "/work" | "/alias" => 10,
                "/work/build" | "/alias/build" => 11,
                "/work/build/cache" | "/alias/build/cache" => 12,
                "/work/other" | "/alias/other" => 13,
                other => return Err(format!("unexpected path {other}")),
            };
            Ok(DevIno {
                dev: Dev { major: 8, minor: 1 },
                ino,
            })
        };
        assert!(
            overlaps_with(
                Path::new("/work/build"),
                &[PathBuf::from("/alias/build/cache")],
                id
            )
            .unwrap()
        );
        assert!(
            !overlaps_with(
                Path::new("/work/other"),
                &[PathBuf::from("/alias/build/cache")],
                id
            )
            .unwrap()
        );
        assert!(
            overlaps_with(
                Path::new("/work/build"),
                &[PathBuf::from("/alias/build")],
                id
            )
            .unwrap()
        );
    }

    #[test]
    fn overlap_identity_uncertainty_fails_closed() {
        let error = overlaps_with(
            Path::new("/work/build"),
            &[PathBuf::from("/alias/build")],
            |_| Err("cannot stat".to_owned()),
        );
        assert_eq!(error.unwrap_err(), "cannot stat");
    }

    #[test]
    fn overlap_screens_network_ancestor_before_local_bind_root() {
        let mount = |id, point: &str, fs_type: &str| MountEntry {
            id,
            parent_id: 1,
            dev: Dev { major: 8, minor: 1 },
            fs_root: PathBuf::from("/"),
            mount_point: PathBuf::from(point),
            fs_type: fs_type.to_owned(),
            source: "fixture".to_owned(),
            read_only: false,
        };
        let mounts = [
            mount(1, "/", "ext4"),
            mount(2, "/remote", "nfs"),
            mount(3, "/remote/alias", "ext4"),
        ];
        let error = screen_overlap_mounts(
            &mounts,
            Path::new("/remote/alias/build"),
            &[PathBuf::from("/protected/build")],
            || Ok(()),
        )
        .unwrap_err();
        assert!(error.contains("/remote"));
        assert!(error.contains("nfs"));
    }
}
