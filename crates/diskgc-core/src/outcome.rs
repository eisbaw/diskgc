//! How a judgement or a reclaim of one candidate ends, shared by the kinds
//! that enumerate candidates (`cargo-target`, `fs`): a verdict on the
//! candidate (a [`Skip`] with its reason), or an error of the run
//! ([`Stop`]) that says nothing about this candidate and would hit the next
//! one too (the engine's rule: `EMFILE`, `EIO`, the deadline).

use std::path::PathBuf;

use crate::engine::{End, Reason, Skip};
use crate::host::HostError;

/// A judgement that can also stop the run: `Ok(Ok(value))`, a skip of
/// the candidate (`Ok(Err(skip))`), or an error of the run (`Err(stop)`).
pub type Judged<T> = Result<Result<T, Skip>, Stop>;

/// Why a step stopped without a verdict on the candidate: an error of the
/// run, never a skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The deadline passed (at `path`).
    DeadlinePassed { path: PathBuf },
    /// The host failed at `path`.
    Failed { path: PathBuf, error: HostError },
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeadlinePassed { path } => write!(f, "deadline passed at {}", path.display()),
            Self::Failed { path, error } => write!(f, "{}: {error}", path.display()),
        }
    }
}

/// How the reclaim of one candidate ended (a Cargo target's protocol,
/// `cargo_target::reclaim`, or an `fs` candidate's, `fs_kind::reclaim`).
/// The executor maps each to the report the same way, whatever the kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimEnd {
    /// Everything the protocol deletes was deleted.
    Complete,
    /// A skip of the candidate, with its reason: before anything was
    /// deleted (`locked`, `changed`, `in_use`, `not_idle`, ...), or
    /// part-way (`changed`, `in_use`; the caller's count of removed entries
    /// says how far it got).
    Skipped(Skip),
    /// The engine's delete of an entry failed (`ENOTEMPTY`: something was
    /// created in it after its walk; `EACCES`, ...): an error for this
    /// candidate, part-way, never retried in the same run.
    Failed { path: PathBuf, error: HostError },
    /// The pre-release sandbox gate refused (docs/safety.md): an error of
    /// the whole run, never a skip; every later deletion is refused too.
    GateRefused { path: PathBuf, error: HostError },
    /// An error of the run: the deadline passed, or a host failure that
    /// says nothing about this candidate (`EIO`, `EMFILE`, a scan slower
    /// than the freshness bound, ...).
    Stopped(Stop),
}

impl From<Stop> for ReclaimEnd {
    fn from(stop: Stop) -> Self {
        Self::Stopped(stop)
    }
}

impl From<Skip> for ReclaimEnd {
    fn from(skip: Skip) -> Self {
        Self::Skipped(skip)
    }
}

impl ReclaimEnd {
    /// How the engine's delete of one record ended, as the end of a reclaim
    /// (`Ok`: complete, go on): a mismatch is a `changed` skip, the deadline
    /// a stop of the run, a refused operation an error of the candidate,
    /// the sandbox gate an error of the run.
    pub fn of(end: End) -> Result<(), ReclaimEnd> {
        match end {
            End::Complete => Ok(()),
            End::Changed { path, detail } => {
                Err(Self::Skipped(Skip::new(Reason::Changed, path, detail)))
            }
            End::DeadlinePassed { path } => Err(Self::Stopped(Stop::DeadlinePassed { path })),
            End::Failed { path, error } => Err(Self::Failed { path, error }),
            End::GateRefused { path, error } => Err(Self::GateRefused { path, error }),
        }
    }
}
