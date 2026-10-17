//! Profiles (docs/reference.md, Profiles): named rules for one kind of
//! cruft. A **source** profile finds candidates (`find`) and says how they
//! are removed (`action`, or the kind's own protocol); a **filter** profile
//! only narrows (`where`).
//!
//! - [`spec`]: the YAML as written ([`ProfileSpec`]), unknown keys rejected.
//! - [`load`]: the static rules that turn specs into a [`Library`] of
//!   [`Profile`]s, all problems reported together.
//! - [`Library`]: the built-in profiles (embedded YAML, `builtin.yaml`)
//!   plus the user's, and the [`Catalog`] the selection grammar checks
//!   names against.
//! - [`roots`]: binding `roots: required` profiles to the roots a tier,
//!   `--roots` or a `protect` entry supplies.
//! - [`idle`]: the `idle_for` predicate, how each kind measures it, and how
//!   several combine in one term.
//!
//! Everything here is pure: parsing a string and checking data. Nothing
//! reads the machine; the built-ins are compiled in.

mod idle;
mod load;
mod roots;
mod spec;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;

use crate::kind::{Kind, PredicateName};
use crate::select::{Catalog, Entry, Term};

pub use idle::{IdleFor, IdleMeasure, Idleness, Unknown};
pub use load::{LoadError, LoadErrors};
pub use roots::RootsError;
pub use spec::{
    ActionSpec, Cwd, DurationSpec, EntryType, ExamplesSpec, FindSpec, KindSpec, ProfileSpec,
    RootsSpec, WhereSpec, parse_profiles,
};

/// The built-in profile library, embedded in the binary.
const BUILTIN_YAML: &str = include_str!("builtin.yaml");

/// A profile that passed the static rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub category: Option<String>,
    pub description: Option<String>,
    pub origin: Origin,
    pub body: Body,
}

/// Where a profile comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    BuiltIn,
    /// The configuration file's `profiles:`.
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Source(Source),
    Filter(Where),
}

/// A profile with `find`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub roots: Roots,
    pub finds: Finds,
    /// The profile's own predicates: `has` / `has_not` only (when a source
    /// is old enough is the tier's choice, through `filter:`s). A term's
    /// filters narrow further; `!profile:` and `protect` ignore them
    /// (docs/reference.md, Selection grammar and `protect`).
    pub where_: Where,
    pub action: Action,
    pub examples: Examples,
    /// Built-in profiles only: the tool version it was checked against.
    pub verified_with: Option<String>,
}

impl Source {
    /// The kind follows from what the profile finds.
    pub fn kind(&self) -> Kind {
        match self.finds {
            Finds::Paths { .. } => Kind::Fs,
            Finds::CargoTargets => Kind::CargoTarget,
        }
    }
}

/// What a source profile finds below its roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finds {
    /// Kind `fs`: entries matching any of the patterns (relative to a root,
    /// one directory level per segment, no `**`), of this type. Matching is
    /// the selection engine's (task-16).
    Paths {
        patterns: Vec<String>,
        entry_type: EntryType,
    },
    /// Kind `cargo-target`: Cargo target directories, recognised by their
    /// structure (docs/safety.md, cargo-target: Discovery).
    CargoTargets,
}

/// Where a source profile looks (docs/reference.md, Profiles).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Roots {
    /// Absolute directories; `{uid}` is the only placeholder.
    Fixed(Vec<String>),
    /// Machine-specific: a tier's `roots:`, `--roots` or a `protect` entry
    /// supplies them ([`Library::bind_roots`]).
    Required,
}

/// A profile's predicates (docs/reference.md, Predicates). Empty for a
/// source without `where`; never empty for a filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Where {
    pub idle_for: Option<IdleFor>,
    /// A file name the candidate must contain.
    pub has: Option<String>,
    /// A file name the candidate must not contain.
    pub has_not: Option<String>,
}

impl Where {
    /// The predicates used, in the order of [`PredicateName::ALL`].
    pub fn predicates(&self) -> Vec<PredicateName> {
        let used = [
            (PredicateName::IdleFor, self.idle_for.is_some()),
            (PredicateName::Has, self.has.is_some()),
            (PredicateName::HasNot, self.has_not.is_some()),
        ];
        used.into_iter()
            .filter_map(|(name, used)| used.then_some(name))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.predicates().is_empty()
    }
}

/// Paths that must and must not match a source's patterns. They check that
/// the patterns do what their author meant (`diskgc profiles test`,
/// task-17), not that the matches are disposable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Examples {
    pub matches: Vec<String>,
    pub no_match: Vec<String>,
}

/// How a source profile's candidates are removed. Two sources share an
/// action exactly when these are equal: the action and every option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// diskgc's own deletion (docs/safety.md, Deletion).
    Delete { make_writable: bool },
    /// An argv run per candidate (docs/safety.md, Command actions).
    Command {
        argv: Vec<String>,
        /// `None` for a profile that finds any type of entry: the
        /// candidate for a directory, its parent otherwise. Profiles of
        /// one type have it resolved at load.
        cwd: Option<Cwd>,
    },
    /// Kind `cargo-target`'s lock protocol (docs/safety.md, Reclaim
    /// protocol), fixed by the kind.
    CargoLockProtocol,
}

/// What the "one action" error shows: enough to see what differs, every
/// option included. Command arguments are quoted, so a space or an empty
/// argument is visible.
impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Delete {
                make_writable: false,
            } => f.write_str("delete"),
            Self::Delete {
                make_writable: true,
            } => f.write_str("delete {make_writable: true}"),
            Self::Command { argv, cwd } => {
                f.write_str("command [")?;
                for (i, arg) in argv.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{arg:?}")?;
                }
                f.write_str("]")?;
                match cwd {
                    Some(cwd) => write!(f, ", cwd: {}", cwd.name()),
                    None => Ok(()),
                }
            }
            Self::CargoLockProtocol => f.write_str("cargo lock protocol"),
        }
    }
}

/// The built-in profiles plus the user's, by name. Built only by the
/// loader, so every profile in it passed the static rules.
#[derive(Debug, Clone)]
pub struct Library {
    profiles: BTreeMap<String, Profile>,
}

impl Library {
    /// The built-in profiles alone (`--select` without a config file). An
    /// error here is a bug in the shipped YAML: the tests load it, so a
    /// broken built-in fails the build's checks first.
    pub fn builtin() -> Result<Self, LoadErrors> {
        Self::with_user_profiles(&[])
    }

    /// The built-in profiles and the configuration file's `profiles:`. A
    /// user profile may not take a built-in's name (overrides: task-17).
    pub fn with_user_profiles(user: &[ProfileSpec]) -> Result<Self, LoadErrors> {
        Self::load(BUILTIN_YAML, user)
    }

    /// [`Self::with_user_profiles`] over other built-in YAML (tests).
    pub(crate) fn load(builtin_yaml: &str, user: &[ProfileSpec]) -> Result<Self, LoadErrors> {
        load::library(builtin_yaml, user).map(|profiles| Self { profiles })
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// Every profile, by name.
    pub fn profiles(&self) -> impl Iterator<Item = &Profile> {
        self.profiles.values()
    }

    /// The source profile `name`.
    fn source(&self, name: &str) -> Option<&Source> {
        match &self.get(name)?.body {
            Body::Source(source) => Some(source),
            Body::Filter(_) => None,
        }
    }

    /// The longest `idle_for` that `term`'s `filter:` factors require
    /// (docs/reference.md, Predicates: two `idle_for` in one term combine
    /// to the longest). `!filter:` factors are complements and do not
    /// combine; a source has no `idle_for` of its own. `None`: no idle
    /// requirement.
    ///
    /// The term must come from an expression checked against this library
    /// ([`crate::select::compile`]); a name it does not know is an error.
    pub fn idle_for(&self, term: &Term) -> Result<Option<IdleFor>, String> {
        idle::longest(self, term)
    }
}

impl Catalog for Library {
    type Action = Action;

    fn lookup(&self, name: &str) -> Option<Entry<'_, Action>> {
        Some(match &self.get(name)?.body {
            Body::Source(source) => Entry::Source {
                kind: source.kind(),
                action: &source.action,
            },
            Body::Filter(where_) => Entry::Filter {
                predicates: where_.predicates(),
            },
        })
    }
}
