//! The executor (docs/architecture.md, The reclaim walk): one selected tier
//! or configured ordered groups, with per-tier discovery and a shared host
//! lock. Ranking may use oldest, best-fit, or a seeded random order.
//! Deadline and signal handling use the [`Host`] clock and cancellation
//! state. Task-31's index is separate.
//!
//! Two entry points, kept apart by the type system:
//!
//! - [`ensure`] reclaims (one entry point for `--select` and configured
//!   groups alike, module `entry`: `--select` is a group of one). It takes a
//!   [`Delete`] host, refuses an action it has no reclaim for, asks the
//!   pre-release sandbox gate ([`Delete::may_delete`]), then measures; at
//!   or above `--min-free` it returns at once, without the host lock.
//!   Otherwise it tallies, takes the host lock ([`crate::hostlock`]; a
//!   non-root run without one, its per-user lock),
//!   measures again, prepares every tier and runs each: discovery
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

mod entry;
mod indexed;
mod nix_walk;
mod plan;
mod policy;
mod preflight;
mod prune_backend;
mod tier;
mod tier_policy;

pub use entry::{
    ensure, ensure_grouped, ensure_grouped_with_index, ensure_with_index, reclaimable,
};
pub use indexed::tally_sources;
pub use plan::{dry_run, dry_run_grouped};
pub use preflight::{
    CAP_SYS_PTRACE, MeasureFailure, PrivilegeError, goal_bytes, privileges, process_visibility,
    reach_path,
};
pub use tier::{
    CompiledSelection, FsUnionSource, NegatedKind, NegatedSource, ProtectedKind, ProtectedSource,
    SELECT_TIER, SelectionFailure, SourceKind, Tier, bind_protections,
};
// Shared between the entry points here and the sibling modules, which
// import them by name from here (no glob: the source scan's sandbox-gate
// guard refuses glob imports).
use plan::{
    FsUnionCandidate, discover_fs_union, scan_negated, scan_union_negated, union_checkout_refusal,
    union_checkouts, union_guard_veto,
};
// The overlap verdict alone, public so the model tests can pin what it
// must refuse whatever discovery would hand it (a covered mount).
pub use policy::overlaps_checked;
use policy::{Refresh, judge_negated, judge_protect, policy_skip, scan_protections};
use preflight::{
    Measured, measure_failure, measure_until, nothing_searched, preflight_roots, roots_text,
    say_lock_readiness,
};
use prune_backend::{Bound, Which};
use tier::FsUnionSourceRef;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backend::Binding;
use crate::cargo_target::{self, Discovery, ReclaimEnd, Stop, Tally, Target};
use crate::command_action::Ready;
use crate::config::Order;
use crate::engine::{Checkouts, Reason, Rules, Skip, UnknownCause};
use crate::fs_kind::{self, Find};
use crate::host::{
    Cause, CommandExit, CommandInvocation, CommandRun, Delete, Dev, Host, HostError, MonoTime,
    SecondSignal, Space, Timestamp,
};
use crate::hostlock::{self, HeldLock, LockError, ReclaimLock};
use crate::inuse::{Checker, REUSE_LIMIT};
use crate::profile::{Action, IdleWindow};
use crate::report::{
    ActionEntry, ErrorCause, ErrorEntry, HeldEntry, OverrunEntry, Report, SkipEntry, Status,
    signed_bytes,
};
use crate::resolve::{Want, resolve};
use crate::tally::TallyState;
use crate::units::{Size, format_age, format_bytes};
use entry::Backend;
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
    timed_report_from(host, None, run)
}

/// [`timed_report`] from an earlier start (`watch`'s triggering sample,
/// [`WatchCycle::origin`]), when given.
pub fn timed_report_from<H: Host>(
    host: &H,
    origin: Option<(MonoTime, Timestamp)>,
    run: impl FnOnce() -> Report,
) -> Report {
    let (started, wall) = origin.unwrap_or_else(|| (host.now(), host.wall_clock()));
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
    /// Set by `watch` for one reclaim cycle ([`WatchCycle`]); `None` for
    /// `ensure`.
    pub watch: Option<WatchCycle>,
}

/// What a `watch` cycle adds to a request (docs/reference.md, The reclaim
/// cycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchCycle {
    /// The space of the filesystem `PATH` resolved to at start
    /// ([`crate::places::space_of`]). The cycle's own resolution of `PATH`
    /// must find it (the one the cycle measures and reclaims on); another
    /// one is an error of the host before anything is deleted (a mount of
    /// another filesystem stacked on `PATH`). Another subvolume of the same
    /// btrfs stacked there is the same space, and is not.
    pub expect_space: crate::places::Space,
    /// The sample that triggered the cycle: the cycle's deadline
    /// (`--timeout`) and its `started_at` and `duration_ms` count from it,
    /// through the lock wait and the reclaim.
    pub origin: MonoTime,
    /// The wall clock at [`Self::origin`], for `started_at`.
    pub origin_wall: Timestamp,
}

impl Request {
    /// When the run started: the triggering sample of a `watch` cycle, now
    /// otherwise.
    fn origin<H: Host>(&self, host: &H) -> MonoTime {
        self.watch.map_or_else(|| host.now(), |w| w.origin)
    }

    /// The run's deadline: `--timeout` from [`Self::origin`].
    fn deadline<H: Host>(&self, host: &H) -> Option<MonoTime> {
        self.timeout.map(|t| {
            self.origin(host)
                .checked_add(t)
                .unwrap_or(MonoTime(Duration::MAX))
        })
    }

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
pub(crate) struct CandidateQueue<T> {
    order: Order,
    oldest: VecDeque<T>,
    sized: BTreeMap<(u64, usize, usize), T>,
    unknown: VecDeque<T>,
}

impl<T> CandidateQueue<T> {
    pub(crate) fn new(
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

    pub(crate) fn is_empty(&self) -> bool {
        self.oldest.is_empty() && self.sized.is_empty() && self.unknown.is_empty()
    }

    pub(crate) fn pop(&mut self, need: u64) -> Option<T> {
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

/// A reclaim lock that could not be taken for a reason other than the
/// deadline or a signal. A lock the installation did not provide (the host
/// lock missing for root, or not the regular file the tmpfiles line makes;
/// an unsafe per-user lock) is exit 2 `provisioning`, so a harness tells
/// "alert ops" from "my call is wrong" without parsing the message
/// (TASK-151, TASK-277). A failure of the host while locking is status
/// `error`, cause `host` (exit 3; in a `watch` cycle back-off `retry`): the
/// environment, which may recover (TASK-277 review, Codex 2; it was exit 2
/// `usage` before).
fn lock_refusal(report: Report, log: &mut dyn FnMut(&str), error: LockError) -> Report {
    let message = error.to_string();
    log(&format!("error: {message}"));
    let cause = match &error {
        LockError::Missing { .. }
        | LockError::Unusable { .. }
        | LockError::UserLockRefused { .. } => ErrorCause::Provisioning,
        LockError::Failed(..) | LockError::UserLockLookup(_) => ErrorCause::Host,
        LockError::DeadlinePassed(_) | LockError::Interrupted(_) => {
            unreachable!("the caller handles the deadline and signals: {error}")
        }
    };
    report.refused(cause, message)
}

/// The path of the lock [`hostlock::acquire`] took, waited for or failed
/// on, for an overrun entry.
fn attempted_lock<F>(acquired: &Result<HeldLock<F>, LockError>) -> PathBuf {
    match acquired {
        Ok(held) => held.lock.path(),
        Err(LockError::DeadlinePassed(lock) | LockError::Failed(lock, _)) => lock.path(),
        Err(LockError::UserLockRefused { path, .. }) => path.clone(),
        Err(_) => hostlock::host_lock_path(),
    }
}

/// The one line that says which lock the reclaim took (docs/safety.md,
/// Host lock): a per-user lock says why and what it does not serialise;
/// none at all is a warning.
fn taken<F>(held: &HeldLock<F>) -> String {
    let waited = held.waited.as_millis();
    let host_lock = hostlock::host_lock_path();
    match &held.lock {
        ReclaimLock::Host => format!("{} taken after {waited} ms", held.lock),
        ReclaimLock::User(_) => format!(
            "{} taken after {waited} ms: the host lock {} is not provisioned, so reclaims by \
             other users are not serialised with this one",
            held.lock,
            host_lock.display()
        ),
        ReclaimLock::Unlocked(hostlock::Unlocked { path, why }) => format!(
            "warning: the host lock {} is not provisioned and the per-user lock{} cannot be \
             made ({why}): this reclaim runs without a lock, serialised with no other \
             (docs/safety.md, Host lock)",
            host_lock.display(),
            path.as_ref()
                .map_or_else(String::new, |p| format!(" {}", p.display()))
        ),
    }
}

/// The deadline `timeout` from now.
fn deadline_of<H: Host>(host: &H, timeout: Option<Duration>) -> Option<MonoTime> {
    timeout.map(|t| host.now().checked_add(t).unwrap_or(MonoTime(Duration::MAX)))
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

/// How a backend's deleting child (a Docker or Podman prune, the Nix GC)
/// ended: whether it started, and the outcome its action records. A
/// refused gate stops the run; a nonzero exit or a signal is an error of
/// the action; the deadline (or a signal to diskgc, which the caller's
/// status reports) stops it.
fn backend_child_end(
    invocation: CommandInvocation,
    candidate: &Path,
    name: &str,
    op: &'static str,
) -> (bool, ReclaimEnd) {
    let attempted = matches!(invocation, CommandInvocation::Started(_));
    let candidate = candidate.to_owned();
    let end = match invocation {
        CommandInvocation::NotStarted(error) if error.cause == Cause::SandboxGate => {
            ReclaimEnd::GateRefused {
                path: candidate,
                error,
            }
        }
        CommandInvocation::NotStarted(error) => ReclaimEnd::Failed {
            path: candidate,
            error,
        },
        CommandInvocation::DeadlineBeforeStart => {
            ReclaimEnd::Stopped(Stop::DeadlinePassed { path: candidate })
        }
        CommandInvocation::Started(Ok(CommandExit::Exited(0))) => ReclaimEnd::Complete,
        CommandInvocation::Started(Ok(CommandExit::Exited(code))) => ReclaimEnd::Failed {
            error: HostError::invalid(
                op,
                candidate.display(),
                format!("{name} client exited {code}"),
            ),
            path: candidate,
        },
        CommandInvocation::Started(Ok(CommandExit::Signaled(signal))) => ReclaimEnd::Failed {
            error: HostError::invalid(
                op,
                candidate.display(),
                format!("{name} client died from signal {signal}"),
            ),
            path: candidate,
        },
        CommandInvocation::Started(Ok(CommandExit::TimedOut)) => {
            ReclaimEnd::Stopped(Stop::DeadlinePassed { path: candidate })
        }
        CommandInvocation::Started(Err(error)) => ReclaimEnd::Failed {
            path: candidate,
            error,
        },
    };
    (attempted, end)
}

/// A `skipped` row of `tier`'s profile with `detail`: `candidate` `None`
/// for the tier as a whole (its profile refused before discovery: a
/// missing executable, an unavailable backend).
fn skip_entry(
    tier: &Tier,
    candidate: Option<&Path>,
    reason: Reason,
    detail: Option<String>,
) -> SkipEntry {
    SkipEntry {
        tier: tier.name.clone(),
        profile: tier.profile.clone(),
        candidate: candidate.map(Path::to_owned),
        count: 1,
        examples: Vec::new(),
        reason,
        detail,
        process_blockers: None,
        unknown_cause: None,
        estimated_bytes: None,
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
        SourceKind::DockerBuildCache
        | SourceKind::DockerImages
        | SourceKind::PodmanImages
        | SourceKind::NixStore => {
            return Err(Stop::Failed {
                path: PathBuf::new(),
                error: HostError::invalid(
                    "discover",
                    &tier.profile,
                    "Docker, Podman and Nix tiers do not use Cargo discovery",
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

/// `other_fs` for a target on another filesystem than `PATH`'s, whose
/// space is `space`: deleting it would free nothing there
/// (docs/architecture.md, Filesystem matching). By space
/// ([`crate::places::space_of`]): a bind mount of the same filesystem, or
/// another subvolume of its btrfs, frees the same space.
fn other_fs(target: &Target, space: crate::places::Space) -> Option<Skip> {
    (!target.space.same_as(&space)).then(|| {
        Skip::new(
            Reason::OtherFs,
            target.path.clone(),
            format!(
                "on device {}, not on PATH's filesystem (device {space})",
                target.space
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
    space: crate::places::Space,
    path_dev: Dev,
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
    // Each held file is placed by the mount it was opened through: every
    // btrfs subvolume of PATH's filesystem is the same space (TASK-286).
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(e) => {
            log(&format!(
                "held_by_deleted_files: the mount table cannot be read, unknown: {e}"
            ));
            return None;
        }
    };
    Some(
        scan.held_by_deleted_files(space.dev(), path_dev, &mounts)
            .into_iter()
            .map(|h| HeldEntry {
                pid: h.pid.0,
                command: Some(h.command.to_string_lossy().into_owned()).filter(|c| !c.is_empty()),
                bytes: h.bytes,
            })
            .collect(),
    )
}

/// Where a candidate meets the `protect` check ([`Reclaim::protected`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtectAt {
    /// As the tier's walk or the index's queue takes it, before its tally.
    FirstScreen,
    /// Immediately before entering its kind's reclaim protocol (under the
    /// lock and after the walk, a delete asks [`DeletePolicy`] instead).
    BeforeReclaim,
}

impl ProtectAt {
    /// A protected skip's detail: `fresh` when judged against matches
    /// enumerated for this check.
    fn detail(self, fresh: bool) -> &'static str {
        match (self, fresh) {
            (ProtectAt::FirstScreen, false) => "overlaps a protected match",
            (ProtectAt::FirstScreen, true) => "overlaps a protected match enumerated again",
            (ProtectAt::BeforeReclaim, false) => "overlaps a protected match found before reclaim",
            (ProtectAt::BeforeReclaim, true) => "overlaps a protected match at deletion recheck",
        }
    }
}

/// What [`Reclaim::protected`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protect {
    /// Not protected: the candidate goes on.
    Clear,
    /// Protected: skipped (reported).
    Skipped,
    /// The run stops: an uncertain check (reported) or the deadline.
    Stopped,
}

/// One reclaim of the tier, under the host lock.
struct Reclaim<'a, 'l, H: Delete> {
    host: &'a H,
    tier: &'a Tier,
    measured: &'a Measured<H::Fd>,
    deadline: Option<MonoTime>,
    tally_budget: Option<Duration>,
    /// What the tier acts through, checked under the host lock before any
    /// tier ran ([`entry::Backend`]).
    backend: Backend,
    /// The tier's `protect` matches ([`DeletePolicy`]), replaced by what a
    /// check enumerates again ([`Reclaim::protected`]).
    protection: Protection,
    report: &'a mut Report,
    log: &'a mut (dyn FnMut(&str) + 'l),
    deadline_hit: bool,
    run_stopped: bool,
    /// Candidates in a row, in the order taken, whose reclaim ended
    /// `in_use_unknown` ([`IN_USE_UNKNOWN_IN_A_ROW_MAX`]).
    in_use_unknown_in_a_row: usize,
    /// The cause and detail of the latest of them when that cause stays
    /// ([`UnknownCause::stays`]): the candidates skipped unchecked then
    /// take it instead of a busy host.
    in_use_unknown_cause: Option<(UnknownCause, String)>,
    /// The run's index, which the reclaim walk takes its queue from
    /// ([`indexed`]); `None` for a tier that discovers only.
    index: Option<&'a mut TallyState>,
    /// What this invocation's synchronous tally left the walk: the
    /// candidates it refused, skipped for the run
    /// ([`indexed::SyncTallied::refused`]), and the first tier's deferred
    /// sizing ([`indexed::SyncTallied::deferred`]).
    tallied: &'a mut indexed::SyncTallied,
    /// When the run started (the first event of `ensure`, on the `Host`
    /// port's monotonic clock): the time to the first deletion is
    /// measured from here (docs/architecture.md, The reclaim walk).
    run_started: MonoTime,
    /// What a second signal does to a running child's grace: `watch`'s
    /// policy or `ensure`'s ([`SecondSignal`]).
    second_signal: SecondSignal,
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
            | Action::DockerImagePrune { .. }
            | Action::PodmanImagePrune { .. }
            | Action::NixStoreGc => false,
        }
    }

    /// The `protect` check of `candidate` at `at`, the one used at every
    /// stage (TASK-269): against the tier's enumerated matches; against
    /// matches enumerated now when an enumerated one cannot be proven any
    /// more (removed or moved since: a `cargo clean` of a protected target
    /// says nothing about this candidate, TASK-200), and before the reclaim
    /// of an action that acts on what it finds when it runs (a command, a
    /// Docker prune). A protected candidate is skipped; an uncertain
    /// enumeration or overlap, or the deadline, stops the run.
    fn protected(&mut self, candidate: &Path, at: ProtectAt) -> Protect {
        let started = self.host.now();
        let verdict = self.protect_verdict(candidate, at);
        self.record_overrun("protect", candidate, started, self.host.now());
        if self.past_deadline() {
            self.stop(&Stop::DeadlinePassed {
                path: candidate.to_owned(),
            });
            return Protect::Stopped;
        }
        match verdict {
            Ok(None) => Protect::Clear,
            Ok(Some(detail)) => {
                self.skip(
                    candidate,
                    &Skip::new(Reason::Protected, candidate.to_owned(), detail.to_owned()),
                );
                Protect::Skipped
            }
            Err(error) => {
                self.run_error(error);
                Protect::Stopped
            }
        }
    }

    /// [`Reclaim::protected`]'s decision: `Some(detail)` when `candidate`
    /// overlaps a match. Matches enumerated again replace the tier's, so
    /// one removed match costs one enumeration, not one per candidate.
    fn protect_verdict(
        &mut self,
        candidate: &Path,
        at: ProtectAt,
    ) -> Result<Option<&'static str>, String> {
        // Judged against the tier's matches again under the lock and after
        // the walk ([`DeletePolicy`]): enumerating them here too would only
        // repeat the tier's enumeration per candidate. A command or Docker
        // prune acts on what it finds when it runs.
        let refresh = if at == ProtectAt::FirstScreen || self.unlinks_only_walked_entries() {
            Refresh::OnError
        } else {
            Refresh::Always
        };
        let (protected, fresh) = judge_protect(
            self.host,
            &self.tier.protections,
            &self.protection,
            candidate,
            self.deadline,
            refresh,
        )?;
        let detail = protected.then(|| at.detail(fresh.is_some()));
        if let Some(fresh) = fresh {
            self.protection.refresh(fresh);
        }
        Ok(detail)
    }

    /// `candidate` `None`: the backend as a whole was skipped.
    fn skip_backend(&mut self, name: &str, candidate: Option<&Path>, reason: Reason, detail: &str) {
        (self.log)(&format!(
            "{}: skip {name} backend: {reason}: {detail}",
            self.tier.name
        ));
        let entry = skip_entry(self.tier, candidate, reason, Some(detail.to_owned()));
        self.report.skipped.push(entry);
    }

    /// The prune through `client` of the backend `bound` under the host
    /// lock before any tier ran, rebound right before the prune.
    fn walk_backend(&mut self, which: Which, client: &Ready, bound: Bound) {
        if self.stop_if_interrupted() {
            return;
        }
        let candidate = bound.candidate(self.tier);
        if self.protected(&candidate, ProtectAt::BeforeReclaim) != Protect::Clear {
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
        // The backend and its storage can change while the host lock is
        // held; compare the full binding (Docker's server ID, Podman's
        // version and store) just before the deleting child is started.
        let started = self.host.now();
        let rebound = prune_backend::inspect(self.host, self.measured.space, which);
        self.record_overrun("preflight", &candidate, started, self.host.now());
        if self.past_deadline() {
            self.deadline_hit = true;
            return;
        }
        match rebound {
            Ok(Binding::Bound(rebound)) if rebound == bound => {}
            Ok(Binding::Bound(_)) => {
                self.skip_backend(
                    which.name(),
                    Some(&candidate),
                    Reason::Changed,
                    which.changed(),
                );
                return;
            }
            Ok(Binding::Skipped(skip)) => {
                self.skip_backend(which.name(), Some(&candidate), skip.reason, &skip.detail);
                return;
            }
            Err(error) => {
                self.tier_error(format!("recheck {} backend: {error}", which.name()));
                return;
            }
        }
        if self.protected(&candidate, ProtectAt::BeforeReclaim) != Protect::Clear {
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
                self.tier_error(format!("open {} command cwd: {error}", which.name()));
                return;
            }
        };
        let prune = bound.prune(self.tier);
        let run = CommandRun {
            executable: client.program.clone(),
            arguments: prune.arguments,
            environment: prune.environment,
            docker_info: prune.docker_info,
            output_prefix: format!("{}/{}", self.tier.name, self.tier.profile),
            deadline: self.deadline.unwrap_or(MonoTime(Duration::MAX)),
            second_signal: self.second_signal,
        };
        let started = self.host.now();
        let invocation = self.host.run_command(&cwd.fd, &run);
        let (attempted, end) =
            backend_child_end(invocation, &candidate, which.name(), which.prune_op());
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
        let skip = match &self.in_use_unknown_cause {
            Some((cause, detail)) => Skip::in_use_unknown(
                candidate.to_owned(),
                *cause,
                format!(
                    "not checked: the {} candidates before it in this tier could not be \
                     checked: {detail}",
                    self.in_use_unknown_in_a_row
                ),
            ),
            None => Skip::in_use_unknown(
                candidate.to_owned(),
                UnknownCause::Busy,
                format!(
                    "not checked: the host was too busy to check the {} candidates before it \
                     in this tier; kept, run again when the host is less busy",
                    self.in_use_unknown_in_a_row
                ),
            ),
        };
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
            unknown_cause: skip.unknown_cause,
            // The tally's size, else the refusing check's own complete walk.
            estimated_bytes: estimated_bytes.or(skip.measured_bytes),
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
        // The summary's blocks follow the order the tiers run in.
        self.report.terms.extend(plan::run_terms(self.tier, &[]));
        // An earlier tier's list is not this run's last word: unknown until
        // this tier's end-of-run scan answers (null if it stops first).
        self.report.held_by_deleted_files = None;
        if self.stop_if_interrupted() {
            return;
        }
        let rules = rules(self.tier, owners);
        let mut checker = Checker::new(&rules, REUSE_LIMIT)
            .with_pid_lookup(self.tier.pid_lookup())
            .with_pytest_facts(self.tier.pytest_facts())
            .with_project_facts(self.tier.project_facts());
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
            SourceKind::DockerBuildCache | SourceKind::DockerImages | SourceKind::PodmanImages => {
                match &self.backend {
                    Backend::Prune {
                        which,
                        client,
                        bound,
                    } => {
                        let (which, client, bound) = (*which, client.clone(), bound.clone());
                        self.walk_backend(which, &client, bound);
                    }
                    Backend::Files | Backend::Command(_) | Backend::Nix(_) => {
                        unreachable!("a backend tier is prepared with its backend")
                    }
                }
            }
            SourceKind::NixStore => match &self.backend {
                Backend::Nix(bound) => {
                    let bound = bound.clone();
                    self.walk_nix(&bound);
                }
                Backend::Files | Backend::Command(_) | Backend::Prune { .. } => {
                    unreachable!("a Nix tier is prepared with its bound daemon")
                }
            },
        }
        if !self.deadline_hit && !self.run_stopped && !self.stop_if_interrupted() {
            let started = self.host.now();
            let held_files = held(
                self.host,
                &mut checker,
                self.measured.space,
                self.measured.at.stat.dev,
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
                unknown_cause: skip.unknown_cause,
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
        let mut negated_matches = match scanned {
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
            match self.protected(&target.path, ProtectAt::FirstScreen) {
                Protect::Clear => {}
                Protect::Skipped => continue,
                Protect::Stopped => return,
            }
            let started = self.host.now();
            let exclusion = judge_negated(
                self.host,
                &self.tier.negated,
                &negated_matches,
                &target.path,
                self.deadline,
                Refresh::OnError,
            );
            self.record_overrun("exclude", &target.path, started, self.host.now());
            if self.past_deadline() {
                return self.stop(&Stop::DeadlinePassed {
                    path: target.path.clone(),
                });
            }
            let excluded = match exclusion {
                Ok((excluded, fresh)) => {
                    if let Some(fresh) = fresh {
                        negated_matches = fresh;
                    }
                    excluded
                }
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
            if let Some(skip) = other_fs(target, self.measured.space) {
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
            match self.protected(&target.path, ProtectAt::BeforeReclaim) {
                Protect::Clear => {}
                Protect::Skipped => continue,
                Protect::Stopped => return,
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
                        protection: &mut self.protection,
                        negated: &[&self.tier.negated],
                        negated_matches: std::slice::from_mut(&mut negated_matches),
                        term: Some(0),
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
        let judge = fs_kind::Judge {
            find,
            rules,
            idle: IdleWindow {
                at_least: self.tier.idle_for,
                before: self.tier.idle_before,
            },
            space: self.measured.space,
        };
        // The index's queue first, then this tier's own discovery for what
        // the queue did not hold (docs/architecture.md, The reclaim walk).
        let mut handled = indexed::Handled::default();
        if self.index.is_some() && indexed::takes_index(self.tier) {
            match self.walk_fs_index(find, &judge, checker, &mut handled) {
                indexed::Walked::TierDone => return,
                indexed::Walked::Discover => {}
            }
        }
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
            if handled.covers(path, candidate) {
                continue;
            }
            match self.protected(path, ProtectAt::FirstScreen) {
                Protect::Clear => {}
                Protect::Skipped => continue,
                Protect::Stopped => return,
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
            match self.act_fs(found, tally.as_ref(), avail, &judge, checker) {
                Acted::Next { avail: after, .. } => avail = after,
                Acted::Stop => return,
            }
        }
    }

    /// One `fs` candidate's reclaim, after discovery (or the index's
    /// re-match) and its tally: the protected re-check, the deadline, the
    /// busy-host give-up, the kind's protocol (every check again under the
    /// candidate's lock), and the record. The available space after it.
    fn act_fs(
        &mut self,
        found: &fs_kind::Found,
        tally: Option<&fs_kind::Tally>,
        avail: u64,
        judge: &fs_kind::Judge<'_>,
        checker: &mut Checker<'_>,
    ) -> Acted {
        let action = &self.tier.action;
        match self.protected(&found.path, ProtectAt::BeforeReclaim) {
            Protect::Clear => {}
            Protect::Skipped => {
                return Acted::Next {
                    avail,
                    reclaimed: false,
                };
            }
            Protect::Stopped => return Acted::Stop,
        }
        if self.stop_if_interrupted() {
            return Acted::Stop;
        }
        if self.past_deadline() {
            self.deadline_hit = true;
            (self.log)(&format!(
                "{}: the deadline passed before {}",
                self.tier.name,
                found.path.display()
            ));
            return Acted::Stop;
        }
        if self.too_busy_to_check(&found.path, &self.tier.profile.clone()) {
            return Acted::Next {
                avail,
                reclaimed: false,
            };
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
                        judge: *judge,
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
                            protection: &mut self.protection,
                            negated: &[],
                            negated_matches: &mut [],
                            term: None,
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
                let ready = self.backend.command();
                let done = fs_kind::run_command_checked(
                    self.host,
                    judge,
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
                        second_signal: self.second_signal,
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
            | Action::DockerImagePrune { .. }
            | Action::PodmanImagePrune { .. }
            | Action::NixStoreGc => unreachable!("fs action admitted by ensure"),
        };
        let reclaimed = matches!(outcome.end, ReclaimEnd::Complete) && !outcome.attempted_command;
        match self.record(&found.path, outcome) {
            Some(after) => Acted::Next {
                avail: after,
                reclaimed,
            },
            None => Acted::Stop,
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
        let mut negated_matches = match scanned {
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
            match self.protected(path, ProtectAt::FirstScreen) {
                Protect::Clear => {}
                Protect::Skipped => continue,
                Protect::Stopped => return,
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
                self.measured.space,
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
                let exclusion = judge_negated(
                    self.host,
                    source.negated,
                    &negated_matches[*index],
                    &found.path,
                    self.deadline,
                    Refresh::OnError,
                );
                self.record_overrun("exclude", &found.path, started, self.host.now());
                if self.past_deadline() {
                    return self.stop(&Stop::DeadlinePassed {
                        path: found.path.clone(),
                    });
                }
                let excluded = match exclusion {
                    Ok((excluded, fresh)) => {
                        if let Some(fresh) = fresh {
                            negated_matches[*index] = fresh;
                        }
                        excluded
                    }
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
                    space: self.measured.space,
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
            match self.protected(path, ProtectAt::BeforeReclaim) {
                Protect::Clear => {}
                Protect::Skipped => continue,
                Protect::Stopped => return,
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
                            space: self.measured.space,
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
            let term_negated: Vec<&[NegatedSource]> =
                sources.iter().map(|source| source.negated).collect();
            let (selected, removed_entries, attempted_command, end) = match &self.tier.action {
                Action::Delete { make_writable } => {
                    let done = fs_kind::reclaim_alternatives_checked(
                        self.host,
                        &alternatives,
                        *make_writable,
                        checker,
                        self.deadline,
                        &mut |selected, path, at| {
                            DeletePolicy {
                                protections: &self.tier.protections,
                                protection: &mut self.protection,
                                negated: &term_negated,
                                negated_matches: &mut negated_matches,
                                term: Some(contributors[selected].0),
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
                            ready: self.backend.command(),
                            argv,
                            cwd: *cwd,
                            need_bytes: self.measured.target.saturating_sub(avail),
                            mount: &self.measured.at.mount.mount_point,
                            sources: command_sources,
                            deadline: self.deadline,
                            second_signal: self.second_signal,
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
            if self.report.actions.is_empty() && outcome.removed_entries > 0 {
                (self.log)(&format!(
                    "first deletion: {} ms after the run started ({})",
                    action_finished
                        .saturating_since(self.run_started)
                        .as_millis(),
                    candidate.display()
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
        let in_use_unknown = match &outcome.end {
            ReclaimEnd::Skipped(skip) if skip.reason == Reason::InUseUnknown => Some((
                skip.unknown_cause.unwrap_or(UnknownCause::Busy),
                skip.detail.clone(),
            )),
            _ => None,
        };
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
        if let Some((cause, detail)) = in_use_unknown {
            self.in_use_unknown_in_a_row += 1;
            self.in_use_unknown_cause = cause.stays().then_some((cause, detail));
            if self.in_use_unknown_in_a_row == IN_USE_UNKNOWN_IN_A_ROW_MAX {
                let why = match &self.in_use_unknown_cause {
                    Some(_) => "a cause that stays, named in each skip",
                    None => "the host is too busy",
                };
                (self.log)(&format!(
                    "{}: {} candidates in a row could not be checked ({why}): the rest of this \
                     tier is skipped unchecked",
                    self.tier.name, self.in_use_unknown_in_a_row
                ));
            }
        } else {
            self.in_use_unknown_in_a_row = 0;
            self.in_use_unknown_cause = None;
        }
        if self.run_stopped { None } else { after }
    }
}

/// What [`Reclaim::act_fs`] came to.
enum Acted {
    /// Go on, with the space available now; `reclaimed`: the candidate's
    /// deletion completed.
    Next { avail: u64, reclaimed: bool },
    /// The tier (or the run) stops.
    Stop,
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
    use super::policy::{overlaps_in_filesystem, overlaps_with, screen_overlap_mounts};
    use super::{CandidateQueue, Dev, Order};
    use crate::host::DevIno;
    use crate::host::MountEntry;
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

    /// A mount table: `/` (dev 8:1), `/tmp` (dev 0:42), `/mnt/x` binding
    /// `/gen/x` of `/tmp`'s filesystem (TASK-260), and `/other` (dev 0:43)
    /// whose filesystem has a directory of the same name `/gen`.
    fn bind_table() -> Vec<MountEntry> {
        let mount = |id, parent_id, point: &str, minor, root: &str| MountEntry {
            id,
            parent_id,
            dev: Dev {
                major: if minor == 1 { 8 } else { 0 },
                minor,
            },
            fs_root: PathBuf::from(root),
            mount_point: PathBuf::from(point),
            fs_type: "tmpfs".to_owned(),
            source: "fixture".to_owned(),
            read_only: false,
            idmapped: false,
            noatime: false,
            upper_dir: None,
        };
        vec![
            mount(1, 0, "/", 1, "/"),
            mount(2, 1, "/tmp", 42, "/"),
            mount(3, 1, "/mnt/x", 42, "/gen/x"),
            mount(4, 1, "/other", 43, "/"),
            // A filesystem mounted deep in a (Cargo target) match, shown
            // again at /mnt/fast, and a directory of it at /proj/other/m.
            mount(5, 1, "/proj/target/deps/fast", 60, "/"),
            mount(6, 1, "/mnt/fast", 60, "/"),
            mount(7, 1, "/proj/other/m", 60, "/elsewhere"),
            // A filesystem mounted on the private bind /mnt/x only (not
            // seen at /tmp/gen/x/sub) and again at /mnt/f; one mounted on
            // that one in turn, and again at /mnt/g.
            mount(8, 3, "/mnt/x/sub", 70, "/"),
            mount(9, 1, "/mnt/f", 70, "/"),
            mount(10, 8, "/mnt/x/sub/deeper", 71, "/"),
            mount(11, 1, "/mnt/g", 71, "/"),
            // A bind of a prefix sibling, and a root field with a space
            // (`\040` in mountinfo, unescaped by the parser).
            mount(12, 1, "/mnt/xy", 42, "/gen/xy"),
            mount(13, 1, "/mnt/sp", 42, "/my proj/x"),
        ]
    }

    fn overlaps_by_place(mounts: &[MountEntry], candidate: &str, matched: &str) -> bool {
        overlaps_in_filesystem(
            mounts,
            Path::new(candidate),
            &[PathBuf::from(matched)],
            || Ok(()),
        )
        .unwrap()
    }

    #[test]
    fn overlap_maps_a_subdirectory_bind_back_to_its_filesystem() {
        let mounts = bind_table();
        let overlaps =
            |candidate: &str, matched: &str| overlaps_by_place(&mounts, candidate, matched);
        // Under the bind, against an ancestor of its source, both ways.
        assert!(overlaps("/mnt/x/y", "/tmp/gen"));
        assert!(overlaps("/tmp/gen", "/mnt/x/y"));
        // The bind's root itself is `/tmp/gen/x`.
        assert!(overlaps("/mnt/x", "/tmp/gen/x/y"));
        // A sibling of the source, and one of its ancestors' siblings.
        assert!(!overlaps("/mnt/x/y", "/tmp/gen/w"));
        assert!(!overlaps("/mnt/x/y", "/tmp/other"));
        // The same path from the root of another filesystem.
        assert!(!overlaps("/other/gen/x/y", "/tmp/gen"));
        // A filesystem mounted inside a match, reached through another
        // mount of it; only the part a mount inside the match shows.
        assert!(overlaps("/mnt/fast/y", "/proj/target"));
        assert!(!overlaps("/mnt/fast/y", "/proj/other"));
        assert!(overlaps("/mnt/fast/elsewhere/z", "/proj/other"));
        assert!(overlaps("/mnt/fast", "/proj/other"));
        // Another filesystem whose root path overlaps the candidate's place.
        assert!(!overlaps("/other/q", "/proj/target"));
        // Mounted on a place inside the match (through the private bind),
        // one level and two levels down the parent chain (mped B1).
        assert!(overlaps("/mnt/f/w", "/tmp/gen"));
        assert!(overlaps("/mnt/f/w", "/tmp/gen/x/sub"));
        assert!(!overlaps("/mnt/f/w", "/tmp/gen/w"));
        assert!(overlaps("/mnt/g/q", "/tmp/gen"));
        assert!(!overlaps("/mnt/g/q", "/tmp/other"));
        // A prefix sibling is not inside; a root field with a space.
        assert!(!overlaps("/mnt/xy/q", "/tmp/gen/x"));
        assert!(overlaps("/mnt/xy/q", "/tmp/gen"));
        assert!(overlaps("/mnt/sp/q", "/tmp/my proj"));
        assert!(!overlaps("/mnt/sp/q", "/tmp/my"));
    }

    #[test]
    fn overlap_by_filesystem_place_fails_closed_on_an_unclear_table() {
        let mut stacked = bind_table();
        // Two mounts on one point, neither on the other: no topmost.
        let mut twin = stacked[2].clone();
        twin.id = 50;
        stacked.push(twin);
        let error = overlaps_in_filesystem(
            &stacked,
            Path::new("/mnt/x/y"),
            &[PathBuf::from("/tmp/other")],
            || Ok(()),
        )
        .unwrap_err();
        assert!(
            error.contains("cannot prove overlap at /mnt/x/y"),
            "{error}"
        );

        // A relative root field, met on the match's side, on the
        // candidate's, or on another mount of the candidate's filesystem.
        let mut relative = bind_table();
        relative[2].fs_root = PathBuf::from("gen/x");
        for (candidate, matched) in [
            ("/other/q", "/mnt/x/y"),
            ("/mnt/x/y", "/other/q"),
            ("/tmp/q", "/other/q"),
        ] {
            let error = overlaps_in_filesystem(
                &relative,
                Path::new(candidate),
                &[PathBuf::from(matched)],
                || Ok(()),
            )
            .unwrap_err();
            assert!(error.contains("not an absolute path"), "{error}");
        }

        // A mount whose point is not below its parent's, and a parent
        // chain that loops, cannot be attached anywhere.
        let mut misplaced = bind_table();
        misplaced[7].parent_id = 4;
        // Two mounts on one point, each the other's parent, one of them
        // a mount of the candidate's filesystem.
        let mut looped = bind_table();
        let mut first = looped[10].clone();
        (first.id, first.parent_id, first.mount_point) = (20, 21, PathBuf::from("/mnt/loop"));
        let mut second = looped[0].clone();
        (second.id, second.parent_id, second.mount_point) = (21, 20, PathBuf::from("/mnt/loop"));
        looped.extend([first, second]);
        for (table, candidate, wanted) in [
            (
                misplaced,
                "/mnt/f/w",
                "cannot be placed on its parent /other",
            ),
            (looped, "/mnt/g/q", "does not end"),
        ] {
            let error = overlaps_in_filesystem(
                &table,
                Path::new(candidate),
                &[PathBuf::from("/home")],
                || Ok(()),
            )
            .unwrap_err();
            assert!(error.contains(wanted), "{error}");
        }

        // The deadline is polled per match.
        let error = overlaps_in_filesystem(
            &bind_table(),
            Path::new("/mnt/x/y"),
            &[PathBuf::from("/tmp/other")],
            || Err("timed out".to_owned()),
        )
        .unwrap_err();
        assert_eq!(error, "timed out");
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
            idmapped: false,
            noatime: false,
            upper_dir: None,
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

#[cfg(test)]
mod lock_refusal_tests {
    use super::{ErrorCause, HostError, LockError, Report, Status, lock_refusal};

    /// mped S5 (TASK-32 review), TASK-277 review (Codex 2): a host failure
    /// while locking (EIO) is the environment, in `ensure` and in a `watch`
    /// cycle alike: status `error` (exit 3), cause `host`, so a cycle backs
    /// off `retry`. A missing lock file is `provisioning`, exit 2.
    #[test]
    fn a_failing_host_lock_is_a_host_error() {
        let failed = || {
            LockError::Failed(
                crate::hostlock::ReclaimLock::Host,
                HostError::errno("flock", "/run/diskgc/lock", crate::host::Errno::EIO),
            )
        };
        let report = || Report::new(Status::Error, None, false);
        let failure = lock_refusal(report(), &mut |_| {}, failed());
        assert_eq!(failure.status, Status::Error);
        assert_eq!(failure.exit_code(), 3);
        assert_eq!(failure.errors[0].cause, ErrorCause::Host);
        let missing = || LockError::Missing {
            detail: "ENOENT".into(),
        };
        let gone = lock_refusal(report(), &mut |_| {}, missing());
        assert_eq!(gone.status, Status::UsageError);
        assert_eq!(gone.errors[0].cause, ErrorCause::Provisioning);
    }
}

/// TASK-36 moved the Docker and Podman prune's end mapping out of
/// `walk_backend` into [`backend_child_end`], shared with the Nix GC: each
/// way a child can end, for each backend, as the prune always recorded it.
#[cfg(test)]
mod backend_child_end_tests {
    use super::{
        Cause, CommandExit, CommandInvocation, HostError, ReclaimEnd, Stop, Which,
        backend_child_end,
    };
    use crate::docker_backend::Storage;
    use std::path::Path;

    #[test]
    fn every_child_end_maps_as_the_prune_recorded_it() {
        let candidate = Path::new("/store");
        for (name, op) in [
            Which::Docker(Storage::BuildCache),
            Which::Docker(Storage::Images),
            Which::Podman,
        ]
        .map(|which| (which.name(), which.prune_op()))
        .into_iter()
        .chain([("Nix", "Nix GC")])
        {
            let gate = HostError::sandbox_gate("run command", "x", "closed");
            assert_eq!(gate.cause, Cause::SandboxGate);
            let (started, end) = backend_child_end(
                CommandInvocation::NotStarted(gate.clone()),
                candidate,
                name,
                op,
            );
            assert!(!started);
            assert!(matches!(end, ReclaimEnd::GateRefused { error, .. } if error == gate));
            let other = HostError::invalid("run command", "x", "no");
            let (started, end) = backend_child_end(
                CommandInvocation::NotStarted(other.clone()),
                candidate,
                name,
                op,
            );
            assert!(!started);
            assert!(matches!(end, ReclaimEnd::Failed { error, .. } if error == other));
            let (started, end) =
                backend_child_end(CommandInvocation::DeadlineBeforeStart, candidate, name, op);
            assert!(!started);
            assert!(matches!(
                end,
                ReclaimEnd::Stopped(Stop::DeadlinePassed { .. })
            ));
            let started_with =
                |exit| backend_child_end(CommandInvocation::Started(Ok(exit)), candidate, name, op);
            let (started, end) = started_with(CommandExit::Exited(0));
            assert!(started);
            assert!(matches!(end, ReclaimEnd::Complete));
            let (_, end) = started_with(CommandExit::Exited(2));
            let ReclaimEnd::Failed { error, .. } = end else {
                panic!("exit 2 is a failure")
            };
            assert_eq!(error.op, op);
            assert!(
                error
                    .to_string()
                    .contains(&format!("{name} client exited 2"))
            );
            let (_, end) = started_with(CommandExit::Signaled(9));
            let ReclaimEnd::Failed { error, .. } = end else {
                panic!("a signal is a failure")
            };
            assert!(
                error
                    .to_string()
                    .contains(&format!("{name} client died from signal 9"))
            );
            let (started, end) = started_with(CommandExit::TimedOut);
            assert!(started);
            assert!(matches!(
                end,
                ReclaimEnd::Stopped(Stop::DeadlinePassed { .. })
            ));
            let failed = HostError::invalid("forward command output", "x", "broken");
            let (started, end) = backend_child_end(
                CommandInvocation::Started(Err(failed.clone())),
                candidate,
                name,
                op,
            );
            assert!(started);
            assert!(matches!(end, ReclaimEnd::Failed { error, .. } if error == failed));
        }
    }
}
