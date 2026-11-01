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
//! 3. Under the host lock ([`Tallied::walk`]), in this order:
//!    [`Tallied::sample_and_protect`] (another reclaim may have freed the
//!    space; `protect` enumerated), [`Tallied::prepare_all`] (every tier's
//!    roots, command executables and Docker backend, [`Backend`]), and only
//!    then [`Tallied::reclaim`], which takes what preparation returned: no
//!    tier deletes before every tier was prepared.
//! 4. One exit after the tally ([`Ending`]): the snapshot
//!    ([`RunIndex::finish`]; declined, with one line, when the host lock
//!    was still held at the deadline), then the final sample that decides
//!    the status, then the host lock released. `finish` itself declines a
//!    write after a signal or at the deadline, with one log line, so every
//!    run that tallied either leaves a snapshot or says why not (PRD: each
//!    `ensure` that tallies leaves a snapshot).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::command_action::{self, Preflight, Ready};
use crate::docker_backend::{self, Binding, Bound};
use crate::engine::Reason;
use crate::host::{Delete, Host, MonoTime, SecondSignal};
use crate::hostlock::{self, LockError};
use crate::profile::Action;
use crate::report::{Final, Report, Status, after_lock_timeout, conclude, human_space_values};
use crate::snapshot::{Reclaimed, RunIndex};
use crate::tally::TallyState;
use crate::units::format_bytes;

use super::preflight::{
    Measured, deadline_before_first_sample, measure_failure, measure_until, preflight_roots,
    warn_if_host_lock_unready_timed,
};
use super::tier_policy::Protection;
use super::{
    NotesOncePerRun, Reclaim, Request, SourceKind, Tier, attempted_lock, deadline_of,
    deadline_passed, docker_candidate, docker_storage, final_sample, host_error, indexed,
    lock_refusal, record_deadline_overrun, record_error, run_error, sample, skip_entry, taken,
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
    let tiers = [(request.tier.clone(), Duration::ZERO)];
    timed(host, request, &tiers, owners, index, log)
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
    if tiers.len() != tier_timeouts.len() {
        return timed_report_from(
            host,
            request.watch.map(|w| (w.origin, w.origin_wall)),
            || {
                let message = format!(
                    "{} configured tier(s) with {} timeout(s): each tier needs one",
                    tiers.len(),
                    tier_timeouts.len()
                );
                usage(request.report(Status::Error, false), log, message)
            },
        );
    }
    let tiers: Vec<(Tier, Duration)> = tiers
        .iter()
        .cloned()
        .zip(tier_timeouts.iter().copied())
        .collect();
    timed(host, request, &tiers, owners, index, log)
}

/// [`run`], timed from the run's origin (a `watch` cycle's triggering
/// sample, or now).
fn timed<H: Delete>(
    host: &H,
    request: &Request,
    tiers: &[(Tier, Duration)],
    owners: &[u32],
    index: &mut RunIndex,
    log: &mut dyn FnMut(&str),
) -> Report {
    let origin = request.watch.map(|w| (w.origin, w.origin_wall));
    timed_report_from(host, origin, || {
        run(host, request, tiers, owners, index, log)
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

/// The deadline passed after the first sample, before the tally: the
/// final sample decides 0 or 124.
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

fn run<H: Delete>(
    host: &H,
    request: &Request,
    tiers: &[(Tier, Duration)],
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
    let Some((first, _)) = tiers.first() else {
        return usage(report, log, "configuration has no tiers to run".to_owned());
    };
    if tiers
        .iter()
        .any(|(tier, _)| tier.protections != first.protections)
    {
        return usage(
            report,
            log,
            "configured tiers have different protection policies".to_owned(),
        );
    }
    // Before anything else (the gate, the first sample, the host lock).
    if let Some(refused) = tiers.iter().find_map(|(tier, _)| reclaimable(tier).err()) {
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
        tiers.iter().map(|(tier, _)| tier),
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
    let run = Tallied {
        host,
        request,
        tiers,
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
        run.walk(index, &tallied.refused, &mut report, log)
    };
    // The snapshot, before the final sample: the sample and the exit code
    // see the space it takes (docs/architecture.md, Snapshot). A run that
    // waited for the host lock until its deadline writes none: the write
    // would come after the deadline (exit 75 never writes).
    match ending {
        Ending::LockTimeout => index.decline("the host lock was still held at the deadline", log),
        Ending::Decided | Ending::Concluded { .. } => index.finish(host, log),
    }
    ending.conclude(&run, &mut report, log);
    // Released only here, by closing, after the final sample.
    drop(host_lock);
    report
}

/// How a run that tallied ends, decided under (or waiting for) the host
/// lock and enacted by its one exit after the snapshot ([`run`]).
#[derive(Debug, Clone, Copy)]
enum Ending {
    /// The status is decided without a final sample: a signal, the host
    /// lock refused, or the sample under the lock failed.
    Decided,
    /// The deadline passed waiting for the host lock: no snapshot, and a
    /// fresh sample decides 0, 75 or 3.
    LockTimeout,
    /// The final sample concludes (0 when it meets `--min-free`, whatever
    /// else happened): after the walk, after an error of the run (a
    /// `protect` that cannot be enumerated), when the space was there under
    /// the lock, or when the deadline stopped the run (`deadline_hit`).
    /// `at` is the tier the run was at (in preparation or reclaiming), to
    /// which a failure of that sample is attributed.
    Concluded { deadline_hit: bool, at: usize },
}

impl Ending {
    fn conclude<H: Delete>(
        self,
        run: &Tallied<'_, H>,
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) {
        let (host, measured, deadline) = (run.host, run.measured, run.deadline);
        match self {
            Self::Decided => {}
            Self::LockTimeout => {
                // A fresh sample decides: the other holder may have freed enough.
                let first = &run.tiers[0].0;
                report.avail_after_bytes =
                    final_sample(host, measured, deadline, report, log, first);
                report.status =
                    after_lock_timeout(Final::of(report.avail_after_bytes, measured.min));
            }
            Self::Concluded { deadline_hit, at } => {
                let tier = &run.tiers[at].0;
                report.avail_after_bytes =
                    final_sample(host, measured, deadline, report, log, tier);
                report.status = conclude(
                    Final::of(report.avail_after_bytes, measured.min),
                    deadline_hit || deadline_passed(host, deadline),
                    !report.errors.is_empty(),
                );
            }
        }
    }
}

/// How a prepared tier acts: the one place its environment was checked
/// ([`Tallied::prepare_tier`]), passed to its reclaim. Each action kind has
/// exactly the backend it needs; a new kind (`nix-store`) adds a variant.
#[derive(Debug, Clone)]
pub(super) enum Backend {
    /// diskgc's own deletion: `delete` and the Cargo lock protocol.
    Files,
    /// A command action's resolved executables.
    Command(Ready),
    /// A Docker prune: the resolved client and the backend bound under the
    /// host lock (rebound right before the prune).
    Docker { client: Ready, bound: Bound },
}

impl Backend {
    /// The command of a command tier. Only an `Action::Command` tier runs a
    /// command action, and [`Tallied::prepare_tier`] gives every such tier
    /// a [`Backend::Command`].
    pub(super) fn command(&self) -> &Ready {
        match self {
            Self::Command(ready) => ready,
            Self::Files | Self::Docker { .. } => {
                unreachable!("a command action runs only with its command resolved")
            }
        }
    }
}

/// A tier ready to reclaim under the host lock: its roots on `PATH`'s
/// filesystem only, and its backend.
struct PreparedTier {
    tier: Tier,
    backend: Backend,
}

/// The run's deadline passed while a tier was prepared.
struct PastDeadline;

/// A run that tallied: what the walk under the host lock needs from it.
struct Tallied<'r, H: Delete> {
    host: &'r H,
    request: &'r Request,
    /// The tiers in policy order, each with its own timeout (zero: none).
    tiers: &'r [(Tier, Duration)],
    owners: &'r [u32],
    measured: &'r Measured<H::Fd>,
    deadline: Option<MonoTime>,
    run_started: MonoTime,
}

impl<H: Delete> Tallied<'_, H> {
    /// Everything under the host lock up to the end of the last tier: no
    /// tier is reclaimed before every tier was prepared.
    fn walk(
        &self,
        index: Option<&mut TallyState>,
        tally_refused: &BTreeMap<PathBuf, Reason>,
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) -> Ending {
        let protection = match self.sample_and_protect(report, log) {
            Ok(protection) => protection,
            Err(ending) => return ending,
        };
        let prepared = match self.prepare_all(report, log) {
            Ok(prepared) => prepared,
            Err(ending) => return ending,
        };
        self.reclaim(prepared, protection, index, tally_refused, report, log)
    }

    /// The sample under the lock (someone else, the reclaim we waited for,
    /// may have freed the space meanwhile), then `protect` enumerated before
    /// any tier may delete.
    fn sample_and_protect(
        &self,
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) -> Result<Protection, Ending> {
        let (host, measured, deadline) = (self.host, self.measured, self.deadline);
        let past_deadline = Ending::Concluded {
            deadline_hit: true,
            at: 0,
        };
        if deadline_passed(host, deadline) {
            return Err(past_deadline);
        }
        let started = host.now();
        let sampled = sample(host, measured);
        record_deadline_overrun(host, report, deadline, started, "statvfs", &measured.path);
        match sampled {
            Ok(space) if space.avail_bytes >= measured.min => {
                log(&format!(
                    "{} available now: --min-free is met, nothing to reclaim",
                    format_bytes(space.avail_bytes)
                ));
                return Err(Ending::Concluded {
                    deadline_hit: false,
                    at: 0,
                });
            }
            Ok(_) => {}
            Err(error) => {
                record_error(report, log, host_error(&self.tiers[0].0, error));
                return Err(Ending::Decided);
            }
        }
        if deadline_passed(host, deadline) {
            return Err(past_deadline);
        }
        self.protect(0, report, log)
    }

    /// `protect` enumerated for the tier at `at` (docs/safety.md, `protect`):
    /// once before any tier may delete, and again before each later tier, so
    /// that a tier's delete actions judge against its own (TASK-200). An
    /// enumeration that fails is an error of the run, which ends.
    fn protect(
        &self,
        at: usize,
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) -> Result<Protection, Ending> {
        let (host, deadline) = (self.host, self.deadline);
        let tier = &self.tiers[at].0;
        let started = host.now();
        let enumerated = Protection::enumerate(host, &tier.protections, deadline);
        record_deadline_overrun(
            host,
            report,
            deadline,
            started,
            "protect",
            &self.request.path,
        );
        if deadline_passed(host, deadline) {
            return Err(Ending::Concluded {
                deadline_hit: true,
                at,
            });
        }
        enumerated.map_err(|error| {
            record_error(report, log, run_error(tier, error));
            Ending::Concluded {
                deadline_hit: false,
                at,
            }
        })
    }

    /// Every tier prepared ([`Self::prepare_tier`]) before any may delete;
    /// `None` for a tier that is excluded (reported), the others still run.
    fn prepare_all(
        &self,
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) -> Result<Vec<Option<PreparedTier>>, Ending> {
        let past_deadline = |at| Ending::Concluded {
            deadline_hit: true,
            at,
        };
        let mut prepared = Vec::with_capacity(self.tiers.len());
        for (at, (tier, _)) in self.tiers.iter().enumerate() {
            if deadline_passed(self.host, self.deadline) {
                return Err(past_deadline(at));
            }
            match self.prepare_tier(tier, report, log) {
                Ok(tier) => prepared.push(tier),
                Err(PastDeadline) => return Err(past_deadline(at)),
            }
        }
        Ok(prepared)
    }

    /// Each prepared tier reclaimed in policy order, until the goal, the
    /// deadline, a signal or an error of the run.
    fn reclaim(
        &self,
        prepared: Vec<Option<PreparedTier>>,
        mut protection: Protection,
        mut index: Option<&mut TallyState>,
        tally_refused: &BTreeMap<PathBuf, Reason>,
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) -> Ending {
        let (host, measured, deadline) = (self.host, self.measured, self.deadline);
        let mut deadline_hit = false;
        // The tier the run is at: the last one that began.
        let mut at = 0;
        let mut a_tier_ran = false;
        for (index_of_tier, (prepared, (_, tier_timeout))) in
            prepared.into_iter().zip(self.tiers).enumerate()
        {
            if host.interrupted().is_some() {
                break;
            }
            let Some(PreparedTier { tier, backend }) = prepared else {
                continue;
            };
            at = index_of_tier;
            // Between tiers: the goal may be there. The first tier to run
            // starts from the sample taken under the lock.
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
                match self.protect(at, report, log) {
                    Ok(enumerated) => protection = enumerated,
                    Err(ending) => return ending,
                }
            }
            a_tier_ran = true;
            let mut reclaim = Reclaim {
                host,
                tier: &tier,
                measured,
                deadline: tier_deadline,
                tally_budget: self.request.tally_budget,
                backend,
                // Its own copy: a check that enumerates again replaces it
                // for the tier's later candidates; the next tier
                // enumerates anew.
                protection: protection.clone(),
                report: &mut *report,
                log: &mut *log,
                deadline_hit: false,
                run_stopped: false,
                in_use_unknown_in_a_row: 0,
                index: index.as_deref_mut(),
                tally_refused,
                run_started: self.run_started,
                second_signal: if self.request.watch.is_some() {
                    SecondSignal::LeaveRunning
                } else {
                    SecondSignal::WaitOutGrace
                },
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
        Ending::Concluded { deadline_hit, at }
    }

    /// `tier`'s run-time checks under the host lock, before any tier may
    /// delete: its roots ([`preflight_roots`]: one on another filesystem
    /// is skipped, an unsafe one excludes the tier) and its [`Backend`]:
    /// command executables (a tool not installed is a `missing` skip of the
    /// tier, docs/safety.md, Command actions) and the Docker backend
    /// (docs/reference.md, Backend skips and errors). `Ok(None)`: the tier
    /// is excluded, reported, and the other tiers still run.
    fn prepare_tier(
        &self,
        tier: &Tier,
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) -> Result<Option<PreparedTier>, PastDeadline> {
        let (host, deadline) = (self.host, self.deadline);
        let started = host.now();
        let roots = preflight_roots(host, tier, self.measured.dev);
        record_deadline_overrun(
            host,
            report,
            deadline,
            started,
            "preflight",
            &self.request.path,
        );
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
        let backend = match &tier.action {
            Action::Delete { .. } | Action::CargoLockProtocol => Backend::Files,
            Action::Command { argv, .. } => match self.command(&tier, argv, report, log)? {
                Some(ready) => Backend::Command(ready),
                None => return Ok(None),
            },
            Action::DockerBuilderPrune | Action::DockerImagePrune { .. } => {
                let Some(client) = self.command(&tier, &["docker".to_owned()], report, log)? else {
                    return Ok(None);
                };
                let Some(bound) = self.bind_docker(&tier, report, log)? else {
                    return Ok(None);
                };
                Backend::Docker { client, bound }
            }
        };
        Ok(Some(PreparedTier { tier, backend }))
    }

    /// `argv`'s executables looked up on the host (its shape was checked
    /// when the profile loaded). `Ok(None)`: the tier is excluded, as a
    /// `missing` skip or an error of the tier.
    fn command(
        &self,
        tier: &Tier,
        argv: &[String],
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) -> Result<Option<Ready>, PastDeadline> {
        let (host, deadline) = (self.host, self.deadline);
        let started = host.now();
        let preflight = command_action::preflight(host, argv);
        record_deadline_overrun(
            host,
            report,
            deadline,
            started,
            "preflight",
            &self.request.path,
        );
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
                Ok(Some(ready))
            }
            Ok(Preflight::Missing(program)) => {
                log(&format!(
                    "{}: profile `{}` is missing executable {program:?}",
                    tier.name, tier.profile
                ));
                report
                    .skipped
                    .push(skip_entry(tier, None, Reason::Missing, None));
                Ok(None)
            }
            Err(error) => {
                record_error(report, log, run_error(tier, error.to_string()));
                Ok(None)
            }
        }
    }

    /// The Docker backend of `tier` bound (docs/reference.md, Backend skips
    /// and errors). `Ok(None)`: the tier is excluded, as a skip of the
    /// backend or an error of the tier.
    fn bind_docker(
        &self,
        tier: &Tier,
        report: &mut Report,
        log: &mut dyn FnMut(&str),
    ) -> Result<Option<Bound>, PastDeadline> {
        let (host, deadline) = (self.host, self.deadline);
        let started = host.now();
        let inspected =
            docker_backend::inspect_backend(host, self.measured.dev, docker_storage(tier));
        record_deadline_overrun(
            host,
            report,
            deadline,
            started,
            "preflight",
            &self.request.path,
        );
        if deadline_passed(host, deadline) {
            return Err(PastDeadline);
        }
        match inspected {
            Ok(Binding::Bound(bound)) => {
                log(&format!(
                    "{}: Docker backend bound to {}",
                    tier.name,
                    docker_candidate(tier, &bound).display()
                ));
                Ok(Some(bound))
            }
            Ok(Binding::Skipped(skip)) => {
                log(&format!(
                    "{}: skip Docker backend: {}: {}",
                    tier.name, skip.reason, skip.detail
                ));
                report
                    .skipped
                    .push(skip_entry(tier, None, skip.reason, Some(skip.detail)));
                Ok(None)
            }
            Err(error) => {
                record_error(
                    report,
                    log,
                    run_error(tier, format!("bind Docker backend: {error}")),
                );
                Ok(None)
            }
        }
    }
}
