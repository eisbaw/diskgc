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
use super::judge::{Judge, Tally, Walked, mountinfo, same_object};
use crate::engine::{DeleteOptions, Identity};
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
}

/// The tally of `found` for the tier's order (see the module docs): no
/// lock, no idle requirement, no in-use check, nothing deleted.
pub fn tally<H: Host>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    deadline: Option<MonoTime>,
) -> Judged<Tally> {
    check_deadline(host, deadline, &found.path)?;
    let mounts = mountinfo(host)?;
    let reached = judged!(judge.approach(host, found, &mounts, deadline));
    let candidate = reached.candidate(found)?;
    let walked = judged!(judge.contents(host, &reached, &candidate, found, false, deadline));
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
    let mut done = Reclaimed {
        removed_entries: 0,
        removed_bytes: 0,
        end: ReclaimEnd::Complete,
    };
    if let Err(end) = protocol(
        host,
        judge,
        found,
        make_writable,
        checker,
        deadline,
        &mut done,
    ) {
        done.end = end;
    }
    done
}

fn protocol<H: Delete>(
    host: &H,
    judge: &Judge<'_>,
    found: &Found,
    make_writable: bool,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
    done: &mut Reclaimed,
) -> Result<(), ReclaimEnd> {
    let path = &found.path;
    let failed = |error| {
        ReclaimEnd::from(Stop::Failed {
            path: path.clone(),
            error,
        })
    };
    checker.serves(judge.rules).map_err(failed)?;
    // Steps 1 and 2: a kept candidate is never locked.
    check_deadline(host, deadline, path)?;
    let mounts = mountinfo(host)?;
    let reached = judge.approach(host, found, &mounts, deadline)??;
    let candidate = reached.candidate(found)?;
    // Step 3. Held until this function returns (step 7).
    let lock = lock_candidate(host, &candidate).map_err(failed)??;
    if let Some(locked) = lock.locked() {
        let stat = host
            .statx(Base::Dir(locked.fd()), Path::new(""))
            .map_err(failed)?;
        same_object(found, Identity::of(&stat), "locked")?;
    }
    // Step 4: everything judged again, under the lock.
    let mounts = mountinfo(host)?;
    judge.again(host, found, &reached, &mounts, deadline)??;
    let Walked { record, .. } =
        judge.contents(host, &reached, &candidate, found, true, deadline)??;
    locked_is_walked(host, &candidate, &lock, &record).map_err(failed)??;
    // Steps 5 and 6.
    let options = DeleteOptions {
        make_writable,
        deadline,
    };
    let deletion = match clear_and_delete(host, checker, &candidate, &record, options, &mut |_| {
        if let Some(lock) = &judge.find.app_not_running {
            super::app::check(host, &found.root, lock, deadline)
                .map_err(NotDeleted::Stopped)?
                .map_err(NotDeleted::Skipped)
        } else {
            Ok(())
        }
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
