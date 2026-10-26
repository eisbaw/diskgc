//! Command action on one fs candidate. The path, walk, lock, and fresh
//! in-use verdict are the same ones used by the delete action. Only the
//! final operation differs: an absolute preflighted program receives the
//! checked working-directory fd instead of the walk's delete record.

use std::path::Path;
use std::time::Duration;

use super::Found;
use super::discover::check_deadline;
use super::judge::{Judge, Reached, Walked, mountinfo, same_object, still_reached};
use super::reclaim::{Alternative, runtime_retention_guard_veto};
use crate::command_action::{self, Ready};
use crate::engine::{Checkouts, Identity, Reason, Skip, checkout_metadata_below};
use crate::host::{
    Base, Cause, CommandExit, CommandInvocation, Delete, FileType, HostError, MonoTime,
};
use crate::inuse::{CandidateLock, Checker, FreshAttempts, lock_candidate, locked_is_walked};
use crate::outcome::{ReclaimEnd, Stop};
use crate::profile::Cwd;

/// Whether the cleaner was invoked, and how the candidate ended. Even a
/// nonzero exit may have changed space, so the executor samples afterwards.
#[derive(Debug)]
pub struct CommandResult {
    pub attempted: bool,
    pub end: ReclaimEnd,
    /// Source whose fresh locked judgement selected the candidate.
    pub selected: Option<usize>,
}

/// One source's identity for a shared command invocation.
pub struct CommandSource<'a> {
    pub profile: &'a str,
    pub output_prefix: String,
}

/// The preflighted command and its invocation context for one candidate.
/// `sources` follows the alternatives in discovery order.
pub struct CommandSpec<'a> {
    pub ready: &'a Ready,
    pub argv: &'a [String],
    pub cwd: Option<Cwd>,
    pub need_bytes: u64,
    pub mount: &'a Path,
    pub sources: Vec<CommandSource<'a>>,
    pub deadline: Option<MonoTime>,
}

struct Admitted<'a, F> {
    index: usize,
    found: &'a Found,
    reached: Reached<F>,
    walked: Walked,
}

type LockedVerdict<'a, F> = Result<Admitted<'a, F>, (Option<usize>, Skip)>;

#[allow(clippy::too_many_arguments)]
pub fn run_command<H: Delete>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    ready: &Ready,
    argv: &[String],
    cwd: Option<Cwd>,
    checker: &mut Checker<'_>,
    need_bytes: u64,
    mount: &Path,
    profile: &str,
    output_prefix: String,
    deadline: Option<MonoTime>,
) -> CommandResult {
    run_command_checked(
        host,
        judge,
        found,
        checker,
        CommandSpec {
            ready,
            argv,
            cwd,
            need_bytes,
            mount,
            sources: vec![CommandSource {
                profile,
                output_prefix,
            }],
            deadline,
        },
        &mut |_| Ok(None),
    )
}

/// Run the command after rechecking runtime policy under the candidate lock.
/// The policy is checked after the fresh walk and again immediately before
/// invoking the command; uncertainty stops the run.
pub fn run_command_checked<H: Delete>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    checker: &mut Checker<'_>,
    command: CommandSpec<'_>,
    policy: &mut impl FnMut(&Path) -> Result<Option<Skip>, Stop>,
) -> CommandResult {
    run_command_alternatives_checked(
        host,
        &[Alternative {
            judge: *judge,
            found,
            checkouts: judge.rules.checkouts,
            admitted: true,
        }],
        checker,
        command,
        &mut |_, path| policy(path),
    )
}

/// Judge every source of one inode while holding one candidate lock, then
/// invoke the common command once with the selected source's identity.
/// `command.sources` corresponds to `alternatives` in discovery order.
pub fn run_command_alternatives_checked<H: Delete>(
    host: &H,
    alternatives: &[Alternative<'_>],
    checker: &mut Checker<'_>,
    command: CommandSpec<'_>,
    policy: &mut impl FnMut(usize, &Path) -> Result<Option<Skip>, Stop>,
) -> CommandResult {
    assert!(alternatives.first().is_some_and(|source| source.admitted));
    assert_eq!(alternatives.len(), command.sources.len());
    let mut result = CommandResult {
        attempted: false,
        end: ReclaimEnd::Complete,
        selected: None,
    };
    if let Err(end) = protocol(host, alternatives, checker, command, &mut result, policy) {
        result.end = end;
    }
    result
}

fn protocol<H: Delete>(
    host: &H,
    alternatives: &[Alternative<'_>],
    checker: &mut Checker<'_>,
    command: CommandSpec<'_>,
    result: &mut CommandResult,
    policy: &mut impl FnMut(usize, &Path) -> Result<Option<Skip>, Stop>,
) -> Result<(), ReclaimEnd> {
    let CommandSpec {
        ready,
        argv,
        cwd,
        need_bytes,
        mount,
        sources,
        deadline,
    } = command;
    let found = alternatives[0].found;
    let path = &found.path;
    let stopped = |error| {
        ReclaimEnd::Stopped(Stop::Failed {
            path: path.to_owned(),
            error,
        })
    };
    checker
        .serves(alternatives[0].judge.rules)
        .map_err(stopped)?;
    check_deadline(host, deadline, path)?;
    if let Some((index, skip)) =
        runtime_retention_guard_veto(host, alternatives, deadline).map_err(ReclaimEnd::Stopped)?
    {
        result.selected = Some(index);
        return Err(skip.into());
    }
    let mounts = mountinfo(host)?;
    let mut first_skip = None;
    let mut reached = None;
    for (index, alternative) in alternatives.iter().enumerate() {
        if !alternative.admitted {
            continue;
        }
        match alternative
            .judge
            .approach(host, alternative.found, &mounts, deadline)?
        {
            Ok(candidate) => {
                reached = Some((index, alternative.found, candidate));
                break;
            }
            Err(skip) => {
                first_skip.get_or_insert(skip);
            }
        }
    }
    let (_, lock_found, reached) =
        reached.ok_or_else(|| first_skip.expect("nonempty alternatives"))?;
    let candidate = reached.candidate(lock_found)?;
    let lock = lock_candidate(host, &candidate).map_err(stopped)??;
    if let Some(locked) = lock.locked() {
        let stat = host
            .statx(Base::Dir(locked.fd()), Path::new(""))
            .map_err(stopped)?;
        same_object(lock_found, Identity::of(&stat), "locked")?;
    }
    // A late policy change can exclude the first source while another still
    // admits the inode. Retry under the same lock, rejudging *all* terms so a
    // newly admitted refusing source can still veto a checkout. Each source
    // gets at most one final-boundary refusal; policy flapping cannot spin.
    let mut refused_at_boundary = vec![false; alternatives.len()];
    let mut boundary_skip = None;
    // One budget for the candidate, across alternatives: a busy host does
    // not get more attempts because a policy refused a source.
    let mut attempts = FreshAttempts::new(deadline).scan_not_before(super::contract_scan_floor(
        alternatives
            .iter()
            .filter(|a| a.admitted)
            .map(|a| (a.judge.find, a.found)),
    ));
    loop {
        let Admitted {
            index,
            found,
            reached,
            walked,
        } = match judge_locked_alternatives(
            host,
            alternatives,
            &lock,
            deadline,
            policy,
            &refused_at_boundary,
            boundary_skip.as_ref(),
        )? {
            Ok(selected) => selected,
            Err((source, skip)) => {
                result.selected = source;
                return Err(skip.into());
            }
        };
        result.selected = Some(index);
        let candidate = reached.candidate(found)?;
        let judge = &alternatives[index].judge;
        let Walked { record, .. } = walked;
        let use_candidate = match cwd {
            Some(Cwd::Candidate) => true,
            Some(Cwd::Parent) => false,
            None => found.identity.file_type == FileType::Directory,
        };
        let cwd_fd = if use_candidate {
            lock.locked()
                .expect("a walked directory has its candidate lock")
                .fd()
        } else {
            &reached.parent().fd
        };
        let run = command_action::request(
            ready,
            argv,
            command_action::Context {
                candidate: &found.path,
                need_bytes,
                mount,
                profile: sources[index].profile,
                label: sources[index].output_prefix.clone(),
                deadline: deadline.unwrap_or(MonoTime(Duration::MAX)),
            },
        );
        // The clear verdict is valid only briefly. Build the source-specific
        // argv first, then start immediately after the final policy scan.
        // The one freshness loop (`Checker::with_fresh_clear`) checks, spends
        // attempts and rescans; this closure is the last checks and the
        // command, or a late policy refusal of this source.
        let pass = checker.with_fresh_clear(
            host,
            &candidate,
            &record,
            &mut attempts,
            |scan, clear| -> Result<Option<CommandPass>, ReclaimEnd> {
                check_deadline(host, deadline, path)?;
                if let Some(app_lock) = &judge.find.app_not_running {
                    super::app::check(host, &found.root, app_lock, deadline)??;
                }
                // The contract is judged from the scan that just cleared it:
                // every discovering source's contract, on this same scan.
                let admitted: Vec<usize> = (0..alternatives.len())
                    .filter(|at| alternatives[*at].admitted)
                    .collect();
                if let Err((veto_index, skip)) = super::liveness::veto(
                    host,
                    scan,
                    admitted
                        .iter()
                        .map(|at| (alternatives[*at].judge.find, alternatives[*at].found)),
                    candidate.parent(),
                    &record,
                )? {
                    result.selected = Some(admitted[veto_index]);
                    return Err(skip.into());
                }
                // The path again, right before the command (TASK-165): it
                // gets the path text, and `protect` below judges it, so a
                // directory on the way renamed since the locked judgement is
                // `changed`.
                let mounts = mountinfo(host)?;
                judge.again(host, found, &reached, &mounts, deadline)??;
                if let Some(skip) = policy(index, &found.path).map_err(ReclaimEnd::Stopped)? {
                    return Ok(Some(CommandPass::RefusedAtBoundary(skip)));
                }
                if let Some((guard_index, skip)) =
                    runtime_retention_guard_veto(host, alternatives, deadline)
                        .map_err(ReclaimEnd::Stopped)?
                {
                    result.selected = Some(guard_index);
                    return Err(skip.into());
                }
                // The path once more as the last filesystem check before the
                // command: the checks above can take a while.
                still_reached(host, found, &reached, &mounts, deadline)??;
                check_deadline(host, deadline, path)?;
                // The policy may scan other trees and take longer than the
                // in-use verdict remains fresh. Recheck its lifetime at the
                // point of use.
                if host.now() >= clear.fresh_until() {
                    return Ok(None);
                }
                let (started, ended) = start_command(host, cwd_fd, &run, path);
                result.attempted |= started;
                Ok(Some(CommandPass::Ran(ended)))
            },
        )?;
        match pass {
            CommandPass::Ran(ended) => return ended,
            CommandPass::RefusedAtBoundary(skip) => {
                boundary_skip = Some((index, skip));
                refused_at_boundary[index] = true;
            }
        }
    }
}

/// How one pass of a command's freshness loop ended.
enum CommandPass {
    /// The command ran (or could not start): its end.
    Ran(Result<(), ReclaimEnd>),
    /// The tier's policy refused the selected source at the last boundary:
    /// judge the alternatives again under the same lock.
    RefusedAtBoundary(Skip),
}

/// Runs the command `run` for the candidate at `path`: whether it started,
/// and how it ended (exit 0 complete, any other exit or a signal an error
/// of the candidate, a deadline before or during it a stop of the run, the
/// sandbox gate an error of the run).
fn start_command<H: Delete>(
    host: &H,
    cwd_fd: &H::Fd,
    run: &crate::host::CommandRun,
    path: &Path,
) -> (bool, Result<(), ReclaimEnd>) {
    let exit = match host.run_command(cwd_fd, run) {
        CommandInvocation::Started(exit_result) => exit_result,
        CommandInvocation::DeadlineBeforeStart => {
            return (
                false,
                Err(ReclaimEnd::Stopped(Stop::DeadlinePassed {
                    path: path.to_owned(),
                })),
            );
        }
        CommandInvocation::NotStarted(error) if error.cause == Cause::SandboxGate => {
            return (
                false,
                Err(ReclaimEnd::GateRefused {
                    path: path.to_owned(),
                    error,
                }),
            );
        }
        CommandInvocation::NotStarted(error) => return (false, ended(Err(error), path)),
    };
    (true, ended(exit, path))
}

/// How a command that ran ended, as the end of its reclaim.
fn ended(exit: Result<CommandExit, HostError>, path: &Path) -> Result<(), ReclaimEnd> {
    match exit {
        Ok(CommandExit::Exited(0)) => Ok(()),
        Ok(CommandExit::Exited(code)) => Err(ReclaimEnd::Failed {
            path: path.to_owned(),
            error: HostError::invalid(
                "run command",
                path.display(),
                format!("cleaner exited with status {code}"),
            ),
        }),
        Ok(CommandExit::Signaled(signal)) => Err(ReclaimEnd::Failed {
            path: path.to_owned(),
            error: HostError::invalid(
                "run command",
                path.display(),
                format!("cleaner died from signal {signal}"),
            ),
        }),
        Ok(CommandExit::TimedOut) => Err(ReclaimEnd::Stopped(Stop::DeadlinePassed {
            path: path.to_owned(),
        })),
        Err(error) => Err(ReclaimEnd::Failed {
            path: path.to_owned(),
            error,
        }),
    }
}

/// A complete source decision under the held inode lock. The outer command
/// loop calls this again after a late policy refusal; no earlier walk or
/// checkout decision is reused for a different source context.
fn judge_locked_alternatives<'a, H: Delete>(
    host: &H,
    alternatives: &[Alternative<'a>],
    lock: &CandidateLock<H::Fd>,
    deadline: Option<MonoTime>,
    policy: &mut impl FnMut(usize, &Path) -> Result<Option<Skip>, Stop>,
    refused_at_boundary: &[bool],
    boundary_skip: Option<&(usize, Skip)>,
) -> Result<LockedVerdict<'a, H::Fd>, ReclaimEnd> {
    if let Some((index, skip)) =
        runtime_retention_guard_veto(host, alternatives, deadline).map_err(ReclaimEnd::Stopped)?
    {
        return Ok(Err((Some(index), skip)));
    }
    let mounts = mountinfo(host)?;
    let mut first_skip = None;
    let mut selected = None;
    let mut checkout_refusal = None;
    for (index, alternative) in alternatives.iter().enumerate() {
        if !alternative.admitted {
            continue;
        }
        let found = alternative.found;
        let judge = &alternative.judge;
        let reached = match judge.approach(host, found, &mounts, deadline)? {
            Ok(reached) => reached,
            Err(skip) => {
                first_skip.get_or_insert(skip);
                continue;
            }
        };
        let candidate = reached.candidate(found)?;
        if let Err(skip) = judge.again(host, found, &reached, &mounts, deadline)? {
            first_skip.get_or_insert(skip);
            continue;
        }
        if let Some(skip) = policy(index, &found.path).map_err(ReclaimEnd::Stopped)? {
            first_skip.get_or_insert(skip);
            continue;
        }
        let walked = match judge.contents(
            host,
            &reached,
            super::judge::CandidateView {
                candidate: &candidate,
                locked_fd: lock.locked().map(|held| held.fd()),
            },
            found,
            true,
            deadline,
        )? {
            Ok(walked) => walked,
            Err(skip) => {
                first_skip.get_or_insert(skip);
                continue;
            }
        };
        locked_is_walked(host, &candidate, lock, &walked.record).map_err(|error| {
            ReclaimEnd::Stopped(Stop::Failed {
                path: found.path.clone(),
                error,
            })
        })??;
        if alternative.checkouts == Checkouts::Refuse {
            let refusal = checkout_metadata_below(Checkouts::Refuse, &found.root, &found.path)
                .or_else(|| {
                    walked.record.contains_checkout().then(|| {
                        Skip::new(
                            Reason::ContainsCheckout,
                            found.path.clone(),
                            "an admitted source refuses a version-control checkout in this candidate",
                        )
                    })
                });
            if let Some(skip) = refusal {
                checkout_refusal.get_or_insert((index, skip));
            }
        }
        if selected.is_none() && !refused_at_boundary[index] {
            selected = Some(Admitted {
                index,
                found,
                reached,
                walked,
            });
        }
    }
    if let Some((index, skip)) = checkout_refusal {
        return Ok(Err((Some(index), skip)));
    }
    Ok(match selected {
        Some(selected) => Ok(selected),
        // A final-boundary refusal explains why the source that was about to
        // run did not. Earlier filtered alternatives are less informative.
        None if boundary_skip.is_some() => Err((
            boundary_skip.map(|(index, _)| *index),
            boundary_skip.expect("checked above").1.clone(),
        )),
        None => Err((
            None,
            first_skip.expect("every union alternative has a verdict"),
        )),
    })
}
