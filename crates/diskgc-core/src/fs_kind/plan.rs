//! The dry run of an `fs` tier: [`super::discover`], then every candidate
//! found judged as the reclaim judges it ([`super::judge`]: its place,
//! reached again from its root, the source's `where`, the eligibility walk
//! and the idle requirement) and checked against a process scan. The
//! reclaim ([`super::reclaim`]) runs the same discovery and the same
//! judgement (under the candidate's `flock`, against a fresh scan), so a
//! plan predicts what a reclaim does, `locked` aside: a plan takes no lock,
//! so a `flock`ed candidate cannot be told.
//!
//! The mount table is read again for each candidate: a plan of a large
//! `/tmp` takes minutes, and a mount made meanwhile must not be `statx`ed.

use std::path::PathBuf;

use super::discover::{Discovery, check_deadline, discover};
use super::judge::{Judge, Tally, mountinfo};
use super::source_anchor::SourceEvidence;
use super::{Find, Found};
use crate::engine::{Reason, Rules, Skip, WalkRecord};
use crate::host::{Dev, Host, MonoTime};
use crate::inuse::Checker;
use crate::outcome::{Judged, Stop};
use crate::profile::IdleWindow;
use crate::resolve::{Want, resolve_in};

/// What [`plan`] found.
#[derive(Debug, Default)]
pub struct Planned {
    /// Every candidate judged, in the order found.
    pub judged: Vec<(PathBuf, Option<SourceEvidence>, Result<Tally, Skip>)>,
    /// Roots and directories on the way that could not be read or must not
    /// be (discovery's, [`Discovery::refused`]).
    pub refused: Vec<Skip>,
    /// Candidates found again through another root or pattern: listed once.
    pub duplicates: Vec<PathBuf>,
    /// `systemd-private-*` names a pattern matched and roots inside one
    /// ([`Discovery::private_tmp`]).
    pub private_tmp: usize,
    /// Candidates found but not judged: the run stopped first.
    pub unjudged: usize,
}

/// Finds the candidates of `find` below `roots` (absolute, `{uid}`
/// replaced) and judges each (see the module docs). `space` is `PATH`'s
/// filesystem's ([`crate::places::space_of`]). Errors of the run (a failing disk, no fds left, a
/// process scan that fails, the deadline) stop it: they are returned with
/// what was judged until then. Anything about one root, directory or
/// candidate is in the result. A process scan serves the candidates
/// judged within the plan checker's scan reuse window. `log` gets a
/// progress line every 10,000 candidates.
#[allow(clippy::too_many_arguments)]
pub fn plan<H: Host>(
    host: &H,
    roots: &[PathBuf],
    find: &Find,
    space: Dev,
    rules: &Rules<'_>,
    idle: IdleWindow,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
) -> (Planned, Option<Stop>) {
    let (found, stopped) = discover(host, roots, find, deadline);
    let Discovery {
        candidates,
        refused,
        duplicates,
        private_tmp,
        ..
    } = found;
    let mut out = Planned {
        refused,
        duplicates,
        private_tmp,
        ..Planned::default()
    };
    if stopped.is_some() {
        // Discovery did not finish: what it found is not the tier's whole
        // set, and judging it would only take longer past the deadline.
        out.unjudged = candidates.len();
        return (out, stopped);
    }
    if host.interrupted().is_some() {
        out.unjudged = candidates.len();
        return (out, None);
    }
    let judge = Judge {
        find,
        rules,
        idle,
        space,
    };
    let mut checker = Checker::for_plan(rules)
        .with_pid_lookup(find.liveness_contract.is_some())
        .with_pytest_facts(
            find.liveness_contract == Some(crate::profile::LivenessContract::Pytest),
        );
    let total = candidates.len();
    for (path, candidate) in candidates {
        if host.interrupted().is_some() {
            out.unjudged = total - out.judged.len();
            return (out, None);
        }
        let source_evidence = candidate
            .as_ref()
            .ok()
            .and_then(|found| found.source_evidence.clone());
        let verdict = match candidate {
            Err(skip) => Err(skip),
            Ok(found) => match judge_one(host, &judge, &found, &mut checker, deadline) {
                Ok(verdict) => verdict,
                Err(stop) => {
                    out.unjudged = total - out.judged.len();
                    return (out, Some(stop));
                }
            },
        };
        out.judged.push((path, source_evidence, verdict));
        if out.judged.len().is_multiple_of(10_000) {
            log(&format!(
                "{} of {total} candidates judged so far",
                out.judged.len()
            ));
        }
    }
    (out, None)
}

/// One candidate's judgement for the plan: every step of [`super::judge`],
/// then the in-use check against the plan's scan.
pub fn judge_one<H: Host>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
) -> Judged<Tally> {
    Ok(judge_walked(host, judge, found, checker, deadline)?.map(|(tally, _)| tally))
}

/// [`judge_one`], also returning the walk of an admitted candidate (a
/// union plan judges the other terms' liveness contracts on it,
/// [`contract_veto`]).
pub fn judge_walked<H: Host>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
) -> Judged<(Tally, WalkRecord)> {
    check_deadline(host, deadline, &found.path)?;
    let mounts = mountinfo(host)?;
    let reached = judged!(judge.approach(host, found, &mounts, deadline));
    let candidate = reached.candidate(found)?;
    let walked = judged!(judge.contents(
        host,
        &reached,
        super::judge::CandidateView {
            candidate: &candidate,
            locked_fd: None
        },
        found,
        true,
        deadline
    ));
    let scan = checker
        .current(host, deadline)
        .map_err(|e| e.at(&found.path))?;
    // The scan stops at the deadline (TASK-189), but a reused or finished
    // one may end right at it: leave this candidate unjudged rather than
    // publishing a late verdict.
    check_deadline(host, deadline, &found.path)?;
    if let Err(skip) = scan.check(&candidate, &walked.record) {
        return Ok(Err(skip));
    }
    judged!(super::liveness::check(
        host,
        scan,
        judge.find,
        found,
        candidate.parent(),
        &walked.record
    ));
    let tally = walked.tally();
    Ok(Ok((tally, walked.record)))
}

/// The union rule of liveness contracts for the plan: every admitted
/// source that found the candidate and whose contract recognises it must
/// permit it (the refusing source's index in `sources` comes back), judged from
/// the plan's scan ([`super::liveness::veto`]; the reclaim applies the
/// same rule on its last scan). The candidate's parent is reached again
/// safely; one that cannot be is `unsafe_path`. `record` is the walk of
/// the candidate by the term that admitted it: every term found the same
/// path, so its tree is theirs.
pub fn contract_veto<'a, H: Host>(
    host: &H,
    sources: &[(&'a Find, &'a Found)],
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
    record: &WalkRecord,
) -> Result<Result<(), (usize, Skip)>, Stop> {
    let Some((_, first)) = sources
        .iter()
        .find(|(find, _)| find.liveness_contract.is_some())
    else {
        return Ok(Ok(()));
    };
    check_deadline(host, deadline, &first.path)?;
    let mounts = mountinfo(host)?;
    let Some(parent) = first.path.parent() else {
        return Ok(Err((
            0,
            Skip::new(
                Reason::UnsafePath,
                first.path.clone(),
                "the candidate has no parent directory",
            ),
        )));
    };
    let parent = match resolve_in(host, &mounts, parent, Want::Directory) {
        Ok(parent) => parent,
        Err(e) => {
            return Ok(Err((
                0,
                Skip::new(
                    Reason::UnsafePath,
                    first.path.clone(),
                    format!("cannot safely reach the candidate's parent: {e}"),
                ),
            )));
        }
    };
    let scan = checker
        .current(host, deadline)
        .map_err(|e| e.at(&first.path))?;
    super::liveness::veto(host, scan, sources.iter().copied(), &parent.fd, record)
}
