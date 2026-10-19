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
use std::time::Duration;

use super::discover::{Discovery, check_deadline, discover};
use super::judge::{Judge, Tally, mountinfo};
use super::{Find, Found};
use crate::engine::{Rules, Skip};
use crate::host::{Dev, Host, HostError, MonoTime};
use crate::inuse::{Options, REUSE_LIMIT, Scan, scan};
use crate::outcome::{Judged, Stop};
use crate::profile::IdleFor;

/// What [`plan`] found.
#[derive(Debug, Default)]
pub struct Planned {
    /// Every candidate judged, in the order found.
    pub judged: Vec<(PathBuf, Result<Tally, Skip>)>,
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

/// The process scans of a plan: one is reused for later candidates while
/// it is younger than `reuse` (a plan deletes nothing, so the deletion's
/// freshness bound, which `inuse::Checker` enforces, does not apply). A
/// scan that blocks every candidate (unsettled: processes kept starting
/// while it read; or an unreadable process of the user) is reused only
/// within [`REUSE_LIMIT`], as for a deletion, so one busy moment does not
/// mark a minute's worth of candidates `in_use`.
struct Scans<'o> {
    options: Options<'o>,
    reuse: Duration,
    latest: Option<Scan>,
}

impl Scans<'_> {
    fn current<H: Host>(&mut self, host: &H) -> Result<&Scan, HostError> {
        let now = host.now();
        let fresh = self.latest.as_ref().is_some_and(|s| {
            let reuse = if s.blocked().is_some() {
                self.reuse.min(REUSE_LIMIT)
            } else {
                self.reuse
            };
            now.saturating_since(s.started()) < reuse
        });
        if !fresh {
            self.latest = None;
            self.latest = Some(scan(host, &self.options)?);
        }
        Ok(self.latest.as_ref().expect("made above"))
    }
}

/// Finds the candidates of `find` below `roots` (absolute, `{uid}`
/// replaced) and judges each (see the module docs). `dev` is the device of
/// `PATH`'s filesystem. Errors of the run (a failing disk, no fds left, a
/// process scan that fails, the deadline) stop it: they are returned with
/// what was judged until then. Anything about one root, directory or
/// candidate is in the result. A process scan serves the candidates
/// judged within `scan_reuse` of its start. `log` gets a progress line
/// every 10,000 candidates.
#[allow(clippy::too_many_arguments)]
pub fn plan<H: Host>(
    host: &H,
    roots: &[PathBuf],
    find: &Find,
    dev: Dev,
    rules: &Rules<'_>,
    idle_for: Option<IdleFor>,
    scan_reuse: Duration,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
) -> (Planned, Option<Stop>) {
    let (found, stopped) = discover(host, roots, find, deadline);
    let Discovery {
        candidates,
        refused,
        duplicates,
        private_tmp,
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
    let judge = Judge {
        find,
        rules,
        idle_for,
        dev,
    };
    let mut scans = Scans {
        options: Options::from_rules(rules),
        reuse: scan_reuse,
        latest: None,
    };
    let total = candidates.len();
    for (path, candidate) in candidates {
        let verdict = match candidate {
            Err(skip) => Err(skip),
            Ok(found) => match judge_one(host, &judge, &found, &mut scans, deadline) {
                Ok(verdict) => verdict,
                Err(stop) => {
                    out.unjudged = total - out.judged.len();
                    return (out, Some(stop));
                }
            },
        };
        out.judged.push((path, verdict));
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
fn judge_one<H: Host>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    scans: &mut Scans<'_>,
    deadline: Option<MonoTime>,
) -> Judged<Tally> {
    check_deadline(host, deadline, &found.path)?;
    let mounts = mountinfo(host)?;
    let reached = judged!(judge.approach(host, found, &mounts, deadline));
    let candidate = reached.candidate(found)?;
    let walked = judged!(judge.contents(host, &reached, &candidate, found, true, deadline));
    let scan = scans.current(host).map_err(|error| Stop::Failed {
        path: found.path.clone(),
        error,
    })?;
    if let Err(skip) = scan.check(&candidate, &walked.record) {
        return Ok(Err(skip));
    }
    Ok(Ok(walked.tally()))
}
