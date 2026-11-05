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
//!
//! Candidates are judged one at a time, in the order found; the host is
//! told [`READ_AHEAD`] candidates ahead which one comes next
//! ([`ReadAhead`], [`Host::read_ahead`]), so that a host whose walks wait
//! on the disk can read their metadata meanwhile. Nothing it reads is used
//! (TASK-325).

use std::path::{Path, PathBuf};

use super::discover::{Discovery, check_deadline, discover};
use super::judge::{Judge, Tally, mountinfo};
use super::source_anchor::SourceEvidence;
use super::{Find, Found};
use crate::engine::{Reason, Rules, Skip, WalkRecord};
use crate::host::{Host, MonoTime};
use crate::inuse::{Checker, PlanScan};
use crate::outcome::{Judged, Stop};
use crate::profile::IdleWindow;
use crate::resolve::{Want, resolve_in};

/// How many candidates ahead of its judgement a plan names a candidate to
/// the host ([`Host::read_ahead`]). Enough to keep a host's readers busy
/// while the judgement walks one candidate, few enough that what they read
/// is still cached when the judgement reaches it: a whole `/tmp` read
/// ahead was evicted again before the plan got there (TASK-325).
pub const READ_AHEAD: usize = 32;

/// The candidates a plan will judge, in its order, each named to the host
/// [`READ_AHEAD`] places before its judgement ([`ReadAhead::judging`]).
/// `None` is a place without a directory to read (a candidate discovery
/// already refused): it keeps its place in the count, so the distance
/// stays the same.
#[derive(Debug)]
pub struct ReadAhead<'p> {
    paths: Vec<Option<&'p Path>>,
    /// How many places have been named (or passed over) so far.
    named: usize,
}

impl<'p> ReadAhead<'p> {
    /// The plan's candidates, in the order it judges them.
    pub fn new(paths: impl IntoIterator<Item = Option<&'p Path>>) -> Self {
        Self {
            paths: paths.into_iter().collect(),
            named: 0,
        }
    }

    /// Called before the judgement of place `index` (from 0): names to
    /// `name` (the host's [`Host::read_ahead`]) every candidate up to
    /// `index + READ_AHEAD` (exclusive) not named yet, in order.
    pub fn judging(&mut self, index: usize, mut name: impl FnMut(&Path)) {
        let until = index.saturating_add(READ_AHEAD).min(self.paths.len());
        while self.named < until {
            if let Some(path) = self.paths[self.named] {
                name(path);
            }
            self.named += 1;
        }
    }
}

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
/// judged within the plan checker's scan reuse window; with
/// [`PlanScan::NotChecked`] none is made and candidates are not judged on
/// in-use or liveness evidence. `log` gets a progress line every 10,000
/// candidates.
#[allow(clippy::too_many_arguments)]
pub fn plan<H: Host>(
    host: &H,
    roots: &[PathBuf],
    find: &Find,
    space: crate::places::Space,
    rules: &Rules<'_>,
    idle: IdleWindow,
    scan: PlanScan,
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
    let mut checker = Checker::for_plan(rules, scan)
        .with_pid_lookup(find.liveness_contract.is_some())
        .with_pytest_facts(find.liveness_contract == Some(crate::profile::LivenessContract::Pytest))
        .with_project_facts(
            find.liveness_contract
                .is_some_and(crate::profile::LivenessContract::is_project),
        );
    let total = candidates.len();
    let mut read_ahead = ReadAhead::new(
        candidates
            .iter()
            .map(|(_, candidate)| candidate.as_ref().ok().map(|found| found.path.as_path())),
    );
    for (index, (path, candidate)) in candidates.iter().enumerate() {
        if host.interrupted().is_some() {
            out.unjudged = total - out.judged.len();
            return (out, None);
        }
        read_ahead.judging(index, |path| host.read_ahead(path));
        let source_evidence = candidate
            .as_ref()
            .ok()
            .and_then(|found| found.source_evidence.clone());
        let verdict = match candidate {
            Err(skip) => Err(skip.clone()),
            Ok(found) => match judge_one(host, &judge, found, &mut checker, deadline) {
                Ok(verdict) => verdict,
                Err(stop) => {
                    out.unjudged = total - out.judged.len();
                    return (out, Some(stop));
                }
            },
        };
        out.judged.push((path.clone(), source_evidence, verdict));
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
    Ok(
        match judge_term(host, judge, found, checker, deadline, true)? {
            TermVerdict::Admitted(tally, _) => Ok(tally),
            TermVerdict::Refused(skip) | TermVerdict::InUse(skip) => Err(skip),
        },
    )
}

/// One source term's plan judgement of a candidate ([`judge_term`]). A
/// union tells the two refusals apart (docs/reference.md, filesystem
/// unions): what the term's own evidence refuses, another term may still
/// admit; what the in-use check refuses is a fact about the candidate.
#[derive(Debug)]
pub enum TermVerdict {
    /// Every check passed: the candidate's tally, and its walk (a union
    /// plan judges the other terms' liveness contracts on it,
    /// [`contract_veto`]; boxed, it is most of the size).
    Admitted(Tally, Box<WalkRecord>),
    /// Refused by the term's own judgement: its place, `where`, filters,
    /// eligibility walk, idle requirement, liveness contract or toolchain
    /// guard.
    Refused(Skip),
    /// Refused by the in-use check against the process scan
    /// ([`crate::inuse::Scan::check`]: `in_use`, `in_use_unknown`, or a
    /// live socket's `contains_ipc`), after the term's own place, filters
    /// and walk admitted it (a liveness contract is not evidence for
    /// that: its refusal, even a static one, is judged after the scan, as
    /// the reclaim vetoes after it). It says what the host's processes do with the
    /// candidate, whichever term found it: the reclaim checks it once, for
    /// the term it selected ([`super::reclaim`]), so a union judges no
    /// later term for it.
    InUse(Skip),
}

/// [`judge_one`] for one term of a union: the same steps, the verdict
/// told apart ([`TermVerdict`]). `check_in_use` is false only for a term
/// judged after the union has selected another (for the union's checkout
/// policy): the reclaim checks in-use for the selected term alone. A term
/// that may still be selected is always checked, on its own walk and the
/// scan current now, never on a clearance another term's walk got from an
/// older scan (TASK-337 review).
pub fn judge_term<H: Host>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
    check_in_use: bool,
) -> Result<TermVerdict, Stop> {
    check_deadline(host, deadline, &found.path)?;
    let mounts = mountinfo(host)?;
    let reached = match judge.approach(host, found, &mounts, deadline)? {
        Ok(reached) => reached,
        Err(skip) => return Ok(TermVerdict::Refused(skip)),
    };
    let candidate = reached.candidate(found)?;
    let walked = match judge.contents(
        host,
        &reached,
        super::judge::CandidateView {
            candidate: &candidate,
            locked_fd: None,
        },
        found,
        true,
        deadline,
    )? {
        Ok(walked) => walked,
        Err(skip) => return Ok(TermVerdict::Refused(skip)),
    };
    // A project contract's walk and rules 1 and 2, before the scan: files
    // and ages only, no process read.
    let prepared = super::liveness::prepare(
        host,
        judge,
        found,
        candidate.parent(),
        &walked.record,
        deadline,
    )?;
    // A default survey's plan (crate::inuse::PlanScan::NotChecked) stops
    // here: a refusal of those static rules stands; otherwise no scan, no
    // contract rule on processes, and the candidate is "not checked".
    // In a union the candidate stays with this term, the first that admits
    // it: with no scan there is no in-use verdict to hand on (TASK-337).
    if !checker.checks_in_use() {
        if let super::liveness::Prepared::Refused(skip) = &prepared {
            return Ok(TermVerdict::Refused(skip.clone().measured(walked.bytes())));
        }
        return Ok(TermVerdict::Admitted(
            walked.tally(),
            Box::new(walked.record),
        ));
    }
    let scan = checker
        .current(host, deadline)
        .map_err(|e| e.at(&found.path))?;
    // The scan stops at the deadline (TASK-189), but a reused or finished
    // one may end right at it: leave this candidate unjudged rather than
    // publishing a late verdict.
    check_deadline(host, deadline, &found.path)?;
    // Refused after a complete walk: sized ([`Skip::measured_bytes`]).
    if check_in_use && let Err(skip) = scan.check(&candidate, &walked.record) {
        return Ok(TermVerdict::InUse(skip.measured(walked.bytes())));
    }
    if let Err(skip) = super::liveness::check(
        host,
        scan,
        judge.find,
        found,
        candidate.parent(),
        &walked.record,
        &prepared,
    )? {
        return Ok(TermVerdict::Refused(skip.measured(walked.bytes())));
    }
    let tally = walked.tally();
    Ok(TermVerdict::Admitted(tally, Box::new(walked.record)))
}

/// The union rule of liveness contracts and toolchain guards for the
/// plan: every admitted source that found the candidate and whose contract
/// recognises it, or whose toolchain guard keeps it, must permit it (the
/// refusing source's index in `sources` comes back), judged from
/// the plan's scan ([`super::liveness::veto`]; the reclaim applies the
/// same rule on its last scan). The candidate's parent is reached again
/// safely; one that cannot be is `unsafe_path`. `record` is the walk of
/// the candidate by the term that admitted it: every term found the same
/// path, so its tree is theirs.
pub fn contract_veto<'a, H: Host>(
    host: &H,
    sources: &[(Judge<'a>, &'a Found)],
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
    record: &WalkRecord,
) -> Result<Result<(), (usize, Skip)>, Stop> {
    let Some((_, first)) = sources.iter().find(|(judge, _)| {
        judge.find.liveness_contract.is_some() || judge.find.protect_toolchains.is_some()
    }) else {
        return Ok(Ok(()));
    };
    // A default survey judges no liveness contract (judge_term).
    if !checker.checks_in_use() {
        return Ok(Ok(()));
    }
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
    // The project contracts' walks, before the scan.
    let prepared = sources
        .iter()
        .map(|(judge, found)| {
            super::liveness::prepare(host, judge, found, &parent.fd, record, deadline)
        })
        .collect::<Result<Vec<_>, Stop>>()?;
    let scan = checker
        .current(host, deadline)
        .map_err(|e| e.at(&first.path))?;
    super::liveness::veto(
        host,
        scan,
        sources
            .iter()
            .zip(&prepared)
            .map(|((judge, found), prepared)| (judge.find, *found, prepared)),
        &parent.fd,
        record,
    )
}

#[cfg(test)]
mod tests {
    use super::{READ_AHEAD, ReadAhead};
    use std::path::{Path, PathBuf};

    /// TASK-325: every candidate with a directory is named exactly once, in
    /// the plan's order, before its own judgement and never more than
    /// [`READ_AHEAD`] places ahead of the judgement in progress; a place
    /// without one (a refused candidate) keeps its place in the count.
    #[test]
    fn each_candidate_is_named_once_read_ahead_places_before_its_judgement() {
        for total in [
            0,
            1,
            READ_AHEAD - 1,
            READ_AHEAD,
            READ_AHEAD + 1,
            3 * READ_AHEAD + 5,
        ] {
            let paths: Vec<Option<PathBuf>> = (0..total)
                .map(|i| (i % 7 != 3).then(|| PathBuf::from(format!("/tmp/c{i}"))))
                .collect();
            let mut ahead = ReadAhead::new(paths.iter().map(Option::as_deref));
            let mut named: Vec<PathBuf> = Vec::new();
            for index in 0..total {
                ahead.judging(index, |path: &Path| named.push(path.to_owned()));
                let expected: Vec<PathBuf> = paths
                    .iter()
                    .take(index + READ_AHEAD)
                    .flatten()
                    .cloned()
                    .collect();
                assert_eq!(named, expected, "{total} candidates, judging {index}");
            }
            let all: Vec<PathBuf> = paths.iter().flatten().cloned().collect();
            assert_eq!(named, all, "{total} candidates: each named once, in order");
        }
    }
}
