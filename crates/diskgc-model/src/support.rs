//! Test support shared by the model tests and the e2e test driver: the
//! deletion step of one filesystem candidate composed of diskgc-core's
//! parts, without the `fs` kind's discovery and judgement. It used to be
//! `diskgc_core::inuse::delete_unused`; it lives here, in a test-only
//! crate, so production code has one deletion path for checked candidates
//! (`fs_kind::reclaim` and the Cargo protocol, both through
//! `inuse::clear_and_delete`) and no weaker second one.

use diskgc_core::engine::{self, Candidate, DeleteOptions, Deletion, Rules, Skip, WalkStop};
use diskgc_core::host::{Delete, HostError};
use diskgc_core::inuse::{Checker, NotDeleted, clear_and_delete, lock_candidate, locked_is_walked};

/// Why [`delete_unused`] deleted nothing (or stopped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The candidate is not eligible or is in use: a skip with its reason.
    Skipped(Skip),
    /// The walk ended without a record (deadline, error of the run).
    Walk(WalkStop),
    /// A host error of the run (opening or locking the candidate, the
    /// process list, a scan slower than the freshness bound, a checker
    /// whose options disagree with the walk's rules).
    Failed(HostError),
}

/// The deletion step for one candidate, in the order of docs/safety.md,
/// Deletion, without its discovery and judgement: lock the candidate
/// directory, run the eligibility walk under the lock, check that the
/// locked directory is the walked one, then [`clear_and_delete`]. The lock
/// is held until the delete returns.
pub fn delete_unused<H: Delete>(
    host: &H,
    checker: &mut Checker<'_>,
    candidate: &Candidate<'_, H::Fd>,
    rules: &Rules<'_>,
    options: DeleteOptions,
) -> Result<Deletion, Stop> {
    checker.serves(rules).map_err(Stop::Failed)?;
    let lock = lock_candidate(host, candidate)
        .map_err(Stop::Failed)?
        .map_err(Stop::Skipped)?;
    let record = engine::walk(host, candidate, rules, options.deadline).map_err(Stop::Walk)?;
    locked_is_walked(host, candidate, &lock, &record)
        .map_err(Stop::Failed)?
        .map_err(Stop::Skipped)?;
    let done = clear_and_delete(host, checker, candidate, &record, options, &mut |_| Ok(()))
        .map_err(|e| match e {
            NotDeleted::Skipped(skip) => Stop::Skipped(skip),
            NotDeleted::Failed(error) => Stop::Failed(error),
        })?;
    drop(lock);
    Ok(done)
}
