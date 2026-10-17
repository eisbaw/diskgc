//! The `idle_for` predicate (docs/reference.md, Predicates): "not used or
//! changed for this long, as the kind measures it". Here: the requirement,
//! how several combine in one term, which measure each kind uses, and the
//! comparison. Reading the measure off the machine (the newest mtime or
//! ctime in a tree, a Cargo target's last build, Docker's record) is the
//! kinds' work (the tally, task-9, task-35); they hand the result to
//! [`IdleFor::verdict`].

use std::time::Duration;

use super::{Body, Library};
use crate::host::Timestamp;
use crate::kind::Kind;
use crate::select::{Factor, Term};

/// "Idle for at least this long."
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdleFor(pub Duration);

impl IdleFor {
    /// Whether something last used at `last_use` (by its kind's
    /// [`IdleMeasure`]) has been idle for at least this long at `now`.
    /// Exactly this long counts as idle.
    pub fn verdict(self, last_use: Option<Timestamp>, now: Timestamp) -> Idleness {
        let Some(last_use) = last_use else {
            return Idleness::Unknown(Unknown::Unanswered);
        };
        let nanos = |t: Timestamp| i128::from(t.secs) * 1_000_000_000 + i128::from(t.nanos);
        let idle = nanos(now) - nanos(last_use);
        if idle < 0 {
            Idleness::Unknown(Unknown::InFuture)
        } else if idle.unsigned_abs() >= self.0.as_nanos() {
            Idleness::Idle
        } else {
            Idleness::NotIdle
        }
    }
}

/// An [`IdleFor`] verdict. Three-way, so that an age diskgc cannot tell
/// selects nothing under either polarity: `filter:idle-3d` must not take
/// it as idle, and `!filter:idle-30d` must not take it as recent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Idleness {
    Idle,
    NotIdle,
    Unknown(Unknown),
}

impl Idleness {
    /// Whether the candidate passes `filter:F` (`negated` false) or
    /// `!filter:F` (`negated` true). [`Idleness::Unknown`] passes neither:
    /// the candidate is skipped with [`Unknown::skip_reason`].
    pub fn passes(self, negated: bool) -> bool {
        match self {
            Self::Idle => !negated,
            Self::NotIdle => negated,
            Self::Unknown(_) => false,
        }
    }
}

/// Why idleness cannot be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unknown {
    /// No last use: the kernel did not answer a timestamp the measure needs.
    Unanswered,
    /// The last use is after `now`: a clock step, or a file given a future
    /// time (`touch -d`, an archive). Its age says nothing.
    InFuture,
}

impl Unknown {
    /// The skip reason in reports (docs/reference.md, Output).
    pub fn skip_reason(self) -> &'static str {
        match self {
            Self::Unanswered => "unsupported_fs",
            Self::InFuture => "future_time",
        }
    }
}

/// How a kind measures "last used" (docs/reference.md, the `idle_for`
/// table). The report names the measure it used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdleMeasure {
    /// Kind `fs`: the newest mtime or ctime in the candidate's tree (atime
    /// is ignored: under `relatime` any reader refreshes it).
    NewestChange,
    /// Kind `cargo-target`: the newest mtime of the profile directories
    /// and their `.fingerprint/`, `deps/`, `build/`, `incremental/`
    /// (docs/safety.md, cargo-target: Last build).
    LastBuild,
    /// Kind `docker-build-cache`: last use as Docker records it.
    DockerLastUse,
}

impl IdleMeasure {
    /// The measure of `kind`; `None` for kinds without `idle_for`.
    pub fn of(kind: Kind) -> Option<Self> {
        match kind {
            Kind::Fs => Some(Self::NewestChange),
            Kind::CargoTarget => Some(Self::LastBuild),
            Kind::DockerBuildCache => Some(Self::DockerLastUse),
            Kind::DockerImages | Kind::NixStore => None,
        }
    }

    /// Its name in reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::NewestChange => "newest change",
            Self::LastBuild => "last build",
            Self::DockerLastUse => "last use recorded by Docker",
        }
    }
}

/// See [`Library::idle_for`].
pub(super) fn longest(library: &Library, term: &Term) -> Result<Option<IdleFor>, String> {
    if library.source(&term.source).is_none() {
        return Err(format!("no source profile is named `{}`", term.source));
    }
    // A source's own `where` has no `idle_for` (the loader refuses it):
    // when is the tier's choice, through its filters.
    let mut longest = None;
    for factor in &term.factors {
        let Factor::Filter(name) = factor else {
            continue;
        };
        let Some(Body::Filter(where_)) = library.get(name).map(|p| &p.body) else {
            return Err(format!("no filter is named `{name}`"));
        };
        longest = longest.max(where_.idle_for);
    }
    Ok(longest)
}
