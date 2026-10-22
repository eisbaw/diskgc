//! Pure membership rules for a checked selection. The host supplies facts
//! from discovery and predicate checks; this module decides which paths a
//! tier may consider. The same rules apply in planning and before deletion.

use std::path::{Path, PathBuf};

use super::{Checked, Factor};

/// A filter's verdict for one candidate. An unavailable measure is neither
/// a match nor its complement (notably for an unknown or future idle time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterVerdict {
    Matches,
    DoesNotMatch,
    Unknown,
}

/// A configured protection with its own bound roots. These roots are
/// independent of a tier's roots, including when the same profile also
/// appears in `!profile:` in one of the tier's terms. Empty roots mean the
/// profile defines fixed roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protection {
    pub profile: String,
    pub roots: Vec<PathBuf>,
}

impl From<&str> for Protection {
    fn from(profile: &str) -> Self {
        Self {
            profile: profile.to_owned(),
            roots: Vec::new(),
        }
    }
}

/// Facts observed for one tier. `source_matches` includes a source's own
/// evidence `where` and mandatory `min_idle_for`, but excludes positive
/// retention guards. `source_discovered` is raw structural coverage, before
/// source-owned or term evidence is checked; every guard on that set must
/// pass, even if the term's evidence fails. `profile_overlaps` deliberately
/// ignores those source conditions, and must
/// include a match equal to, above, or below `candidate`. Paths must be
/// absolute and normalised. If discovery of a protected or negated profile
/// is incomplete, the caller must refuse the affected tier rather than
/// report `false` here.
pub trait Facts {
    type Error;

    fn source_matches(&self, profile: &str, candidate: &Path) -> Result<bool, Self::Error>;
    /// Sources whose root, path pattern and type cover this candidate before
    /// source-owned evidence is checked. A reclaim caller must preserve this set.
    fn source_discovered(&self, profile: &str, candidate: &Path) -> Result<bool, Self::Error>;
    /// Positive source and filter retention rules only. A name with no
    /// guard returns `Matches`; protected and unknown states both veto.
    fn retention_guard_verdict(
        &self,
        name: &str,
        candidate: &Path,
    ) -> Result<FilterVerdict, Self::Error>;
    fn filter_verdict(&self, filter: &str, candidate: &Path) -> Result<FilterVerdict, Self::Error>;
    /// Raw matches of a negated profile, bound to this tier's roots.
    fn profile_overlaps(&self, profile: &str, candidate: &Path) -> Result<bool, Self::Error>;
    /// Raw matches of a protection, bound to its own roots. An incomplete
    /// lookup is an error; it must not be treated as `false`.
    fn protection_overlaps(
        &self,
        protection: &Protection,
        candidate: &Path,
    ) -> Result<bool, Self::Error>;
}

/// Whether `candidate` is selected by any term, after `protect`. The
/// positive source is checked first: factors can only remove candidates.
/// Callers must use fresh facts at deletion time; a plan is not authority.
pub fn eligible<F: Facts>(
    selection: &Checked,
    candidate: &Path,
    protect: &[Protection],
    facts: &F,
) -> Result<bool, F::Error> {
    for protection in protect {
        if facts.protection_overlaps(protection, candidate)? {
            return Ok(false);
        }
    }

    // Guard veto precedes the union's any-term admission. A later term must
    // never revive an inode retained by an earlier, now filtered, source.
    for term in &selection.expr().terms {
        if !facts.source_discovered(&term.source, candidate)? {
            continue;
        }
        if facts.retention_guard_verdict(&term.source, candidate)? != FilterVerdict::Matches {
            return Ok(false);
        }
        for factor in &term.factors {
            if let Factor::Filter(name) = factor
                && facts.retention_guard_verdict(name, candidate)? != FilterVerdict::Matches
            {
                return Ok(false);
            }
        }
    }

    for term in &selection.expr().terms {
        if !facts.source_matches(&term.source, candidate)? {
            continue;
        }
        let mut accepted = true;
        for factor in &term.factors {
            let passes = match factor {
                Factor::Filter(name) => {
                    facts.filter_verdict(name, candidate)? == FilterVerdict::Matches
                }
                Factor::NotFilter(name) => {
                    facts.filter_verdict(name, candidate)? == FilterVerdict::DoesNotMatch
                }
                Factor::NotProfile(name) => !facts.profile_overlaps(name, candidate)?,
            };
            if !passes {
                accepted = false;
                break;
            }
        }
        if accepted {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Component-aware overlap: an equal path, ancestor or descendant. A
/// character prefix (`/tmp/a` and `/tmp/ab`) is not an overlap. Both inputs
/// must be absolute, normalised paths, as discovery and root binding produce.
pub fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

#[cfg(test)]
mod tests;
