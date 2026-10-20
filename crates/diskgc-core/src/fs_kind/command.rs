//! Command action on one fs candidate. The path, walk, lock, and fresh
//! in-use verdict are the same ones used by the delete action. Only the
//! final operation differs: an absolute preflighted program receives the
//! checked working-directory fd instead of the walk's delete record.

use std::path::Path;
use std::time::Duration;

use super::Found;
use super::discover::check_deadline;
use super::judge::{Judge, Walked, mountinfo, same_object};
use crate::command_action::{self, Ready};
use crate::engine::Identity;
use crate::host::{
    Base, Cause, CommandExit, CommandInvocation, Delete, FileType, HostError, MonoTime,
};
use crate::inuse::{Checker, lock_candidate, locked_is_walked};
use crate::outcome::{ReclaimEnd, Stop};
use crate::profile::Cwd;

/// Whether the cleaner was invoked, and how the candidate ended. Even a
/// nonzero exit may have changed space, so the executor samples afterwards.
#[derive(Debug)]
pub struct CommandResult {
    pub attempted: bool,
    pub end: ReclaimEnd,
}

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
    let mut result = CommandResult {
        attempted: false,
        end: ReclaimEnd::Complete,
    };
    if let Err(end) = protocol(
        host,
        judge,
        found,
        ready,
        argv,
        cwd,
        checker,
        need_bytes,
        mount,
        profile,
        output_prefix,
        deadline,
        &mut result.attempted,
    ) {
        result.end = end;
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn protocol<H: Delete>(
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
    attempted: &mut bool,
) -> Result<(), ReclaimEnd> {
    let path = &found.path;
    let stopped = |error| {
        ReclaimEnd::Stopped(Stop::Failed {
            path: path.clone(),
            error,
        })
    };
    checker.serves(judge.rules).map_err(stopped)?;
    check_deadline(host, deadline, path)?;
    let mounts = mountinfo(host)?;
    let reached = judge.approach(host, found, &mounts, deadline)??;
    let candidate = reached.candidate(found)?;
    let lock = lock_candidate(host, &candidate).map_err(stopped)??;
    if let Some(locked) = lock.locked() {
        let stat = host
            .statx(Base::Dir(locked.fd()), Path::new(""))
            .map_err(stopped)?;
        same_object(found, Identity::of(&stat), "locked")?;
    }
    let mounts = mountinfo(host)?;
    judge.again(host, found, &reached, &mounts, deadline)??;
    let Walked { record, .. } = judge.contents(
        host,
        &reached,
        super::judge::CandidateView {
            candidate: &candidate,
            locked_fd: lock.locked().map(|held| held.fd()),
        },
        found,
        true,
        deadline,
    )??;
    locked_is_walked(host, &candidate, &lock, &record).map_err(stopped)??;
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
            candidate: path,
            need_bytes,
            mount,
            profile,
            label: output_prefix,
            deadline: deadline.unwrap_or(MonoTime(Duration::MAX)),
        },
    );
    // A successful in-use clear is valid only briefly. Construct the argv
    // before the scan, then begin the command immediately after the final
    // freshness and deadline reads.
    for attempt in 0..2 {
        let clear = checker
            .check(host, &candidate, &record)
            .map_err(stopped)??;
        if host.now() >= clear.fresh_until() {
            if attempt == 0 {
                checker.forget();
                continue;
            }
            return Err(stopped(HostError::invalid(
                "command in-use check",
                path.display(),
                "two clear verdicts expired before the command could start",
            )));
        }
        check_deadline(host, deadline, path)?;
        if let Some(app_lock) = &judge.find.app_not_running {
            super::app::check(host, &found.root, app_lock, deadline)??;
        }
        let exit = match host.run_command(cwd_fd, &run) {
            CommandInvocation::Started(result) => {
                *attempted = true;
                result
            }
            CommandInvocation::DeadlineBeforeStart => {
                return Err(ReclaimEnd::Stopped(Stop::DeadlinePassed {
                    path: path.clone(),
                }));
            }
            CommandInvocation::NotStarted(error) if error.cause == Cause::SandboxGate => {
                return Err(ReclaimEnd::GateRefused {
                    path: path.clone(),
                    error,
                });
            }
            CommandInvocation::NotStarted(error) => Err(error),
        };
        return match exit {
            Ok(CommandExit::Exited(0)) => Ok(()),
            Ok(CommandExit::Exited(code)) => Err(ReclaimEnd::Failed {
                path: path.clone(),
                error: HostError::invalid(
                    "run command",
                    path.display(),
                    format!("cleaner exited with status {code}"),
                ),
            }),
            Ok(CommandExit::Signaled(signal)) => Err(ReclaimEnd::Failed {
                path: path.clone(),
                error: HostError::invalid(
                    "run command",
                    path.display(),
                    format!("cleaner died from signal {signal}"),
                ),
            }),
            Ok(CommandExit::TimedOut) => Err(ReclaimEnd::Stopped(Stop::DeadlinePassed {
                path: path.clone(),
            })),
            Err(error) => Err(ReclaimEnd::Failed {
                path: path.clone(),
                error,
            }),
        };
    }
    unreachable!("at most two freshness attempts")
}
