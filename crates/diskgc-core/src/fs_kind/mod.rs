//! The `fs` kind (docs/safety.md, Filesystem candidates; docs/reference.md,
//! Profiles): what a source profile's `find` finds below its roots, the
//! judgement of each candidate, and its deletion.
//!
//! The data first:
//!
//! - a [`Pattern`] is one `find.paths` pattern, `{uid}` replaced, in
//!   segments ([`pattern`] has the matching rules);
//! - a [`Find`] is what a source profile finds: its patterns, the entry
//!   type, and its own `where` (`has` / `has_not`), which narrows what is
//!   a candidate at all;
//! - [`discover`] walks the roots by the patterns (never deeper than they
//!   say, no symlink followed, no mount crossed, mountinfo first) into a
//!   [`Discovery`]: every candidate [`Found`], with the root it was found
//!   under and its identity;
//! - [`Judge`] holds the one set of checks of a found candidate
//!   ([`judge`]): its approach (path rule, filesystem, mounts, reached
//!   again from its root, identity, keep markers on the way) and its
//!   contents (the source's `where`, the eligibility walk and the idle
//!   requirement), each measured into a [`Tally`];
//! - [`plan`] is the dry run: discovery, then every candidate judged and
//!   checked against a process scan, into a [`Planned`]. It is given a
//!   [`crate::host::Host`] only, so it cannot delete;
//! - [`tally`] and [`reclaim`] are the reclaim's: a read-only tally of a
//!   candidate for the tier's order, and the deletion protocol of one
//!   candidate (docs/safety.md, Deletion): reached again, `flock`ed,
//!   judged again under the lock, checked against a fresh process scan,
//!   and deleted by the engine's identity-checked delete. Given a
//!   [`crate::host::Delete`] host.

mod discover;
mod judge;
mod pattern;
mod plan;
mod reclaim;

#[cfg(test)]
mod tests;

pub use discover::{Discovery, Found, discover};
pub use judge::{Judge, Reached, Tally, Walked};
pub use pattern::{Pattern, RESERVED, Segment, path_matches};
pub use plan::{Planned, plan};
pub use reclaim::{Reclaimed, reclaim, tally};

use crate::profile::{EntryType, Finds, Source, Where};

pub use crate::engine::{PRIVATE_TMP_PREFIX, is_private_tmp, path_in_private_tmp};

/// What a kind `fs` source profile finds (docs/reference.md, Profiles),
/// compiled for one user (`{uid}` replaced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Find {
    pub patterns: Vec<Pattern>,
    pub entry_type: EntryType,
    /// The source's own `where`: a file the candidate must contain
    /// (directly in it, as a regular file) ...
    pub has: Option<String>,
    /// ... and one it must not.
    pub has_not: Option<String>,
}

impl Find {
    /// The `find` of `source` for the user `uid`; `None` when the source
    /// is not of kind `fs`.
    pub fn of(source: &Source, uid: u32) -> Option<Self> {
        match &source.finds {
            Finds::Paths {
                patterns,
                entry_type,
            } => Some(Self::new(patterns, *entry_type, &source.where_, uid)),
            Finds::CargoTargets => None,
        }
    }

    /// The `find` of a kind `fs` source with these patterns, entry type and
    /// own `where`, for the user `uid`.
    pub fn new(patterns: &[String], entry_type: EntryType, where_: &Where, uid: u32) -> Self {
        Self {
            patterns: patterns.iter().map(|p| Pattern::compile(p, uid)).collect(),
            entry_type,
            has: where_.has.clone(),
            has_not: where_.has_not.clone(),
        }
    }
}
