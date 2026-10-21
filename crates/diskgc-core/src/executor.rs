//! The executor (docs/architecture.md, The reclaim walk): one selected tier
//! or configured ordered groups, with per-tier discovery and a shared host
//! lock. Ranking may use oldest, best-fit, or a seeded random order.
//! Later tasks add deadlines with overruns and signals (task-54) and the
//! index (task-31).
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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
use crate::fs_kind::{self, EntryFilter, Find};
use crate::host::{
    Cause, CommandExit, CommandInvocation, CommandRun, Delete, Dev, DevIno, Host, HostError,
    MonoTime, MountEntry, Space, Timestamp,
};
use crate::hostlock::{self, LockError};
use crate::inuse::{Checker, REUSE_LIMIT};
use crate::profile::{Action, Body, Finds, IdleFor, IdleWindow, Library, Where};
use crate::report::{
    ActionEntry, BuildEvidence, ErrorEntry, Final, HeldEntry, PlanEntry, Report, SkipEntry, Status,
    after_lock_timeout, conclude, signed_bytes,
};
use crate::resolve::{
    ResolveError, Resolved, Want, mount_for, resolve, resolve_in, screen_mount_chain,
};
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
    /// Bound protected source matches, shared by every tier of a policy.
    pub protections: Vec<ProtectedSource>,
    /// Raw source matches excluded from this term by overlap.
    pub negated: Vec<NegatedSource>,
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
        let bound = library
            .bind_roots(&checked, roots)
            .map_err(|e| format!("--select `{expr}`: {e}"))?;
        Self::from_checked(expr, &checked, library, &bound, uid)
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
        let not_yet = |what: &str, task: &str| {
            format!(
                "--select `{expr}`: {what} is not supported by ensure in this build yet \
                 ({task}); select one source profile (`profile:NAME`, kind cargo-target, or \
                 kind fs), optionally with supported filters, or a positive fs delete union"
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
            Action::Delete { .. } | Action::CargoLockProtocol
        ) && (!first.negated.is_empty() || terms.iter().any(|term| !term.negated.is_empty()))
        {
            return Err(not_yet("!profile: with this source action", "task-16"));
        }
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
        not_yet: &impl Fn(&str, &str) -> String,
    ) -> Result<Tier, String> {
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
        let mut negated = Vec::new();
        for factor in &term.factors {
            let (name, is_negated) = match factor {
                Factor::Filter(name) => (name, false),
                Factor::NotFilter(name) => (name, true),
                Factor::NotProfile(name) => {
                    let Some(Body::Source(other)) = library.get(name).map(|p| &p.body) else {
                        return Err(format!(
                            "--select `{expr}`: no negated source profile `{name}`"
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
                        _ => return Err(not_yet(&format!("`!profile:{name}`"), "task-16")),
                    };
                    let roots = bound
                        .get(name)
                        .ok_or_else(|| {
                            format!("--select `{expr}`: no bound roots for `!profile:{name}`")
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
                return Err(format!("--select `{expr}`: no filter profile `{name}`"));
            };
            if is_negated {
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

/// A foreign root cannot free space on PATH. Omit it from every source and
/// negated source before discovery, including union terms. Fatal root errors
/// still exclude the whole tier because discovery may be incomplete.
struct RootPreflight {
    errors: Vec<ErrorEntry>,
    skipped: Vec<SkipEntry>,
}

impl RootPreflight {
    fn filtered_tier(&self, tier: &Tier) -> Tier {
        let foreign: HashSet<&Path> = self.skipped.iter().map(|s| s.candidate.as_path()).collect();
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

/// Check every source root against PATH's device before enumerating candidates.
/// A missing root remains an empty source. A foreign root is reported and
/// skipped while same-device roots proceed. An unsafe or unreadable root
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
            tier: tier.name.clone(),
            profile: (*profile).to_owned(),
            candidate: root.display().to_string(),
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
            errors.push(run_error(
                tier,
                format!("cannot read mountinfo for root preflight: {error}; the tier is excluded"),
            ));
            return RootPreflight { errors, skipped };
        }
    };
    for (profile, root) in roots {
        let mut error = |message: String| {
            errors.push(ErrorEntry {
                tier: tier.name.clone(),
                profile: profile.to_owned(),
                candidate: root.display().to_string(),
                message,
            })
        };
        // A local bind mount can sit below an unsafe ancestor. Screen the
        // full mount chain before resolve_in opens any of it.
        if let Err(why) = screen_mount_chain(&mounts, root) {
            error(format!(
                "root {}: {why}; the tier is excluded",
                root.display()
            ));
            continue;
        }
        match resolve_in(host, &mounts, root, Want::Directory) {
            Ok(resolved) if resolved.stat.dev != requested_dev => skipped.push(SkipEntry {
                tier: tier.name.clone(),
                profile: profile.to_owned(),
                candidate: root.clone(),
                count: 1,
                examples: Vec::new(),
                reason: Reason::OtherFs.as_str(),
            }),
            Ok(_) | Err(ResolveError::Missing { .. }) => {}
            Err(ResolveError::UnsafePath { why, .. }) => error(format!(
                "root {}: unsafe_path (misconfigured, or the mounts changed): {why}; the tier is excluded",
                root.display()
            )),
            Err(resolve_error) => {
                error(format!(
                    "root {}: {resolve_error} (misconfigured, or the environment changed); the tier is excluded",
                    root.display()
                ));
            }
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
    let roots = preflight_roots(host, &request.tier, measured.dev);
    report.skipped.extend(roots.skipped.iter().cloned());
    for error in roots.errors.iter().cloned() {
        record_error(&mut report, log, error);
    }
    if !report.errors.is_empty() {
        report.avail_after_bytes = final_sample(host, &measured, &mut report, log, &request.tier);
        report.status = conclude(
            Final::of(report.avail_after_bytes, measured.min),
            false,
            true,
        );
        return report;
    }
    let tier = roots.filtered_tier(&request.tier);
    let protection_matches = match scan_protections(host, &request.tier.protections, deadline) {
        Ok(matches) => matches,
        Err(error) => {
            record_error(&mut report, log, run_error(&request.tier, error));
            report.avail_after_bytes =
                final_sample(host, &measured, &mut report, log, &request.tier);
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
        protection_matches: &protection_matches,
        report: &mut report,
        log,
        deadline_hit: false,
        run_stopped: false,
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
    let mut report = Report::new(Status::Error, Some(&request.path));
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
    let measured = match measure(host, request, &mut report) {
        Ok(measured) => measured,
        Err(error) => return usage(report, log, error),
    };
    let first = &tiers[0];
    let avail = report.avail_before_bytes.expect("measured");
    if avail >= measured.min {
        report.status = Status::Met;
        report.avail_after_bytes = Some(avail);
        warn_if_host_lock_unready(host, log);
        return report;
    }
    let lock = match hostlock::acquire(host, deadline) {
        Ok(lock) => lock,
        Err(LockError::DeadlinePassed) => {
            report.avail_after_bytes = final_sample(host, &measured, &mut report, log, first);
            report.status = after_lock_timeout(Final::of(report.avail_after_bytes, measured.min));
            return report;
        }
        Err(error) => return usage(report, log, error.to_string()),
    };
    log(&format!(
        "host lock {} taken after {} ms",
        hostlock::host_lock_path().display(),
        lock.waited.as_millis()
    ));
    match sample(host, &measured) {
        Ok(space) if space.avail_bytes >= measured.min => {
            report.status = Status::Met;
            report.avail_after_bytes = Some(space.avail_bytes);
            return report;
        }
        Ok(_) => {}
        Err(error) => {
            record_error(&mut report, log, run_error(first, error));
            return report;
        }
    }
    let protection_matches = match scan_protections(host, &first.protections, deadline) {
        Ok(matches) => matches,
        Err(error) => {
            record_error(&mut report, log, run_error(first, error));
            report.avail_after_bytes = final_sample(host, &measured, &mut report, log, first);
            return report;
        }
    };
    let mut prepared = Vec::with_capacity(tiers.len());
    for tier in tiers {
        if deadline.is_some_and(|end| host.now() >= end) {
            report.avail_after_bytes = final_sample(host, &measured, &mut report, log, tier);
            report.status = conclude(
                Final::of(report.avail_after_bytes, measured.min),
                true,
                !report.errors.is_empty(),
            );
            return report;
        }
        let roots = preflight_roots(host, tier, measured.dev);
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
            match command_action::preflight(host, argv) {
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
                        candidate: PathBuf::new(),
                        count: 1,
                        examples: Vec::new(),
                        reason: Reason::Missing.as_str(),
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
            match docker_backend::inspect_backend(host, measured.dev, docker_storage(tier)) {
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
                        candidate: PathBuf::new(),
                        count: 1,
                        examples: Vec::new(),
                        reason: skip.reason,
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
    for ((_, tier_timeout), ready) in tiers.iter().zip(tier_timeouts).zip(prepared) {
        let Some((tier, ready, docker_bound)) = ready else {
            continue;
        };
        let tier = &tier;
        let avail = match sample(host, &measured) {
            Ok(space) => space.avail_bytes,
            Err(error) => {
                record_error(&mut report, log, run_error(tier, error));
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
        let own_deadline = (!tier_timeout.is_zero())
            .then(|| host.now().checked_add(*tier_timeout))
            .flatten();
        let tier_deadline = match (deadline, own_deadline) {
            (Some(run), Some(own)) => Some(run.min(own)),
            (run, None) => run,
            (None, own) => own,
        };
        let mut run = Reclaim {
            host,
            tier,
            measured: &measured,
            deadline: tier_deadline,
            tally_budget: request.tally_budget,
            command_ready: ready,
            docker_bound,
            protection_matches: &protection_matches,
            report: &mut report,
            log,
            deadline_hit: false,
            run_stopped: false,
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
    report.avail_after_bytes = final_sample(host, &measured, &mut report, log, first);
    report.status = conclude(
        Final::of(report.avail_after_bytes, measured.min),
        deadline_hit,
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
    let deadline = deadline_of(host, request.timeout);
    let mut report = Report::new(Status::DryRun, Some(&request.path));
    let measured = match measure(host, request, &mut report) {
        Ok(m) => m,
        Err(e) => return usage(report, log, e),
    };
    let tier = &request.tier;
    let roots = preflight_roots(host, tier, measured.dev);
    report.plan.extend(roots.skipped.iter().map(|skip| {
        let mut entry = plan_entry(tier, &skip.candidate, Some(skip.reason), None);
        entry.profile = skip.profile.clone();
        entry
    }));
    if !roots.errors.is_empty() {
        for error in roots.errors.iter().cloned() {
            record_error(&mut report, log, error);
        }
        report.avail_after_bytes = final_sample(host, &measured, &mut report, log, tier);
        return report;
    }
    let tier = roots.filtered_tier(tier);
    let tier = &tier;
    let protection_matches = match scan_protections(host, &tier.protections, deadline) {
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
        match docker_backend::inspect_backend(host, measured.dev, docker_storage(tier)) {
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
                    protected.then_some(Reason::Protected.as_str()),
                    None,
                ));
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
    let Planned {
        mut eligible,
        skipped,
        stopped,
    } = planned;
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
    for entry in &mut report.plan {
        if entry.candidate.as_os_str().is_empty() {
            continue;
        }
        let protected = match overlaps_checked(host, &entry.candidate, &protection_matches) {
            Ok(protected) => protected,
            Err(error) => {
                record_error(&mut report, log, run_error(tier, error));
                report.status = Status::Error;
                report.plan.extend(root_skips);
                return report;
            }
        };
        if protected {
            log(&format!(
                "{} {}: skip protected (overlaps a protected source match)",
                tier.name,
                entry.candidate.display()
            ));
            entry.reason = Some(Reason::Protected.as_str());
            entry.estimated_bytes = None;
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
            run_error(tier, format!("stopped: {stop}")),
        );
    }
    // Unmeasured: unknown, not the earlier sample.
    report.avail_after_bytes = final_sample(host, &measured, &mut report, log, tier);
    report.held_by_deleted_files = held(host, &mut checker, measured.dev, &measured.at.mount, log);
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
    let Some(first) = tiers.first().filter(|_| tiers.len() == tier_timeouts.len()) else {
        return usage(
            Report::new(Status::DryRun, Some(&request.path)),
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
        // Only this tier's budget expired. Reclaim excludes that tier and
        // continues; a grouped plan must still show later tiers.
        report.status = Status::DryRun;
    }
    for (tier, timeout) in tiers.iter().zip(tier_timeouts).skip(1) {
        if run_deadline.is_some_and(|end| host.now() >= end) {
            report.status = Status::Deadline;
            break;
        }
        for_tier.tier = tier.clone();
        for_tier.timeout = bounded_tier_timeout(host, run_deadline, *timeout);
        let part = dry_run(host, &for_tier, owners, log);
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
        // A tier deadline alone does not stop the overall plan.
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
                    skipped.push(entry(&target.path, Some(Reason::Excluded.as_str()), None));
                    log(&format!(
                        "{}: {} overlaps !profile:{profile}",
                        tier.name,
                        target.path.display()
                    ));
                    continue;
                }
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
/// its final action. Protection is global; negation belongs to the selected
/// source term. Uncertain discovery or identity stops the run.
fn policy_skip<H: Host>(
    host: &H,
    protections: &[ProtectedSource],
    negated: &[NegatedSource],
    deadline: Option<MonoTime>,
    path: &Path,
) -> Result<Option<Skip>, Stop> {
    let failed = |what: &'static str, detail: String| Stop::Failed {
        path: path.to_owned(),
        error: HostError::invalid(what, path.display(), detail),
    };
    let protected = scan_protections(host, protections, deadline)
        .map_err(|error| failed("protect recheck", error))?;
    if overlaps_checked_until(host, path, &protected, deadline)
        .map_err(|error| failed("protect overlap", error))?
    {
        return Ok(Some(Skip::new(
            Reason::Protected,
            path.to_owned(),
            "overlaps a protected match at final action check".to_owned(),
        )));
    }
    let negated_matches =
        scan_negated(host, negated, deadline).map_err(|error| failed("!profile recheck", error))?;
    Ok(
        excluded_by_until(host, negated, &negated_matches, path, deadline)
            .map_err(|error| failed("!profile overlap", error))?
            .map(|profile| {
                Skip::new(
                    Reason::Excluded,
                    path.to_owned(),
                    format!("overlaps !profile:{profile} at final action check"),
                )
            }),
    )
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
    let negated_matches = match scan_union_negated(host, &sources, deadline) {
        Ok(matches) => matches,
        Err(error) => {
            record_error(report, log, run_error(tier, error));
            return planned;
        }
    };
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
    mount: &MountEntry,
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
    scan.held_by_deleted_files_on(&devices)
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
    docker_bound: Option<Bound>,
    protection_matches: &'a [PathBuf],
    report: &'a mut Report,
    log: &'a mut (dyn FnMut(&str) + 'l),
    deadline_hit: bool,
    run_stopped: bool,
}

impl<H: Delete> Reclaim<'_, '_, H> {
    /// Recheck immediately before entering a kind's reclaim protocol. A
    /// changed or unreadable protected tree stops the entire run.
    fn protected_before_reclaim(&mut self, candidate: &Path) -> bool {
        if self.tier.protections.is_empty() {
            return false;
        }
        match overlaps_checked_until(self.host, candidate, self.protection_matches, self.deadline) {
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
            Ok(false) => {}
            Err(error) => {
                self.run_error(error);
                return true;
            }
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
        let binding = if let Some(bound) = &self.docker_bound {
            Ok(Binding::Bound(bound.clone()))
        } else {
            docker_backend::inspect_backend(self.host, self.measured.dev, docker_storage(self.tier))
        };
        let bound = match binding {
            Ok(Binding::Bound(bound)) => bound,
            Ok(Binding::Skipped(skip)) => {
                self.skip_docker(Path::new(""), skip.reason, &skip.detail);
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
                self.tier_error(format!("recheck Docker backend: {error}"));
                return;
            }
        }
        if self.protected_before_reclaim(&candidate) {
            return;
        }
        let cwd = match resolve(self.host, Path::new("/"), Want::Directory) {
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

    /// An error of the run: recorded, and the run stops.
    fn stop(&mut self, stop: &Stop) {
        self.run_stopped = true;
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
                if self.tier.fs_union.is_empty() && self.tier.negated.is_empty() {
                    self.walk_fs(find, &rules, &mut checker);
                } else {
                    self.walk_fs_union(&rules, &mut checker);
                }
            }
            SourceKind::DockerBuildCache | SourceKind::DockerImages => self.walk_docker(),
        }
        self.report.held_by_deleted_files = held(
            self.host,
            &mut checker,
            self.measured.dev,
            &self.measured.at.mount,
            self.log,
        );
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
            // A bad configured root excludes this tier only. Other tiers
            // were independently preflighted and may still run.
            self.tier_error(excluded(skip));
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
        let negated_matches = match scan_negated(self.host, &self.tier.negated, self.deadline) {
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
            let protected = match overlaps_checked(self.host, &target.path, self.protection_matches)
            {
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
            let excluded = match excluded_by(
                self.host,
                &self.tier.negated,
                &negated_matches,
                &target.path,
            ) {
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
        let mut avail = match sample(self.host, self.measured) {
            Ok(space) => space.avail_bytes,
            Err(e) => {
                // Without a sample, the goal cannot be observed: reclaim
                // nothing.
                return self.run_error(e);
            }
        };
        while !queue.is_empty() {
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
            let done = cargo_target::reclaim_checked(
                self.host,
                target,
                cargo_target::ReclaimPolicy {
                    judged_last_build: None,
                    rules,
                    idle: IdleWindow {
                        at_least: self.tier.idle_for,
                        before: self.tier.idle_before,
                    },
                    deadline: self.deadline,
                },
                checker,
                &mut |path| {
                    policy_skip(
                        self.host,
                        &self.tier.protections,
                        &self.tier.negated,
                        self.deadline,
                        path,
                    )
                },
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
            let protected = match overlaps_checked(self.host, path, self.protection_matches) {
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
        let mut avail = match sample(self.host, self.measured) {
            Ok(space) => space.avail_bytes,
            Err(e) => return self.run_error(e),
        };
        while !queue.is_empty() {
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
                    let done = fs_kind::reclaim_alternatives_checked(
                        self.host,
                        &[fs_kind::Alternative { judge, found }],
                        *make_writable,
                        checker,
                        self.deadline,
                        &mut |_, path| {
                            policy_skip(self.host, &self.tier.protections, &[], self.deadline, path)
                        },
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
                            profile: &self.tier.profile,
                            output_prefix: format!("{}/{}", self.tier.name, self.tier.profile),
                            deadline: self.deadline,
                        },
                        &mut |path| {
                            policy_skip(self.host, &self.tier.protections, &[], self.deadline, path)
                        },
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
        let negated_matches = match scan_union_negated(self.host, &sources, self.deadline) {
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
            let protected = match overlaps_checked(self.host, path, self.protection_matches) {
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
                let excluded = match excluded_by(
                    self.host,
                    source.negated,
                    &negated_matches[*index],
                    &found.path,
                ) {
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
        let mut avail = match sample(self.host, self.measured) {
            Ok(space) => space.avail_bytes,
            Err(error) => return self.run_error(error),
        };
        while !queue.is_empty() {
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
            let done = fs_kind::reclaim_alternatives_checked(
                self.host,
                &alternatives,
                make_writable,
                checker,
                self.deadline,
                &mut |selected, path| {
                    let source = &sources[candidate.found_by[selected].0];
                    policy_skip(
                        self.host,
                        &self.tier.protections,
                        source.negated,
                        self.deadline,
                        path,
                    )
                },
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
