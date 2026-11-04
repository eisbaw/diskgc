//! The `fs` kind (docs/safety.md, Filesystem candidates; docs/reference.md,
//! Profiles): what a source profile's `find` finds below its roots, the
//! judgement of each candidate, and its deletion.
//!
//! The data first:
//!
//! - a [`Pattern`] is one `find.paths` pattern, `{uid}` replaced, in
//!   segments ([`pattern`] has the matching rules);
//! - a [`Find`] is what a source profile finds: its patterns, the entry
//!   type, and its own `where` (`has` / `has_dir` / `has_not` / `lacks_entry`), which narrows what is
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

mod app;
mod command;
mod discover;
mod judge;
mod liveness;
mod pattern;
mod plan;
pub(crate) mod project;
mod reclaim;
mod source_anchor;
mod toolchain;

#[cfg(test)]
mod tests;

pub use command::{
    CommandResult, CommandSource, CommandSpec, run_command, run_command_alternatives_checked,
    run_command_checked,
};
pub use discover::{
    Beneath, DiscoverCursor, Discovery, Found, Stepped, discover, discover_beneath, discover_union,
};
pub use judge::{Judge, Reached, Tally, Walked};
pub(crate) use liveness::contract_scan_floor;
pub use pattern::{Pattern, RESERVED, Segment, path_matches, path_matches_except};
pub use plan::{
    Planned, READ_AHEAD, ReadAhead, TermVerdict, contract_veto, judge_one, judge_term, plan,
};
pub use reclaim::{
    Alternative, Reclaimed, reclaim, reclaim_alternatives, reclaim_alternatives_checked,
    retention_guard_veto, tally, tally_for_union,
};
pub use source_anchor::SourceEvidence;

use crate::profile::{EntryType, Finds, Source, Where};

pub use crate::engine::{PRIVATE_TMP_PREFIX, is_private_tmp, path_in_private_tmp};

/// What a kind `fs` source profile finds (docs/reference.md, Profiles),
/// compiled for one user (`{uid}` replaced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Find {
    pub patterns: Vec<Pattern>,
    pub exclude: Vec<Pattern>,
    pub entry_type: EntryType,
    /// The source's own `where`: a file the candidate must contain
    /// (directly in it, as a regular file) ...
    pub has: Option<String>,
    /// A direct child that must be a directory.
    pub has_dir: Option<String>,
    /// ... and one it must not.
    pub has_not: Option<String>,
    /// Selection filters are judged after discovery and again under the
    /// candidate lock. They must not narrow the source's discovered set.
    pub entry_filters: Vec<EntryFilter>,
    /// Each negated mixed filter is one conjunction. Its complement is
    /// evaluated after the walk supplies the newest-change measure.
    pub negated_mixed_filters: Vec<NegatedMixedFilter>,
    pub app_not_running: Option<std::path::PathBuf>,
    /// An entry of any type at this name must be absent.
    pub lacks_entry: Option<String>,
    pub source_anchor: Option<crate::profile::SourceAnchor>,
    /// The producer liveness contract its candidates must pass
    /// ([`liveness`], judged from the in-use check's scan).
    pub liveness_contract: Option<crate::profile::LivenessContract>,
    pub keep_newest: Option<usize>,
    pub newest_group: Option<crate::profile::NewestGroup>,
    pub protect_links: Vec<String>,
    pub protect_link_dirs: Vec<String>,
    pub protect_parent_link: Option<String>,
    pub protect_cursor_extensions: Option<String>,
    /// Keep every toolchain this manager could select ([`toolchain`]).
    pub protect_toolchains: Option<crate::profile::ToolchainManager>,
    /// The source's bound roots in the term that selects it (set by the
    /// tier; empty: only the root a candidate was found under). The
    /// toolchain guard walks every one for pin files.
    pub bound_roots: Vec<std::path::PathBuf>,
}

/// One filesystem entry predicate in a selection term. `has` means a
/// regular direct child; `has_not` means no regular direct child of that
/// name. Negation complements only a known verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryFilter {
    pub name: String,
    pub entry: String,
    pub has: bool,
    pub negated: bool,
}

/// A single `!filter:` with entry and idle predicates. Unknown evidence
/// admits neither this conjunction nor its complement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegatedMixedFilter {
    pub name: String,
    pub has: Option<String>,
    pub has_not: Option<String>,
    pub idle_for: Option<crate::profile::IdleFor>,
}

impl Find {
    /// Positive version and current-installation rules protect an inode
    /// independently of the evidence that admits a union term.
    pub fn has_retention_guard(&self) -> bool {
        self.keep_newest.is_some()
            || !self.protect_links.is_empty()
            || !self.protect_link_dirs.is_empty()
            || self.protect_parent_link.is_some()
            || self.protect_cursor_extensions.is_some()
            || self.protect_toolchains.is_some()
    }

    /// Conjoin a positive version filter. More retention wins when two
    /// `keep_newest` rules occur; conflicting grouping or parent-link
    /// scopes are errors rather than silently changing either rule.
    pub fn apply_version_filter(&mut self, where_: &Where) -> Result<(), String> {
        if let Some(count) = where_.keep_newest {
            let group = where_
                .newest_group
                .unwrap_or(crate::profile::NewestGroup::Siblings);
            if self.keep_newest.is_some()
                && self
                    .newest_group
                    .unwrap_or(crate::profile::NewestGroup::Siblings)
                    != group
            {
                return Err("keep_newest filters use conflicting newest_group values".to_owned());
            }
            self.keep_newest = Some(self.keep_newest.map_or(count, |old| old.max(count)));
            self.newest_group = Some(group);
        }
        if let Some(link) = &where_.protect_parent_link {
            if self
                .protect_parent_link
                .as_ref()
                .is_some_and(|old| old != link)
            {
                return Err("protect_parent_link filters name different links".to_owned());
            }
            self.protect_parent_link = Some(link.clone());
        }
        for link in &where_.protect_links {
            if !self.protect_links.contains(link) {
                self.protect_links.push(link.clone());
            }
        }
        for dir in &where_.protect_link_dirs {
            if !self.protect_link_dirs.contains(dir) {
                self.protect_link_dirs.push(dir.clone());
            }
        }
        if let Some(manifest) = &where_.protect_cursor_extensions {
            if self
                .protect_cursor_extensions
                .as_ref()
                .is_some_and(|old| old != manifest)
            {
                return Err("protect_cursor_extensions filters name different manifests".to_owned());
            }
            self.protect_cursor_extensions = Some(manifest.clone());
        }
        if let Some(manager) = where_.protect_toolchains {
            if self.protect_toolchains.is_some_and(|old| old != manager) {
                return Err("protect_toolchains filters name different managers".to_owned());
            }
            self.protect_toolchains = Some(manager);
        }
        Ok(())
    }

    /// The `find` of `source` for the user `uid`; `None` when the source
    /// is not of kind `fs`.
    pub fn of(source: &Source, uid: u32) -> Option<Self> {
        match &source.finds {
            Finds::Paths {
                patterns,
                exclude,
                entry_type,
            } => Some(Self::with_excludes(
                patterns,
                exclude,
                *entry_type,
                &source.where_,
                uid,
            )),
            Finds::CargoTargets => None,
            Finds::DockerBuildCache
            | Finds::DockerImages
            | Finds::PodmanImages
            | Finds::NixStore => None,
        }
    }

    /// The `find` of a kind `fs` source with these patterns, entry type and
    /// own `where`, for the user `uid`.
    pub fn new(patterns: &[String], entry_type: EntryType, where_: &Where, uid: u32) -> Self {
        Self::with_excludes(patterns, &[], entry_type, where_, uid)
    }

    /// As [`Self::new`], with paths that must never become candidates.
    pub fn with_excludes(
        patterns: &[String],
        exclude: &[String],
        entry_type: EntryType,
        where_: &Where,
        uid: u32,
    ) -> Self {
        Self {
            patterns: patterns.iter().map(|p| Pattern::compile(p, uid)).collect(),
            exclude: exclude.iter().map(|p| Pattern::compile(p, uid)).collect(),
            entry_type,
            has: where_.has.clone(),
            has_dir: where_.has_dir.clone(),
            has_not: where_.has_not.clone(),
            entry_filters: Vec::new(),
            negated_mixed_filters: Vec::new(),
            app_not_running: where_
                .app_not_running
                .as_ref()
                .map(|path| std::path::PathBuf::from(path.replace("{uid}", &uid.to_string()))),
            lacks_entry: where_.lacks_entry.clone(),
            source_anchor: where_.source_anchor,
            liveness_contract: where_.liveness_contract,
            keep_newest: where_.keep_newest,
            newest_group: where_.newest_group,
            protect_links: where_.protect_links.clone(),
            protect_link_dirs: where_.protect_link_dirs.clone(),
            protect_parent_link: where_.protect_parent_link.clone(),
            protect_cursor_extensions: where_.protect_cursor_extensions.clone(),
            protect_toolchains: where_.protect_toolchains,
            bound_roots: Vec::new(),
        }
    }
}
