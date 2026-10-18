//! The `fs` kind, read-only half (task-68, a spike): what a source
//! profile's `find` finds below its roots, and the judgement of each
//! candidate for a dry run. Deleting `fs` candidates is task-18; nothing
//! here can delete (it is given a [`crate::host::Host`], never a
//! `Delete`), and `executor::ensure` refuses a reclaiming run over `fs`
//! candidates.
//!
//! The data first:
//!
//! - a [`Pattern`] is one `find.paths` pattern, `{uid}` replaced, in
//!   segments ([`pattern`] has the matching rules);
//! - a [`Find`] is what a source profile finds: its patterns, the entry
//!   type, and its own `where` (`has` / `has_not`), which narrows what is
//!   a candidate at all;
//! - [`plan`] walks the roots by the patterns (never deeper than they
//!   say, no symlink followed, no mount crossed) and judges every
//!   candidate as it finds it, with the deletion engine's eligibility
//!   walk and the in-use check, into a [`Planned`].

mod pattern;
mod plan;

#[cfg(test)]
mod tests;

pub use pattern::{Pattern, RESERVED, Segment, path_matches};
pub use plan::{Eligible, Planned, plan};

use std::path::Path;

use crate::profile::{EntryType, Finds, Roots, Source, Where};

/// The name prefix of systemd's `PrivateTmp` directories: never
/// candidates, never descended into (docs/safety.md, Before the walk).
pub const PRIVATE_TMP_PREFIX: &str = "systemd-private-";

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

/// The examples of `source` (kind `fs`) that do not hold for the user
/// `uid`: each `match` path that no root and pattern finds, and each
/// `no_match` path that one does, as a message. Paths only (the entry type
/// and the source's `where` are not part of an example). What `diskgc
/// profiles test` will check (task-17).
pub fn example_failures(source: &Source, uid: u32) -> Vec<String> {
    let Some(find) = Find::of(source, uid) else {
        return Vec::new();
    };
    let uid_text = uid.to_string();
    let fixed: Option<Vec<String>> = match &source.roots {
        Roots::Fixed(roots) => Some(
            roots
                .iter()
                .map(|r| r.replace("{uid}", &uid_text))
                .collect(),
        ),
        Roots::Required => None,
    };
    let fixed_paths: Option<Vec<&Path>> = fixed
        .as_ref()
        .map(|roots| roots.iter().map(Path::new).collect());
    let finds = |path: &str| path_matches(fixed_paths.as_deref(), &find.patterns, Path::new(path));
    let mut failures = Vec::new();
    for path in &source.examples.matches {
        if !finds(path) {
            failures.push(format!("match example {path} is not found by the patterns"));
        }
    }
    for path in &source.examples.no_match {
        if finds(path) {
            failures.push(format!("no_match example {path} is found by the patterns"));
        }
    }
    failures
}
