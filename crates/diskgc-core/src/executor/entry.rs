//! The one reclaiming entry point (docs/architecture.md, The reclaim walk).
//!
//! `--select` is a group of one: its tier without a timeout of its own
//! ([`ensure_with_index`]); a configuration runs its tiers in policy order
//! ([`ensure_grouped_with_index`]). Both go through [`run`]:
//!
//! 1. Every tier's action is one this build reclaims ([`reclaimable`]),
//!    then the pre-release sandbox gate, then the first sample. At or above
//!    `--min-free` the run ends there, without the host lock, the tally or
//!    the snapshot.
//! 2. The synchronous tally, before the host lock.
//! 3. Under the host lock ([`Locked::walk`]): a fresh sample (another
//!    reclaim may have freed the space), `protect` enumerated, then every
//!    tier prepared ([`Locked::prepare_tier`]: roots on `PATH`'s filesystem,
//!    command executables resolved, Docker backend bound) before any tier
//!    may delete, then each prepared tier reclaimed in order.
//! 4. One exit after the tally ([`Ending`]): the snapshot
//!    ([`RunIndex::finish`]), then the final sample that decides the status,
//!    then the host lock released. `finish` itself declines a write after a
//!    signal or at the deadline, with one log line, so every run that
//!    tallied either leaves a snapshot or says why not (PRD: each `ensure`
//!    that tallies leaves a snapshot).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::command_action::{self, Preflight, Ready};
use crate::docker_backend::{self, Binding, Bound};
use crate::engine::Reason;
use crate::host::{Delete, MonoTime};
use crate::hostlock::{self, LockError};
use crate::profile::Action;
use crate::report::{
    Final, Report, SkipEntry, Status, after_lock_timeout, conclude, human_space_values,
};
use crate::snapshot::{Reclaimed, RunIndex};
use crate::tally::TallyState;
use crate::units::format_bytes;

use super::preflight::{
    Measured, deadline_before_first_sample, measure_failure, measure_until, preflight_roots,
    warn_if_host_lock_unready_timed,
};
use super::tier_policy::Protection;
use super::{
    NotesOncePerRun, Reclaim, Request, SourceKind, Tier, attempted_lock, deadline_before_reclaim,
    deadline_of, deadline_passed, docker_candidate, docker_storage, final_sample, host_error,
    indexed, lock_refusal, record_deadline_overrun, record_error, run_error, sample, taken,
    timed_report_from, usage,
};

/// Brings `PATH`'s filesystem to the goal with the request's tier (see the
/// module docs). Progress goes to `log`, one line each.
///
/// Trusted-caller precondition: call [`super::privileges`] first. With
/// root, load policy through a protected config path, resolve `owners` to
/// non-system UIDs, and reject actions with [`Action::root_mode_error`]
/// before calling. The CLI enforces these conditions; this library entry
/// point accepts an already-validated request and ownership allowlist.
pub fn ensure<H: Delete>(
    host: &H,
    request: &Request,
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    ensure_with_index(host, request, owners, &mut RunIndex::ephemeral(), log)
}

/// [`ensure`] on `index`: a run that tallies seeds it from the snapshot
/// when `index` says so ([`RunIndex::snapshot`]), its synchronous tally
/// adds to it, the reclaim walk takes its queues from it and updates it
/// (docs/architecture.md, The reclaim walk), and after the tally it is
/// written as the snapshot ([`RunIndex::finish`]). Whatever it holds (a
/// seed, or units injected by a test, stale, partial or wrong) only
/// orders work: every candidate is found again and checked live.
///
/// The request's tier is a group of one without a timeout of its own: the
/// same run as [`ensure_grouped_with_index`].
pub fn ensure_with_index<H: Delete>(
    host: &H,
    request: &Request,
    owners: &[u32],
    index: &mut RunIndex,
    log: &mut dyn FnMut(&str),
) -> Report {
    let tiers = std::slice::from_ref(&request.tier);
    timed(host, request, tiers, &[Duration::ZERO], owners, index, log)
}

/// Reclaim configured tiers in policy order under one host lock. All command
/// and Docker environment checks finish before the first deletion. A tier
/// whose environment fails is excluded and reported; the remaining tiers
/// still run. The caller supplies the tiers in group/ranking order, and
/// each tier's own timeout (zero: none).
pub fn ensure_grouped<H: Delete>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    log: &mut dyn FnMut(&str),
) -> Report {
    let mut index = RunIndex::ephemeral();
    ensure_grouped_with_index(host, request, tiers, tier_timeouts, owners, &mut index, log)
}

/// [`ensure_grouped`] on `index` (see [`ensure_with_index`]).
pub fn ensure_grouped_with_index<H: Delete>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    index: &mut RunIndex,
    log: &mut dyn FnMut(&str),
) -> Report {
    timed(host, request, tiers, tier_timeouts, owners, index, log)
}

/// [`run`], timed from the run's origin (a `watch` cycle's triggering
/// sample, or now).
fn timed<H: Delete>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    index: &mut RunIndex,
    log: &mut dyn FnMut(&str),
) -> Report {
    let origin = request.watch.map(|w| (w.origin, w.origin_wall));
    timed_report_from(host, origin, || {
        run(host, request, tiers, tier_timeouts, owners, index, log)
    })
}

/// Only a source whose removal this build implements is reclaimed. An
/// allowlist: kind cargo-target through its lock protocol, kind fs through
/// its own deletion or checked command protocol, the Docker kinds through
/// their prune of the bound backend. Anything else is refused before the
/// gate, the first sample or the host lock, never approximated by a
/// different kind's protocol.
fn reclaimable(tier: &Tier) -> Result<(), String> {
    match (&tier.source, &tier.action) {
        (SourceKind::CargoTarget, Action::CargoLockProtocol)
        | (SourceKind::Fs(_), Action::Delete { .. } | Action::Command { .. })
        | (SourceKind::DockerBuildCache, Action::DockerBuilderPrune)
        | (SourceKind::DockerImages, Action::DockerImagePrune { .. }) => Ok(()),
        (_, action) => Err(format!(
            "tier `{}` (`{}`): profile `{}` removes with `{action}`, which this build does not \
             run: a reclaiming ensure refuses it; use --dry-run for a read-only plan",
            tier.name, tier.select, tier.profile
        )),
    }
}

fn run<H: Delete>(
    host: &H,
    request: &Request,
    tiers: &[Tier],
    tier_timeouts: &[Duration],
    owners: &[u32],
    index: &mut RunIndex,
    log: &mut dyn FnMut(&str),
) -> Report {
    let run_started = request.origin(host);
    let mut notes = NotesOncePerRun::new(log);
    let log: &mut dyn FnMut(&str) = &mut |line| notes.line(line);
    let deadline = request.deadline(host);
    // Not a success until a path below says so.
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
    // Before anything else (the gate, the first sample, the host lock).
    if let Some(refused) = tiers.iter().find_map(|tier| reclaimable(tier).err()) {
        return usage(report, log, refused);
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
    let [avail_text, min_text, _] = human_space_values([Some(avail), Some(measured.min), None]);
    log(&format!(
        "{} available on {}, below --min-free {}: taking the reclaim lock",
        avail_text,
        measured.at.mount.mount_point.display(),
        min_text
    ));
    // Read-only, before the host lock (docs/architecture.md, The tally).
    let tallied = indexed::sync_tally(
        host,
        index,
        tiers,
        Reclaimed {
            dev: measured.dev,
            fs: measured.at.mount.dev,
            min_free: measured.min,
            deadline,
        },
        request.tally_budget,
        &mut report,
        log,
    );
    // From here on, every return goes through the one exit below.
    let locked = Locked {
        host,
        request,
        tiers,
        tier_timeouts,
        owners,
        measured: &measured,
        deadline,
        run_started,
    };
    let mut host_lock = None;
    let ending = 'locked: {
        if host.interrupted().is_some() {
            break 'locked Ending::Decided;
        }
        let started = host.now();
        let acquired = hostlock::acquire(host, deadline);
        record_deadline_overrun(
            host,
            &mut report,
            deadline,
            started,
            "host_lock",
            &attempted_lock(&acquired),
        );
        match acquired {
            Ok(lock) => {
                log(&taken(&lock));
                host_lock = Some(lock);
            }
            Err(error @ LockError::DeadlinePassed(_)) => {
                log(&error.to_string());
                break 'locked Ending::LockTimeout;
            }
            Err(LockError::Interrupted(signal)) => {
                report.status = Status::Interrupted(signal);
                break 'locked Ending::Decided;
            }
            Err(error) => {
                if let Some(signal) = host.interrupted() {
                    report.status = Status::Interrupted(signal);
                } else {
                    report = lock_refusal(report, log, error);
                }
                break 'locked Ending::Decided;
            }
        }
        let index = tallied.usable.then_some(&mut index.state);
        locked.walk(index, &tallied.refused, &mut report, log)
    };
    // The snapshot, before the final sample: the sample and the exit code
    // see the space it takes (docs/architecture.md, Snapshot).
    index.finish(host, log);
    ending.conclude(&locked, &mut report, log);
    // Released only here, by closing, after the final sample.
    drop(host_lock);
    report
}

/// How a run that tallied ends, decided under (or waiting for) the host
/// lock and enacted by its one exit after the snapshot ([`run`]).
enum Ending {
    /// The status is decided without a final sample: a signal, the host
    /// lock refused, or the sample under the lock failed.
    Decided,
    /// The deadline passed waiting for the host lock: a fresh sample
    /// decides 0, 75 or 3.
    LockTimeout,
    /// An error of the run before any tier ran (`protect` could not be
    /// enumerated): sampled for the report, the status stays `error`.
    Failed,
    /// The final sample concludes: after the walk, or the deadline
    /// stopped it (`deadline_hit`), or the space was there under the lock
    /// (the sample after the snapshot's write still decides).
    Concluded { deadline_hit: bool },
}

impl Ending {
    fn conclude<H: Delete>(
        self,
        run: &Locked<'_, H>,
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) {
        let (host, measured, deadline) = (run.host, run.measured, run.deadline);
        let first = &run.tiers[0];
        match self {
            Self::Decided => {}
            Self::LockTimeout => {
                // A fresh sample decides: the other holder may have freed enough.
                report.avail_after_bytes =
                    final_sample(host, measured, deadline, report, log, first);
                report.status =
                    after_lock_timeout(Final::of(report.avail_after_bytes, measured.min));
            }
            Self::Failed => {
                report.avail_after_bytes =
                    final_sample(host, measured, deadline, report, log, first);
            }
            Self::Concluded { deadline_hit } => {
                report.avail_after_bytes =
                    final_sample(host, measured, deadline, report, log, first);
                report.status = conclude(
                    Final::of(report.avail_after_bytes, measured.min),
                    deadline_hit || deadline_passed(host, deadline),
                    !report.errors.is_empty(),
                );
            }
        }
    }
}

/// A tier ready to reclaim under the host lock: its roots on `PATH`'s
/// filesystem only, its command executables resolved, its Docker backend
/// bound ([`Locked::prepare_tier`]).
struct PreparedTier {
    tier: Tier,
    command_ready: Option<Ready>,
    docker_bound: Option<Bound>,
}

/// The run's deadline passed while a tier was prepared.
struct PastDeadline;

/// What the walk under the host lock needs from the run so far.
struct Locked<'r, H: Delete> {
    host: &'r H,
    request: &'r Request,
    tiers: &'r [Tier],
    tier_timeouts: &'r [Duration],
    owners: &'r [u32],
    measured: &'r Measured<H::Fd>,
    deadline: Option<MonoTime>,
    run_started: MonoTime,
}

impl<H: Delete> Locked<'_, H> {
    /// Everything under the host lock up to the end of the last tier.
    fn walk(
        &self,
        mut index: Option<&mut TallyState>,
        tally_refused: &BTreeMap<PathBuf, Reason>,
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) -> Ending {
        let (host, measured, deadline) = (self.host, self.measured, self.deadline);
        let first = &self.tiers[0];
        let past_deadline = Ending::Concluded { deadline_hit: true };
        if deadline_passed(host, deadline) {
            return past_deadline;
        }
        // Someone else (the reclaim we waited for) may have freed it meanwhile.
        let started = host.now();
        let sampled = sample(host, measured);
        record_deadline_overrun(host, report, deadline, started, "statvfs", &measured.path);
        match sampled {
            Ok(space) if space.avail_bytes >= measured.min => {
                log(&format!(
                    "{} available now: --min-free is met, nothing to reclaim",
                    format_bytes(space.avail_bytes)
                ));
                return Ending::Concluded {
                    deadline_hit: false,
                };
            }
            Ok(_) => {}
            Err(error) => {
                record_error(report, log, host_error(first, error));
                return Ending::Decided;
            }
        }
        if deadline_passed(host, deadline) {
            return past_deadline;
        }
        let started = host.now();
        let protections = Protection::enumerate(host, &first.protections, deadline);
        record_deadline_overrun(
            host,
            report,
            deadline,
            started,
            "protect",
            &self.request.path,
        );
        if deadline_passed(host, deadline) {
            return past_deadline;
        }
        // Enumerated before any tier may delete (a protect that cannot be
        // enumerated stops the run first), then again before each later tier:
        // a tier's delete actions judge against its own (TASK-200).
        let mut protection = match protections {
            Ok(protection) => protection,
            Err(error) => {
                record_error(report, log, run_error(first, error));
                return Ending::Failed;
            }
        };
        // Environmental checks of every tier finish under the host lock
        // before any tier may delete. The action rechecks volatile binding.
        let mut prepared = Vec::with_capacity(self.tiers.len());
        for tier in self.tiers {
            if deadline_passed(host, deadline) {
                return past_deadline;
            }
            match self.prepare_tier(tier, report, log) {
                Ok(tier) => prepared.push(tier),
                Err(PastDeadline) => return past_deadline,
            }
        }
        let mut deadline_hit = false;
        let mut a_tier_ran = false;
        for (prepared, tier_timeout) in prepared.into_iter().zip(self.tier_timeouts) {
            if host.interrupted().is_some() {
                break;
            }
            let Some(PreparedTier {
                tier,
                command_ready,
                docker_bound,
            }) = prepared
            else {
                continue;
            };
            // Between tiers: the goal may be there. The first tier to run
            // starts from the sample taken under the lock above.
            if a_tier_ran {
                let started = host.now();
                let sampled = sample(host, measured);
                record_deadline_overrun(host, report, deadline, started, "statvfs", &measured.path);
                match sampled {
                    Ok(space) if space.avail_bytes >= measured.target => break,
                    Ok(_) => {}
                    Err(error) => {
                        record_error(report, log, host_error(&tier, error));
                        break;
                    }
                }
            }
            if deadline_passed(host, deadline) {
                deadline_hit = true;
                break;
            }
            let own_deadline = (!tier_timeout.is_zero())
                .then(|| deadline_of(host, Some(*tier_timeout)).expect("a timeout"));
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
                    report,
                    deadline,
                    started,
                    "protect",
                    &self.request.path,
                );
                if deadline_passed(host, deadline) {
                    deadline_hit = true;
                    break;
                }
                match enumerated {
                    Ok(enumerated) => protection = enumerated,
                    Err(error) => {
                        record_error(report, log, run_error(&tier, error));
                        break;
                    }
                }
            }
            a_tier_ran = true;
            let mut reclaim = Reclaim {
                host,
                tier: &tier,
                measured,
                deadline: tier_deadline,
                tally_budget: self.request.tally_budget,
                command_ready,
                docker_bound,
                protection: &protection,
                report: &mut *report,
                log: &mut *log,
                deadline_hit: false,
                run_stopped: false,
                in_use_unknown_in_a_row: 0,
                index: index.as_deref_mut(),
                tally_refused,
                run_started: self.run_started,
            };
            reclaim.run(self.owners);
            let (timed_out, stopped) = (reclaim.deadline_hit, reclaim.run_stopped);
            if timed_out {
                // A tier without a timeout of its own (every `--select`
                // run) can only have met the run's deadline.
                if own_deadline.is_none() || deadline_passed(host, deadline) {
                    deadline_hit = true;
                    break;
                }
                record_error(
                    report,
                    log,
                    run_error(&tier, "tier timeout passed; the tier is excluded"),
                );
                continue;
            }
            if stopped {
                break;
            }
        }
        Ending::Concluded { deadline_hit }
    }

    /// `tier`'s run-time checks under the host lock, before any tier may
    /// delete: its roots ([`preflight_roots`]: one on another filesystem
    /// is skipped, an unsafe one excludes the tier), its command
    /// executables (a tool not installed is a `missing` skip of the tier,
    /// docs/safety.md, Command actions) and its Docker backend
    /// (docs/reference.md, Backend skips and errors). `Ok(None)`: the tier
    /// is excluded, reported, and the other tiers still run.
    fn prepare_tier(
        &self,
        tier: &Tier,
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) -> Result<Option<PreparedTier>, PastDeadline> {
        let (host, deadline, path) = (self.host, self.deadline, &self.request.path);
        let started = host.now();
        let roots = preflight_roots(host, tier, self.measured.dev);
        record_deadline_overrun(host, report, deadline, started, "preflight", path);
        if deadline_passed(host, deadline) {
            return Err(PastDeadline);
        }
        report.skipped.extend(roots.skipped.iter().cloned());
        if !roots.errors.is_empty() {
            for error in roots.errors.iter().cloned() {
                record_error(report, log, error);
            }
            return Ok(None);
        }
        let tier = roots.filtered_tier(tier);
        let docker_argv = ["docker".to_owned()];
        let argv = match &tier.action {
            Action::Command { argv, .. } => Some(argv.as_slice()),
            Action::DockerBuilderPrune | Action::DockerImagePrune { .. } => {
                Some(docker_argv.as_slice())
            }
            Action::Delete { .. } | Action::CargoLockProtocol => None,
        };
        let command_ready = match argv {
            None => None,
            Some(argv) => {
                let started = host.now();
                let preflight = command_action::preflight(host, argv);
                record_deadline_overrun(host, report, deadline, started, "preflight", path);
                if deadline_passed(host, deadline) {
                    return Err(PastDeadline);
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
                        report.skipped.push(tier_skip(&tier, Reason::Missing, None));
                        return Ok(None);
                    }
                    Err(error) => {
                        record_error(report, log, run_error(&tier, error.to_string()));
                        return Ok(None);
                    }
                }
            }
        };
        let docker_bound = match tier.source {
            SourceKind::DockerBuildCache | SourceKind::DockerImages => {
                let started = host.now();
                let inspected =
                    docker_backend::inspect_backend(host, self.measured.dev, docker_storage(&tier));
                record_deadline_overrun(host, report, deadline, started, "preflight", path);
                if deadline_passed(host, deadline) {
                    return Err(PastDeadline);
                }
                match inspected {
                    Ok(Binding::Bound(bound)) => {
                        log(&format!(
                            "{}: Docker backend bound to {}",
                            tier.name,
                            docker_candidate(&tier, &bound).display()
                        ));
                        Some(bound)
                    }
                    Ok(Binding::Skipped(skip)) => {
                        log(&format!(
                            "{}: skip Docker backend: {}: {}",
                            tier.name, skip.reason, skip.detail
                        ));
                        report
                            .skipped
                            .push(tier_skip(&tier, skip.reason, Some(skip.detail)));
                        return Ok(None);
                    }
                    Err(error) => {
                        record_error(
                            report,
                            log,
                            run_error(&tier, format!("bind Docker backend: {error}")),
                        );
                        return Ok(None);
                    }
                }
            }
            SourceKind::CargoTarget | SourceKind::Fs(_) => None,
        };
        Ok(Some(PreparedTier {
            tier,
            command_ready,
            docker_bound,
        }))
    }
}

/// A skip of `tier` as a whole (`candidate: null`), refused before its
/// discovery.
fn tier_skip(tier: &Tier, reason: Reason, detail: Option<String>) -> SkipEntry {
    SkipEntry {
        tier: tier.name.clone(),
        profile: tier.profile.clone(),
        candidate: None,
        count: 1,
        examples: Vec::new(),
        reason,
        detail,
        process_blockers: None,
        estimated_bytes: None,
    }
}
