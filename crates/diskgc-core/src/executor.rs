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

mod indexed;
mod plan;
mod policy;
mod preflight;
mod tier;
mod tier_policy;

pub use plan::{dry_run, dry_run_grouped};
pub use preflight::privileges;
pub use tier::{
    FsUnionSource, NegatedKind, NegatedSource, ProtectedKind, ProtectedSource, SELECT_TIER,
    SelectionFailure, SourceKind, Tier, bind_protections,
};
// Shared between the entry points here and the sibling modules, which
// import them by name from here (no glob: the source scan's sandbox-gate
// guard refuses glob imports).
use plan::{
    FsUnionCandidate, discover_fs_union, docker_candidate, docker_storage, scan_negated,
    scan_union_negated, union_checkout_refusal, union_checkouts, union_guard_veto,
};
use policy::{
    excluded_by, overlaps_checked, overlaps_checked_until, policy_skip, policy_verdict,
    scan_protections,
};
use preflight::{
    Measured, deadline_before_first_sample, measure_failure, measure_until, nothing_searched,
    preflight_roots, roots_text, warn_if_host_lock_unready_timed,
};
use tier::FsUnionSourceRef;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cargo_target::{self, Discovery, ReclaimEnd, Stop, Tally, Target};
use crate::command_action::{self, Preflight, Ready};
use crate::config::Order;
use crate::docker_backend::{self, Binding, Bound};
use crate::engine::{Checkouts, Reason, Rules, Skip};
use crate::fs_kind::{self, Find};
use crate::host::{
    Cause, CommandExit, CommandInvocation, CommandRun, Delete, Dev, Host, HostError, MonoTime,
    MountEntry, Space, Timestamp,
};
use crate::hostlock::{self, LockError};
use crate::index::{BUDGET_BYTES, Index};
use crate::inuse::{Checker, REUSE_LIMIT};
use crate::profile::{Action, IdleWindow};
use crate::report::{
    ActionEntry, ErrorCause, ErrorEntry, Final, HeldEntry, OverrunEntry, Report, SkipEntry, Status,
    after_lock_timeout, conclude, human_space_values, signed_bytes,
};
use crate::resolve::{Want, resolve};
use crate::tally::TallyState;
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

/// The deadline `timeout` from now.
fn deadline_of<H: Host>(host: &H, timeout: Option<Duration>) -> Option<MonoTime> {
    timeout.map(|t| host.now().checked_add(t).unwrap_or(MonoTime(Duration::MAX)))
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
    let mut index = TallyState::new(Index::new(BUDGET_BYTES));
    ensure_with_index(host, request, owners, &mut index, log)
}

/// [`ensure`] on `index`: the run's synchronous tally adds to it, the
/// reclaim walk takes its queues from it and updates it (docs/architecture.md,
/// The reclaim walk). Whatever `index` holds (a seed, or units injected by
/// a test, stale, partial or wrong) only orders work: every candidate is
/// found again and checked live.
pub fn ensure_with_index<H: Delete>(
    host: &H,
    request: &Request,
    owners: &[u32],
    index: &mut TallyState,
    log: &mut dyn FnMut(&str),
) -> Report {
    timed_report(host, || ensure_inner(host, request, owners, index, log))
}

fn ensure_inner<H: Delete>(
    host: &H,
    request: &Request,
    owners: &[u32],
    index: &mut TallyState,
    log: &mut dyn FnMut(&str),
) -> Report {
    let run_started = host.now();
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
    // Read-only, before the host lock (docs/architecture.md, The tally).
    indexed::sync_tally(
        host,
        index,
        [&request.tier],
        measured.dev,
        request.tally_budget,
        deadline,
        &mut report,
        log,
    );
    if host.interrupted().is_some() {
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
        index: Some(index),
        run_started,
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
    let mut index = TallyState::new(Index::new(BUDGET_BYTES));
    ensure_grouped_with_index(host, request, tiers, tier_timeouts, owners, &mut index, log)
}

/// [`ensure_grouped`] on `index` (see [`ensure_with_index`]).
pub fn ensure_grouped_with_index<H: Delete>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    index: &mut TallyState,
    log: &mut dyn FnMut(&str),
) -> Report {
    timed_report(host, || {
        ensure_grouped_inner(host, request, tiers, tier_timeouts, owners, index, log)
    })
}

fn ensure_grouped_inner<H: Delete>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    index: &mut TallyState,
    log: &mut dyn FnMut(&str),
) -> Report {
    let run_started = host.now();
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
    // Read-only, before the host lock (docs/architecture.md, The tally).
    indexed::sync_tally(
        host,
        index,
        tiers,
        measured.dev,
        request.tally_budget,
        deadline,
        &mut report,
        log,
    );
    if host.interrupted().is_some() {
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
            index: Some(&mut *index),
            run_started,
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
    /// The run's index, which the reclaim walk takes its queue from
    /// ([`indexed`]); `None` for a tier that discovers only.
    index: Option<&'a mut TallyState>,
    /// When the run started (the first event of `ensure`, on the `Host`
    /// port's monotonic clock): the time to the first deletion is
    /// measured from here (docs/architecture.md, The reclaim walk).
    run_started: MonoTime,
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
        let judge = fs_kind::Judge {
            find,
            rules,
            idle: IdleWindow {
                at_least: self.tier.idle_for,
                before: self.tier.idle_before,
            },
            dev: self.measured.dev,
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
        if self.protected_before_reclaim(&found.path) {
            if self.run_stopped {
                return Acted::Stop;
            }
            return Acted::Next {
                avail,
                reclaimed: false,
            };
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
    use super::policy::{overlaps_with, screen_overlap_mounts};
    use super::{CandidateQueue, Dev, MountEntry, Order};
    use crate::host::DevIno;
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
