//! Profile kinds and predicate names (docs/reference.md, Profiles and
//! Predicates): the shared vocabulary of the selection grammar's checks
//! and the profile model. A leaf module: it depends on nothing in the crate.

use std::fmt;

/// A profile kind (docs/reference.md, Profiles: `kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Fs,
    CargoTarget,
    DockerBuildCache,
    DockerImages,
    NixStore,
}

impl Kind {
    pub const ALL: [Self; 5] = [
        Self::Fs,
        Self::CargoTarget,
        Self::DockerBuildCache,
        Self::DockerImages,
        Self::NixStore,
    ];

    /// The name used in YAML and messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Fs => "fs",
            Self::CargoTarget => "cargo-target",
            Self::DockerBuildCache => "docker-build-cache",
            Self::DockerImages => "docker-images",
            Self::NixStore => "nix-store",
        }
    }

    /// Whether the kind enumerates its candidates one by one (`fs`,
    /// `cargo-target`). The others act as a whole (a prune, a GC): they
    /// take one term, at most an idle filter, and cannot be negated.
    pub fn enumerates(self) -> bool {
        matches!(self, Self::Fs | Self::CargoTarget)
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The name of a predicate a filter profile's `where` uses
/// (docs/reference.md, Predicates), without its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PredicateName {
    IdleFor,
    Has,
    HasNot,
}

impl PredicateName {
    pub const ALL: [Self; 3] = [Self::IdleFor, Self::Has, Self::HasNot];

    /// The name used in YAML and messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::IdleFor => "idle_for",
            Self::Has => "has",
            Self::HasNot => "has_not",
        }
    }

    /// The kinds that support this predicate (the Predicates table).
    pub fn kinds(self) -> &'static [Kind] {
        match self {
            Self::IdleFor => &[Kind::Fs, Kind::CargoTarget, Kind::DockerBuildCache],
            Self::Has | Self::HasNot => &[Kind::Fs],
        }
    }

    pub fn supported_by(self, kind: Kind) -> bool {
        self.kinds().contains(&kind)
    }
}

impl fmt::Display for PredicateName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
