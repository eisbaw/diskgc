//! The deletion of `fs` candidates (docs/safety.md, Filesystem candidates:
//! Deletion), for a reclaiming `ensure`.
//!
//! [`tally`] is the read-only first pass that orders a tier (`oldest`):
//! [`Judge::approach`] and [`Judge::contents`] without the idle
//! requirement, no lock, no in-use check (docs/architecture.md, The reclaim
//! walk). A candidate it refuses is skipped for the run and never locked.
//!
//! [`reclaim`] is the protocol of one candidate, every check made again on
//! the filesystem as it is now, whatever the tally or a plan said (the
//! steps of docs/safety.md, Deletion):
//!
//! 1. and 2. the deadline; the mount table, read now; [`Judge::approach`]:
//!    the candidate's place (the checkout guard's path rule, `other_fs`, a
//!    mount at or inside it, never touched), **reached again** from its
//!    root (the root's mount line first, no symlink, no mount crossed), the
//!    object discovery found (identity: device, inode, owner, type, birth
//!    time; `changed`, `missing`), no keep marker on the way (a kept
//!    candidate is never locked);
//! 3. **`flock`ed** ([`lock_candidate`]: a directory, opened below its
//!    parent, exclusively, without waiting; held by anyone else: `locked`).
//!    The locked directory must be the object found (`changed`). A file
//!    candidate is not locked;
//! 4. **judged again under the lock**: the mount table read again and
//!    [`Judge::again`] (the path must still lead to the directories it was
//!    reached through and to the object found, `changed` otherwise; then
//!    the place on it: a mount made since step 1 is `contains_mount`,
//!    never looked into; the keep markers on the way), then [`Judge::contents`] (the
//!    source's `where`, the eligibility walk with the tier's rules, whose
//!    root must be the object found, and the idle requirement on its newest
//!    change, none known: never idle), and the walked root must be the one
//!    locked ([`locked_is_walked`]);
//! 5. and 6. [`clear_and_delete`]: **in use?** against a process scan
//!    within the freshness rules (docs/safety.md, In use: Freshness);
//!    an unsettled scan is retried once, and a persistently blocking one
//!    only skips candidates briefly. On that scan, the mount table read
//!    again and [`Judge::again`] once more (a directory on the way renamed
//!    during the walk), then the policy (`protect`) on the path that still
//!    leads to it. A clear candidate is **deleted**
//!    right after the check, exactly the record the walk made and the
//!    check cleared: every directory reopened and matched, every entry
//!    `statx`ed and matched right before its `unlinkat`, the deadline read
//!    right before each change. A file created in the candidate after the
//!    walk is never unlinked and fails its directory with `ENOTEMPTY`: an
//!    error for this candidate, never retried;
//! 7. the lock released (closed) when the protocol returns, whatever it
//!    returns.

use super::Found;
use super::discover::check_deadline;
use super::judge::{Judge, Reached, Tally, Walked, mountinfo, same_object, still_reached};
use crate::engine::{Checkouts, DeleteOptions, Identity, Reason, Skip, checkout_metadata_below};
use crate::host::{Base, Delete, Host, MonoTime};
use crate::inuse::{
    Checker, FRESHNESS, FreshAttempts, NotDeleted, clear_and_delete, lock_candidate,
    locked_is_walked,
};
use crate::outcome::{Judged, PolicyAt, ReclaimEnd, Stop};
use std::path::Path;

/// What [`reclaim`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimed {
    /// Entries unlinked, the candidate included if it was.
    pub removed_entries: usize,
    /// Their allocated bytes as the walk recorded them (an estimate; the
    /// measure is the observed change in available space).
    pub removed_bytes: u64,
    pub end: ReclaimEnd,
    /// Source alternative whose fresh lock-time judgement selected this
    /// candidate, if any. The executor uses this for truthful reporting.
    pub selected: Option<usize>,
}

/// A source term that found the same inode. Every alternative is checked
/// again while the one candidate lock is held. Its source predicates are
/// judged with checkout admission enabled; a checkout marker vetoes the
/// object only when an admitted alternative has `checkouts: refuse`.
#[derive(Clone, Copy)]
pub struct Alternative<'a> {
    pub judge: Judge<'a>,
    pub found: &'a Found,
    /// The source's checkout policy. Union judges walk with checkout
    /// admission enabled until their own filters have been judged.
    pub checkouts: Checkouts,
    /// Whether this source passed source-owned discovery evidence and may
    /// select an action. Raw guard contributors set this to false.
    pub admitted: bool,
}

/// A positive retention rule on any discovering source vetoes the shared
/// inode. No term-local admission decision is used here. An uncertain guard
/// path or version state is a refusal, and a host failure stops the run.
pub fn retention_guard_veto<H: Host>(
    host: &H,
    alternatives: &[Alternative<'_>],
    deadline: Option<MonoTime>,
) -> Result<Option<(usize, Skip)>, Stop> {
    if !alternatives
        .iter()
        .any(|alternative| alternative.judge.find.has_retention_guard())
    {
        return Ok(None);
    }
    let mounts = mountinfo(host)?;
    for (index, alternative) in alternatives.iter().enumerate() {
        if let Err(skip) =
            alternative
                .judge
                .retention_guard(host, alternative.found, &mounts, deadline)?
        {
            return Ok(Some((index, skip)));
        }
    }
    Ok(None)
}

/// The extra candidate-wide pass belongs to unions. A single-source
/// protocol retains its established term-local evidence-before-version
/// ordering inside `Judge::contents`.
pub(super) fn runtime_retention_guard_veto<H: Host>(
    host: &H,
    alternatives: &[Alternative<'_>],
    deadline: Option<MonoTime>,
) -> Result<Option<(usize, Skip)>, Stop> {
    if alternatives.len() < 2 {
        return Ok(None);
    }
    retention_guard_veto(host, alternatives, deadline)
}

/// The tally of `found` for the tier's order (see the module docs): no
/// lock, no idle requirement, no in-use check, nothing deleted.
pub fn tally<H: Host>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    deadline: Option<MonoTime>,
) -> Judged<Tally> {
    tally_with_idle(host, judge, found, false, deadline)
}

/// A union term's pre-queue verdict. The idle requirement is included so a
/// term that is not eligible cannot impose its checkout policy on another
/// term before candidates are ranked.
pub fn tally_for_union<H: Host>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    deadline: Option<MonoTime>,
) -> Judged<Tally> {
    tally_with_idle(host, judge, found, true, deadline)
}

fn tally_with_idle<H: Host>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    idle: bool,
    deadline: Option<MonoTime>,
) -> Judged<Tally> {
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
        idle,
        deadline
    ));
    Ok(Ok(walked.tally()))
}

/// The deletion protocol of one candidate (see the module docs).
/// `checker` must have been built from the judge's rules
/// ([`Checker::new`]); `make_writable` is the profile's `delete` option.
/// Starts no operation after `deadline`.
pub fn reclaim<H: Delete>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    make_writable: bool,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
) -> Reclaimed {
    reclaim_alternatives(
        host,
        &[Alternative {
            judge: *judge,
            found,
            checkouts: judge.rules.checkouts,
            admitted: true,
        }],
        make_writable,
        checker,
        deadline,
    )
}

/// Reclaim an inode found by one or more positive filesystem terms. The
/// caller deduplicates by identity. For a union, the judges can use
/// `checkouts: allow` to evaluate predicates first; each alternative keeps
/// its source policy for the final refusal among admitted terms.
pub fn reclaim_alternatives<H: Delete>(
    host: &H,
    alternatives: &[Alternative<'_>],
    make_writable: bool,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
) -> Reclaimed {
    reclaim_alternatives_checked(
        host,
        alternatives,
        make_writable,
        checker,
        deadline,
        &mut |_, _, _| Ok(None),
    )
}

/// Recheck runtime policy while holding the candidate lock, first while
/// judging ([`PolicyAt::BeforeWalk`]) and again after the fresh walk
/// immediately before deletion ([`PolicyAt::AfterWalk`], with the newest
/// change the walk saw). A refusal carries its exact reason; an uncertain
/// lookup stops the whole run.
pub fn reclaim_alternatives_checked<H: Delete>(
    host: &H,
    alternatives: &[Alternative<'_>],
    make_writable: bool,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
    policy: &mut impl FnMut(usize, &Path, PolicyAt) -> Result<Option<Skip>, Stop>,
) -> Reclaimed {
    assert!(alternatives.first().is_some_and(|source| source.admitted));
    let mut done = Reclaimed {
        removed_entries: 0,
        removed_bytes: 0,
        end: ReclaimEnd::Complete,
        selected: None,
    };
    // One scan serves consecutive candidates while each deletion starts on
    // it younger than FRESHNESS (TASK-203, as TASK-173 did for the parts of
    // a Cargo target), not only the checks made within the 500 ms reuse
    // window: on a busy laptop a scan takes 0.3-1 s and a session's walk
    // and delete 0.1-0.3 s, so that window made every candidate rescan (one
    // scan per candidate, ~1-2 h for 6400 pytest sessions). Each deletion
    // still starts on a scan younger than FRESHNESS (`clear_and_delete`);
    // what is reused longer is only the answer about processes
    // (docs/safety.md, In use: Freshness).
    checker.reusing_for(FRESHNESS, |checker| {
        if let Err(end) = protocol(
            host,
            alternatives,
            make_writable,
            checker,
            deadline,
            &mut done,
            policy,
        ) {
            done.end = end;
        }
    });
    done
}

fn protocol<H: Delete>(
    host: &H,
    alternatives: &[Alternative<'_>],
    make_writable: bool,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
    done: &mut Reclaimed,
    policy: &mut impl FnMut(usize, &Path, PolicyAt) -> Result<Option<Skip>, Stop>,
) -> Result<(), ReclaimEnd> {
    let found = alternatives[0].found;
    let path = &found.path;
    let failed = |error| {
        ReclaimEnd::from(Stop::Failed {
            path: path.clone(),
            error,
        })
    };
    checker
        .serves(alternatives[0].judge.rules)
        .map_err(failed)?;
    // A deadline wins over candidate-local skips, as in the single-source
    // protocol.
    check_deadline(host, deadline, path)?;
    if let Some((index, skip)) =
        runtime_retention_guard_veto(host, alternatives, deadline).map_err(ReclaimEnd::Stopped)?
    {
        done.selected = Some(index);
        return Err(skip.into());
    }
    // Steps 1 and 2: keep markers are checked before the lock. A union's
    // checkout policy depends on predicates rechecked under that lock.
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
    let (locked_index, lock_found, reached) =
        reached.ok_or_else(|| first_skip.expect("nonempty alternatives"))?;
    let candidate = reached.candidate(lock_found)?;
    // Step 3. Held until this function returns (step 7).
    let lock = lock_candidate(host, &candidate).map_err(failed)??;
    if let Some(locked) = lock.locked() {
        let stat = host
            .statx(Base::Dir(locked.fd()), Path::new(""))
            .map_err(failed)?;
        same_object(lock_found, Identity::of(&stat), "locked")?;
    }
    // Step 4: each source term is judged afresh while the same inode is
    // locked. A term whose predicate changed can lose membership without
    // preventing another term from selecting the candidate.
    if let Some((index, skip)) =
        runtime_retention_guard_veto(host, alternatives, deadline).map_err(ReclaimEnd::Stopped)?
    {
        done.selected = Some(index);
        return Err(skip.into());
    }
    let mounts = mountinfo(host)?;
    first_skip = None;
    let mut original_reached = Some(reached);
    let mut selected: Option<(usize, &Found, Reached<H::Fd>, Walked)> = None;
    let mut checkout_refusal = None;
    for (index, alternative) in alternatives.iter().enumerate() {
        if !alternative.admitted {
            continue;
        }
        let found = alternative.found;
        let judge = &alternative.judge;
        let reached = if index == locked_index {
            original_reached
                .take()
                .expect("the locked reach is used once")
        } else {
            match judge.approach(host, found, &mounts, deadline)? {
                Ok(reached) => reached,
                Err(skip) => {
                    first_skip.get_or_insert(skip);
                    continue;
                }
            }
        };
        let candidate = reached.candidate(found)?;
        match judge.again(host, found, &reached, &mounts, deadline)? {
            Ok(()) => {}
            Err(skip) => {
                first_skip.get_or_insert(skip);
                continue;
            }
        }
        if let Some(skip) =
            policy(index, &found.path, PolicyAt::BeforeWalk).map_err(ReclaimEnd::Stopped)?
        {
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
        locked_is_walked(host, &candidate, &lock, &walked.record).map_err(failed)??;
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
                checkout_refusal.get_or_insert((index, skip.measured(walked.bytes())));
            }
        }
        if selected.is_none() {
            selected = Some((index, found, reached, walked));
        }
    }
    if let Some((index, skip)) = checkout_refusal {
        done.selected = Some(index);
        return Err(skip.into());
    }
    let (index, found, reached, walked) =
        selected.ok_or_else(|| first_skip.expect("every union alternative has a verdict"))?;
    let candidate = reached.candidate(found)?;
    let judge = &alternatives[index].judge;
    // Refused from here on, before anything is deleted: sized
    // ([`Skip::measured_bytes`]).
    let bytes = walked.bytes();
    let Walked { record, .. } = walked;
    done.selected = Some(index);
    let options = DeleteOptions {
        make_writable,
        deadline,
    };
    // The union rule's sources: the admitted ones, which claimed it.
    let (admitted_index, admitted): (Vec<usize>, Vec<&Alternative<'_>>) = alternatives
        .iter()
        .enumerate()
        .filter(|(_, a)| a.admitted)
        .unzip();
    let mut attempts = FreshAttempts::new(deadline).scan_not_before(super::contract_scan_floor(
        admitted.iter().map(|a| (a.judge.find, a.found)),
    ));
    // The project contracts' walks and rules 1 and 2, before the freshness
    // loop: a large project must not run into a deletion's freshness
    // window. Rule 3 is judged on the scan, in the veto below.
    let prepared = admitted
        .iter()
        .map(|a| {
            super::liveness::prepare(
                host,
                &a.judge,
                a.found,
                candidate.parent(),
                &record,
                deadline,
            )
        })
        .collect::<Result<Vec<_>, Stop>>()?;
    // The delete unlinks only what this walk listed (a file created in the
    // candidate since is never unlinked): the policy learns what it saw.
    let after_walk = PolicyAt::AfterWalk {
        newest_change: record.newest_change(),
    };
    let deletion = match clear_and_delete(
        host,
        checker,
        &candidate,
        &record,
        options,
        &mut attempts,
        &mut |scan| {
            // The path again, right before the deletion (TASK-165): the
            // walk and the scan took time, and a directory on the way
            // renamed meanwhile (under a keep marker, into a checkout, out
            // of the root) leaves every fd held valid. `protect` below
            // judges the path text, so it runs only on a path that still
            // leads here.
            let mounts = mountinfo(host).map_err(NotDeleted::Stopped)?;
            judge
                .again(host, found, &reached, &mounts, deadline)
                .map_err(NotDeleted::Stopped)?
                .map_err(NotDeleted::Skipped)?;
            if let Some(skip) =
                policy(index, &found.path, after_walk).map_err(NotDeleted::Stopped)?
            {
                return Err(NotDeleted::Skipped(skip));
            }
            // Every discovering source's contract, on this same scan.
            if let Err((veto_index, skip)) = super::liveness::veto(
                host,
                scan,
                admitted
                    .iter()
                    .zip(&prepared)
                    .map(|(a, prepared)| (a.judge.find, a.found, prepared)),
                candidate.parent(),
                &record,
            )
            .map_err(NotDeleted::Stopped)?
            {
                done.selected = Some(admitted_index[veto_index]);
                return Err(NotDeleted::Skipped(skip));
            }
            if let Some(app_lock) = &judge.find.app_not_running {
                super::app::check(host, &found.root, app_lock, deadline)
                    .map_err(NotDeleted::Stopped)?
                    .map_err(NotDeleted::Skipped)?;
            }
            if let Some((guard_index, skip)) =
                runtime_retention_guard_veto(host, alternatives, deadline)
                    .map_err(NotDeleted::Stopped)?
            {
                done.selected = Some(guard_index);
                return Err(NotDeleted::Skipped(skip));
            }
            // The checks above can take a while (the policy may scan other
            // trees): the path once more as the last check before the
            // delete, so the window after it is the delete itself.
            still_reached(host, found, &reached, &mounts, deadline)
                .map_err(NotDeleted::Stopped)?
                .map_err(NotDeleted::Skipped)?;
            Ok(())
        },
    ) {
        Ok(deletion) => deletion,
        Err(NotDeleted::Skipped(skip)) => {
            check_deadline(host, deadline, path)?;
            return Err(skip.measured(bytes).into());
        }
        Err(NotDeleted::Stopped(stop)) => return Err(ReclaimEnd::Stopped(stop)),
    };
    done.removed_entries = deletion.removed_entries;
    done.removed_bytes = deletion.removed_bytes;
    ReclaimEnd::of(deletion.end)
}
