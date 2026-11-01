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
//! - [`describe`]: what `diskgc --help` and `diskgc profiles list | show`
//!   print about a library, as text and JSON.
//!
//! Everything here is pure: parsing a string and checking data. Nothing
//! reads the machine; the built-ins are compiled in.

pub mod describe;
pub mod examples;
mod idle;
mod load;
mod roots;
mod spec;
mod temp_roots;

#[cfg(test)]
mod golden;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::engine::Checkouts;
use crate::kind::{Kind, PredicateName};
use crate::select::{Catalog, Entry, Term};

pub use idle::{IdleFor, IdleMeasure, IdleWindow, Idleness, Unknown, age_secs, future_secs};
pub use load::{LoadError, LoadErrors};
pub use roots::RootsError;
pub use spec::{
    ActionSpec, CheckoutsSpec, Cwd, DurationSpec, EntryType, ExamplesSpec, FindSpec, KindSpec,
    LivenessClass, LivenessContract, NewestGroup, ProfileSpec, Release, RootsSpec, SourceAnchor,
    WhereSpec, parse_profiles,
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
    /// Built-ins only: the release catalogs that include it (`releases:`);
    /// empty for the development library alone and for user profiles.
    pub releases: Vec<Release>,
}

impl Profile {
    /// `source` (it has `find`: selected as `profile:NAME`) or `filter`
    /// (only `where`: `filter:NAME`), as `profiles list` and its JSON spell
    /// it.
    pub fn role(&self) -> &'static str {
        match self.body {
            Body::Source(_) => "source",
            Body::Filter(_) => "filter",
        }
    }
}

/// Where a profile comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    BuiltIn,
    /// The configuration file's `profiles:`.
    User,
    /// A user profile that explicitly replaces a built-in.
    UserOverride,
}

impl Origin {
    /// As `profiles list` and its JSON spell it (snake_case, like the
    /// report's enums: a JSON-only value).
    pub fn name(self) -> &'static str {
        match self {
            Self::BuiltIn => "built_in",
            Self::User => "user",
            Self::UserOverride => "user_override",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Source(Box<Source>),
    Filter(Box<Where>),
}

/// A profile with `find`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub roots: Roots,
    pub finds: Finds,
    /// The profile's own predicates: `has`, `has_dir`, `has_not`,
    /// `app_not_running`, and `lacks_entry`. The source can set `min_idle_for`; a tier can require
    /// longer with `filter:`s. A term's
    /// filters narrow further; `!profile:` and `protect` ignore them
    /// (docs/reference.md, Selection grammar and `protect`).
    pub where_: Where,
    /// Minimum age this source always requires, even when selected alone.
    /// A term's positive idle filters may only raise this floor.
    pub min_idle_for: Option<IdleFor>,
    pub action: Action,
    /// Whether its candidates may hold a VCS checkout (`checkouts:`;
    /// [`Checkouts::Refuse`] when the profile does not say). Applies to the
    /// walk and the path rule of every candidate it finds, whatever its
    /// kind and action (docs/safety.md, No source checkout).
    pub checkouts: Checkouts,
    pub examples: Examples,
    /// Built-in profiles only: the tool version it was checked against.
    pub verified_with: Option<String>,
    /// Built-in profiles only (required there): the liveness contract
    /// class (docs/safety.md, Producer liveness).
    pub liveness_class: Option<LivenessClass>,
}

impl Source {
    /// The kind follows from what the profile finds.
    pub fn kind(&self) -> Kind {
        match self.finds {
            Finds::Paths { .. } => Kind::Fs,
            Finds::CargoTargets => Kind::CargoTarget,
            Finds::DockerBuildCache => Kind::DockerBuildCache,
            Finds::DockerImages => Kind::DockerImages,
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
        exclude: Vec<String>,
        entry_type: EntryType,
    },
    /// Kind `cargo-target`: Cargo target directories, recognised by their
    /// structure (docs/safety.md, cargo-target: Discovery).
    CargoTargets,
    /// The bound Docker daemon's default builder cache, one whole unit.
    DockerBuildCache,
    /// The bound Docker daemon's image store, one whole unit.
    DockerImages,
}

/// Where a source profile looks (docs/reference.md, Profiles).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Roots {
    /// Absolute directories; `{uid}` is the only placeholder.
    Fixed(Vec<String>),
    /// Machine-specific: a tier's `roots:`, `--roots` or a `protect` entry
    /// supplies them ([`Library::bind_roots`]).
    Required,
    /// The storage location comes from a checked backend query.
    Backend,
}

/// A profile's predicates (docs/reference.md, Predicates). Empty for a
/// source without `where`; never empty for a filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Where {
    pub idle_for: Option<IdleFor>,
    /// A file name the candidate must contain.
    pub has: Option<String>,
    /// A direct child that must be a directory, without following links.
    pub has_dir: Option<String>,
    /// A file name the candidate must not contain.
    pub has_not: Option<String>,
    /// A Chromium-family `SingletonLock` that must be absent.
    pub app_not_running: Option<String>,
    /// An entry name of any type that the candidate must not contain.
    pub lacks_entry: Option<String>,
    /// A build system's recorded source path and source file.
    pub source_anchor: Option<SourceAnchor>,
    /// The producer liveness contract the candidate must pass.
    pub liveness_contract: Option<LivenessContract>,
    /// Keep the N newest version directories in each parent/group.
    pub keep_newest: Option<usize>,
    pub newest_group: Option<spec::NewestGroup>,
    /// Symlinks relative to the bound root whose targets must be retained.
    pub protect_links: Vec<String>,
    /// Every symlink in these directories may name an installed version.
    pub protect_link_dirs: Vec<String>,
    /// A current symlink beside each candidate's parent directory.
    pub protect_parent_link: Option<String>,
    /// Cursor's installed-extension registry, relative to the bound root.
    pub protect_cursor_extensions: Option<String>,
}

impl Where {
    /// The predicates used, in the order of [`PredicateName::ALL`].
    pub fn predicates(&self) -> Vec<PredicateName> {
        let used = [
            (PredicateName::IdleFor, self.idle_for.is_some()),
            (PredicateName::Has, self.has.is_some()),
            (PredicateName::HasDir, self.has_dir.is_some()),
            (PredicateName::HasNot, self.has_not.is_some()),
            (PredicateName::AppNotRunning, self.app_not_running.is_some()),
            (PredicateName::LacksEntry, self.lacks_entry.is_some()),
            (PredicateName::SourceAnchor, self.source_anchor.is_some()),
            (
                PredicateName::LivenessContract,
                self.liveness_contract.is_some(),
            ),
            (PredicateName::KeepNewest, self.keep_newest.is_some()),
            (
                PredicateName::ProtectLinks,
                !self.protect_links.is_empty()
                    || !self.protect_link_dirs.is_empty()
                    || self.protect_parent_link.is_some(),
            ),
            (
                PredicateName::ProtectCursorExtensions,
                self.protect_cursor_extensions.is_some(),
            ),
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
    pub root: Option<String>,
    pub matches: Vec<String>,
    pub no_match: Vec<String>,
}

/// How a source profile's candidates are removed. Two sources share an
/// action exactly when these are equal: the action and every option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// diskgc's own deletion (docs/safety.md, Deletion).
    Delete {
        make_writable: bool,
    },
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
    /// Fixed Docker prune modes, not user-supplied argv.
    DockerBuilderPrune,
    DockerImagePrune {
        all: bool,
    },
}

impl Action {
    /// Root mode can only apply actions whose candidate tree is checked
    /// against the configured owner IDs. Docker prune operates on an Engine
    /// cache shared by users, so `owners:` cannot bound its effect.
    pub fn root_mode_error(&self) -> Option<&'static str> {
        match self {
            Self::Command { .. } => Some("command actions are refused under --allow-root"),
            Self::DockerBuilderPrune | Self::DockerImagePrune { .. } => Some(
                "Docker prune actions are refused under --allow-root: owners: cannot bound the daemon's shared cache",
            ),
            Self::Delete { .. } | Self::CargoLockProtocol => None,
        }
    }
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
            Self::DockerBuilderPrune => f.write_str("docker builder prune --all"),
            Self::DockerImagePrune { all: false } => f.write_str("docker image prune"),
            Self::DockerImagePrune { all: true } => f.write_str("docker image prune --all"),
        }
    }
}

/// The built-in profiles plus the user's, by name. Built only by the
/// loader, so every profile in it passed the static rules.
#[derive(Debug, Clone)]
pub struct Library {
    profiles: BTreeMap<String, Profile>,
    /// The release catalog this library was narrowed to and the built-ins
    /// it left out (empty for the development library), so a refusal of
    /// one says "not in this release" instead of "no such profile"
    /// (TASK-146): the name is documented and ships in development builds.
    withheld: Option<(String, BTreeSet<String>)>,
}

impl Library {
    /// The built-in profiles alone (`--select` without a config file). An
    /// error here is a bug in the shipped YAML: the tests load it, so a
    /// broken built-in fails the build's checks first.
    pub fn builtin() -> Result<Self, LoadErrors> {
        Self::with_user_profiles(&[])
    }

    /// The built-in profiles and the configuration file's `profiles:`. A
    /// A user profile takes a built-in's name only with `override: true`.
    pub fn with_user_profiles(user: &[ProfileSpec]) -> Result<Self, LoadErrors> {
        Self::load(BUILTIN_YAML, user)
    }

    /// Release `release`'s catalog: the built-ins whose `releases:` lists
    /// `catalog` (builtin.yaml is the one place a catalog's members are
    /// decided; TASK-65 signs off its `/tmp` part). Which label a release
    /// uses is the release descriptor's (`crate::release`); a release
    /// package must opt into it explicitly (`invocation::Catalog`), the
    /// development library still exposes the full catalog.
    ///
    /// Reject all operator-authored profiles, including overrides of an
    /// allowed name. A name-only allowlist would otherwise admit a command
    /// action or an arbitrary filesystem delete under `cargo-target`.
    ///
    /// The release scope (PRD.md, Scope: 0.1's, which no later release has
    /// widened yet) is checked against the data rather than trusted: a
    /// released source is of kind `fs` with the `delete` action or
    /// `cargo-target` with its lock protocol, and of an admissible class
    /// (the loader already refuses `releases` on class none). A violation
    /// is a bug in builtin.yaml, which the tests load.
    pub fn release_catalog(self, release: &str, catalog: Release) -> Result<Self, String> {
        let user = self.user_profile_names();
        let library = self.release_built_ins(release, catalog)?;
        if !user.is_empty() {
            // Overrides too: a name-only allowlist would admit a command
            // action or an arbitrary delete under `cargo-target`.
            let quoted: Vec<String> = user.iter().map(|name| format!("`{name}`")).collect();
            return Err(format!(
                "user profiles are not available in diskgc {release}: the configuration's \
                 `profiles:` defines {} (an `override: true` of a built-in counts too); \
                 remove them. {}",
                quoted.join(", "),
                library.release_scope()
            ));
        }
        Ok(library)
    }

    /// The user profiles' names (overrides of a built-in included), by name.
    pub fn user_profile_names(&self) -> Vec<String> {
        self.profiles
            .values()
            .filter(|p| p.origin != Origin::BuiltIn)
            .map(|p| p.name.clone())
            .collect()
    }

    /// [`Self::release_catalog`] without refusing user profiles: the
    /// catalog's built-ins alone, every user profile (and override)
    /// dropped. For a caller that reports the user profiles itself,
    /// together with the other parts the release refuses (TASK-164),
    /// instead of one refusal per run.
    pub fn release_built_ins(mut self, release: &str, catalog: Release) -> Result<Self, String> {
        let mut withheld = BTreeSet::new();
        self.profiles.retain(|name, profile| {
            let built_in = profile.origin == Origin::BuiltIn;
            if built_in && !profile.releases.contains(&catalog) {
                withheld.insert(name.clone());
            }
            built_in && profile.releases.contains(&catalog)
        });
        self.withheld = Some((release.to_owned(), withheld));
        for profile in self.profiles.values() {
            let in_scope = match &profile.body {
                Body::Filter(_) => true,
                Body::Source(source) => {
                    let shape = match source.kind() {
                        Kind::Fs => matches!(source.action, Action::Delete { .. }),
                        Kind::CargoTarget => source.action == Action::CargoLockProtocol,
                        _ => false,
                    };
                    shape && source.liveness_class.is_some_and(LivenessClass::admissible)
                }
            };
            if !in_scope {
                return Err(format!(
                    "diskgc {release}'s catalog ({}) built-in `{}` is outside the release scope \
                     (kind, action or liveness class)",
                    catalog.name(),
                    profile.name
                ));
            }
        }
        if !self
            .profiles
            .values()
            .any(|p| matches!(&p.body, Body::Source(_)))
        {
            return Err(format!(
                "diskgc {release}'s catalog ({}) has no source",
                catalog.name()
            ));
        }
        Ok(self)
    }

    /// Why `name` is not in this library although it is a documented
    /// built-in: one the release catalog leaves out ("`X` is a built-in
    /// that diskgc 0.1 does not ship."). `None` for a name this library
    /// has, or one no built-in has, and always for the development library.
    /// [`Self::release_scope`] says what the release ships instead.
    pub fn not_shipped(&self, name: &str) -> Option<String> {
        let (release, names) = self.withheld.as_ref()?;
        names
            .contains(name)
            .then(|| format!("`{name}` is a built-in that diskgc {release} does not ship."))
    }

    /// The release whose catalog this library is (`0.2`); `None` for the
    /// development library, which has every built-in and user profiles.
    pub fn release_name(&self) -> Option<&str> {
        self.withheld.as_ref().map(|(release, _)| release.as_str())
    }

    /// What this library offers, in a user's words: its sources and
    /// filters by name, in `profiles list` order, and where to read more.
    pub fn release_scope(&self) -> String {
        let (sources, filters): (Vec<&Profile>, Vec<&Profile>) = describe::listing_order(self)
            .into_iter()
            .partition(|p| matches!(&p.body, Body::Source(_)));
        let names = |profiles: &[&Profile]| {
            profiles
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let release = self
            .withheld
            .as_ref()
            .map_or("this build", |(release, _)| release.as_str());
        format!(
            "diskgc {release} ships the sources {} and the filters {} (`diskgc profiles \
             list` describes them)",
            names(&sources),
            names(&filters)
        )
    }

    /// [`Self::with_user_profiles`] over other built-in YAML (tests).
    pub(crate) fn load(builtin_yaml: &str, user: &[ProfileSpec]) -> Result<Self, LoadErrors> {
        load::library(builtin_yaml, user).map(|profiles| Self {
            profiles,
            withheld: None,
        })
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// Change only a loaded source's checkout guard. Config validation keeps
    /// this separate from profile replacement, so built-in identity and its
    /// matcher, roots, predicates, and action remain intact.
    pub(crate) fn set_checkouts(&mut self, name: &str, checkouts: Checkouts) -> Result<(), String> {
        let profile = self
            .profiles
            .get_mut(name)
            .ok_or_else(|| format!("unknown profile `{name}`"))?;
        let Body::Source(source) = &mut profile.body else {
            return Err(format!("`{name}` is a filter, not a source profile"));
        };
        if checkouts == Checkouts::Allow && !source.kind().enumerates() {
            return Err(format!(
                "`{name}` has kind {} and cannot allow checkouts: it has no file candidates",
                source.kind()
            ));
        }
        source.checkouts = checkouts;
        Ok(())
    }

    /// The `checkouts` setting of the source profile `name`, from the same
    /// library field a tier of that profile walks with (`None`: no such
    /// source profile).
    pub fn checkouts(&self, name: &str) -> Option<Checkouts> {
        self.source(name).map(|source| source.checkouts)
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
    /// combine; a source's `min_idle_for` is an unconditional floor.
    /// `None`: no idle requirement.
    ///
    /// The term must come from an expression checked against this library
    /// ([`crate::select::compile`]); a name it does not know is an error.
    pub fn idle_for(&self, term: &Term) -> Result<Option<IdleFor>, String> {
        idle::longest(self, term)
    }
}

impl Catalog for Library {
    type Action = Action;

    fn withheld(&self, name: &str) -> Option<String> {
        self.not_shipped(name)
            .map(|why| format!("{why} {}", self.release_scope()))
    }

    fn names(&self) -> Vec<&str> {
        self.profiles()
            .map(|profile| profile.name.as_str())
            .collect()
    }

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
