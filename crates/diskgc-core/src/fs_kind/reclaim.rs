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
//!    [`Judge::again`] (the place: a mount made since step 1 is
//!    `contains_mount`, never looked into; the keep markers on the
//!    directories it was reached through), then [`Judge::contents`] (the
//!    source's `where`, the eligibility walk with the tier's rules, whose
//!    root must be the object found, and the idle requirement on its newest
//!    change, none known: never idle), and the walked root must be the one
//!    locked ([`locked_is_walked`]);
//! 5. and 6. [`clear_and_delete`]: **in use?** against a process scan
//!    within the freshness rules (docs/safety.md, In use: Freshness);
//!    an unsettled scan is retried once, and a persistently blocking one
//!    only skips candidates briefly. A clear candidate is **deleted**
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
use super::judge::{Judge, Reached, Tally, Walked, mountinfo, same_object};
use crate::engine::{Checkouts, DeleteOptions, Identity, Reason, Skip, checkout_metadata_below};
use crate::host::{Base, Delete, Host, MonoTime};
use crate::inuse::{Checker, NotDeleted, clear_and_delete, lock_candidate, locked_is_walked};
use crate::outcome::{Judged, ReclaimEnd, Stop};
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
        &mut |_, _| Ok(None),
    )
}

/// Recheck runtime policy while holding the candidate lock, first while
/// judging and again after the fresh walk immediately before deletion. A
/// refusal carries its exact reason; an uncertain lookup stops the whole run.
pub fn reclaim_alternatives_checked<H: Delete>(
    host: &H,
    alternatives: &[Alternative<'_>],
    make_writable: bool,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
    policy: &mut impl FnMut(usize, &Path) -> Result<Option<Skip>, Stop>,
) -> Reclaimed {
    assert!(alternatives.first().is_some_and(|source| source.admitted));
    let mut done = Reclaimed {
        removed_entries: 0,
        removed_bytes: 0,
        end: ReclaimEnd::Complete,
        selected: None,
    };
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
    done
}

fn protocol<H: Delete>(
    host: &H,
    alternatives: &[Alternative<'_>],
    make_writable: bool,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
    done: &mut Reclaimed,
    policy: &mut impl FnMut(usize, &Path) -> Result<Option<Skip>, Stop>,
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
                checkout_refusal.get_or_insert((index, skip));
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
    let deletion =
        match clear_and_delete(host, checker, &candidate, &record, options, &mut |scan| {
            if let Some(skip) = policy(index, &found.path).map_err(NotDeleted::Stopped)? {
                return Err(NotDeleted::Skipped(skip));
            }
            // Every discovering source's contract, on this same scan.
            if let Err((veto_index, skip)) = super::liveness::veto(
                host,
                scan,
                admitted.iter().map(|a| (a.judge.find, a.found)),
                candidate.parent(),
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
            Ok(())
        }) {
            Ok(deletion) => deletion,
            Err(NotDeleted::Skipped(skip)) => {
                check_deadline(host, deadline, path)?;
                return Err(skip.into());
            }
            Err(NotDeleted::Failed(error)) => return Err(failed(error)),
            Err(NotDeleted::Stopped(stop)) => return Err(ReclaimEnd::Stopped(stop)),
        };
    done.removed_entries = deletion.removed_entries;
    done.removed_bytes = deletion.removed_bytes;
    ReclaimEnd::of(deletion.end)
}
