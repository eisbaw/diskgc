//! The profile schema as YAML spells it (docs/reference.md, Profiles): what
//! serde reads, with unknown keys rejected everywhere. Values that have one
//! obvious type (a kind, a duration, `roots: required` or a list) are
//! checked here, so their errors carry the YAML line and column; the rules
//! that relate fields to each other are in [`super::load`].
//!
//! The configuration file (task-15) embeds a list of [`ProfileSpec`] under
//! `profiles:`.
//!
//! Keys that are Rust keywords (`where`, `type`, `match`) are raw
//! identifiers (`r#where`); serde names the field without the `r#`.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::engine::Checkouts;
use crate::kind::Kind;
use crate::units::parse_duration;

/// One profile as written. Exactly one of `find` (a source profile) or
/// `where` alone (a filter profile) makes sense; [`super::load`] says which
/// combinations are allowed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSpec {
    pub name: String,
    /// Explicit permission to replace a built-in with this name.
    #[serde(default)]
    pub r#override: Option<bool>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub kind: Option<KindSpec>,
    #[serde(default)]
    pub find: Option<FindSpec>,
    #[serde(default)]
    pub r#where: Option<WhereSpec>,
    /// Mandatory minimum age for every candidate of this source. A tier
    /// may demand longer through an `idle_for` filter, never shorter.
    #[serde(default, deserialize_with = "present_min_idle_for")]
    pub min_idle_for: Option<DurationSpec>,
    #[serde(default)]
    pub action: Option<ActionSpec>,
    #[serde(default)]
    pub examples: Option<ExamplesSpec>,
    /// `checkouts: refuse | allow`; absent means `refuse` (docs/safety.md,
    /// No source checkout). Any other value is a parse error, a present
    /// null (`checkouts:`, `~`, `null`) included: only absence means the
    /// default.
    #[serde(default, deserialize_with = "present_checkouts")]
    pub checkouts: Option<CheckoutsSpec>,
    #[serde(default)]
    pub verified_with: Option<String>,
}

/// `find:`
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindSpec {
    pub roots: RootsSpec,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Paths relative to each root which this source never finds.
    #[serde(default, deserialize_with = "present_exclude")]
    pub exclude: Option<Vec<String>>,
    #[serde(default)]
    pub r#type: Option<EntryType>,
}

/// `roots: [/tmp, /var/tmp]` or `roots: required`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootsSpec {
    Fixed(Vec<String>),
    Required,
}

/// `type:` of a `find`: what a found entry must be (docs/reference.md,
/// Profiles). Symlinks, sockets, FIFOs and devices are never candidates of
/// any type: a symlink candidate could only end as `unsafe_path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    /// A directory.
    Dir,
    /// A regular file.
    File,
    /// A directory or a regular file.
    Any,
}

impl EntryType {
    /// As the YAML (and the JSON of `profiles`) spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::File => "file",
            Self::Any => "any",
        }
    }
}

/// `where:` (docs/reference.md, Predicates). Each predicate at most once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhereSpec {
    #[serde(default)]
    pub idle_for: Option<DurationSpec>,
    #[serde(default)]
    pub has: Option<String>,
    #[serde(default)]
    pub has_not: Option<String>,
    /// A Chromium-family `SingletonLock` entry, absolute or relative to
    /// each source root. Any present entry blocks the candidate.
    #[serde(default, deserialize_with = "present_non_null_string")]
    pub app_not_running: Option<String>,
    /// A source safety guard: a written null must not disable it.
    #[serde(default, deserialize_with = "present_non_null_string")]
    pub lacks_entry: Option<String>,
    #[serde(default, deserialize_with = "present_non_null_usize")]
    pub keep_newest: Option<usize>,
    #[serde(default, deserialize_with = "present_non_null_newest_group")]
    pub newest_group: Option<NewestGroup>,
    #[serde(default)]
    pub protect_links: Vec<String>,
    #[serde(default)]
    pub protect_link_dirs: Vec<String>,
    #[serde(default, deserialize_with = "present_non_null_string")]
    pub protect_parent_link: Option<String>,
    #[serde(default, deserialize_with = "present_non_null_string")]
    pub protect_cursor_extensions: Option<String>,
}

/// How `keep_newest` partitions names in one candidate parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewestGroup {
    Siblings,
    CursorExtension,
}

impl NewestGroup {
    pub fn name(self) -> &'static str {
        match self {
            Self::Siblings => "siblings",
            Self::CursorExtension => "cursor_extension",
        }
    }
}

/// `examples:`
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExamplesSpec {
    /// An illustrative supplied root for profiles with `roots: required`.
    #[serde(default, deserialize_with = "present_non_null_string")]
    pub root: Option<String>,
    #[serde(default)]
    pub r#match: Vec<String>,
    #[serde(default)]
    pub no_match: Vec<String>,
}

/// `action:` (docs/reference.md, Actions):
///
/// - `delete`
/// - `{delete: {make_writable: true}}`
/// - `{command: [argv...], cwd: candidate | parent}`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionSpec {
    Delete { make_writable: bool },
    Command { argv: Vec<String>, cwd: Option<Cwd> },
}

/// Where a command action runs (docs/reference.md, Actions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cwd {
    /// The checked candidate itself (a directory).
    Candidate,
    /// The candidate's parent directory.
    Parent,
}

impl Cwd {
    pub fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Parent => "parent",
        }
    }
}

/// A [`Checkouts`] setting by its YAML name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckoutsSpec(pub Checkouts);

/// A [`Kind`] by its YAML name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindSpec(pub Kind);

/// A duration in the syntax of [`parse_duration`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationSpec(pub Duration);

/// A written null must not silently remove a source's safety floor.
fn present_min_idle_for<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DurationSpec>, D::Error> {
    DurationSpec::deserialize(deserializer).map(Some)
}

/// A written null must not silently disable a source safety guard.
fn present_non_null_string<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

fn present_non_null_usize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<usize>, D::Error> {
    usize::deserialize(deserializer).map(Some)
}

fn present_non_null_newest_group<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<NewestGroup>, D::Error> {
    NewestGroup::deserialize(deserializer).map(Some)
}

/// A present null must not silently remove names excluded by a profile.
fn present_exclude<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    struct ExcludeList;

    impl<'de> Visitor<'de> for ExcludeList {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a non-null list of excluded paths")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut paths = Vec::new();
            while let Some(path) = seq.next_element()? {
                paths.push(path);
            }
            Ok(paths)
        }
    }

    deserializer.deserialize_any(ExcludeList).map(Some)
}

// ---- Hand-written deserialisers: a value of one of two shapes, with the
// error of the shape that was written (an untagged enum would only say
// that no variant matched).

/// Every kind's name, for the "expected one of" of an unknown kind.
const KIND_NAMES: [&str; Kind::ALL.len()] = {
    let mut names = [""; Kind::ALL.len()];
    let mut i = 0;
    while i < names.len() {
        names[i] = Kind::ALL[i].name();
        i += 1;
    }
    names
};

impl<'de> Deserialize<'de> for KindSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Kind::ALL
            .into_iter()
            .find(|kind| kind.name() == name)
            .map(KindSpec)
            .ok_or_else(|| de::Error::unknown_variant(&name, &KIND_NAMES))
    }
}

/// `checkouts:` when the key is present: its value, never `None`. Without
/// this, serde's `Option` would read a present null as absent, which would
/// silently mean `refuse` on a source and pass the "a filter has no
/// `checkouts`" rule.
fn present_checkouts<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<CheckoutsSpec>, D::Error> {
    CheckoutsSpec::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for CheckoutsSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = CheckoutsSpec;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("`refuse` or `allow`")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<CheckoutsSpec, E> {
                Checkouts::ALL
                    .into_iter()
                    .find(|c| c.as_str() == text)
                    .map(CheckoutsSpec)
                    .ok_or_else(|| E::unknown_variant(text, &Checkouts::NAMES))
            }
            fn visit_unit<E: de::Error>(self) -> Result<CheckoutsSpec, E> {
                Err(E::custom(
                    "`checkouts` has no value: write `refuse` or `allow`, or leave the key out \
                     (the default is `refuse`)",
                ))
            }
            fn visit_none<E: de::Error>(self) -> Result<CheckoutsSpec, E> {
                self.visit_unit()
            }
        }
        deserializer.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for DurationSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = DurationSpec;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration such as `3d` (an integer and one of s, m, h, d, w)")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<DurationSpec, E> {
                parse_duration(text).map(DurationSpec).map_err(E::custom)
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<DurationSpec, E> {
                Err(E::custom(format!(
                    "`{n}`: a bare number is not a duration; add a unit (s, m, h, d or w)"
                )))
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<DurationSpec, E> {
                Err(E::custom(format!("`{n}` is not a duration")))
            }
        }
        deserializer.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for RootsSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RootsSpec;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list of absolute directories, or `required`")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<RootsSpec, E> {
                if text == "required" {
                    Ok(RootsSpec::Required)
                } else {
                    Err(E::custom(format!(
                        "roots `{text}`: write a list (`roots: [{text}]`) or `required`"
                    )))
                }
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<RootsSpec, A::Error> {
                Vec::deserialize(de::value::SeqAccessDeserializer::new(seq)).map(RootsSpec::Fixed)
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// The mapping form of `action:`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionMap {
    #[serde(default)]
    delete: Option<DeleteOptions>,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    cwd: Option<Cwd>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteOptions {
    #[serde(default)]
    make_writable: bool,
}

impl<'de> Deserialize<'de> for ActionSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ActionSpec;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "`delete`, `{delete: {make_writable: true}}` or `{command: [...], cwd: ...}`",
                )
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<ActionSpec, E> {
                match text {
                    "delete" => Ok(ActionSpec::Delete {
                        make_writable: false,
                    }),
                    "command" => Err(E::custom(
                        "a command action is a mapping: `{command: [argv...]}`",
                    )),
                    other => Err(E::custom(format!(
                        "unknown action `{other}`: write `delete` or `{{command: [...]}}`"
                    ))),
                }
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<ActionSpec, A::Error> {
                let map = ActionMap::deserialize(de::value::MapAccessDeserializer::new(map))?;
                match map {
                    ActionMap {
                        delete: Some(options),
                        command: None,
                        cwd: None,
                    } => Ok(ActionSpec::Delete {
                        make_writable: options.make_writable,
                    }),
                    ActionMap {
                        delete: None,
                        command: Some(argv),
                        cwd,
                    } => Ok(ActionSpec::Command { argv, cwd }),
                    ActionMap {
                        delete: Some(_),
                        command: Some(_),
                        ..
                    } => Err(de::Error::custom(
                        "an action is either `delete` or `command`, not both",
                    )),
                    ActionMap {
                        delete: Some(_),
                        cwd: Some(_),
                        ..
                    } => Err(de::Error::custom(
                        "`cwd` belongs to command actions, not to `delete`",
                    )),
                    ActionMap { .. } => Err(de::Error::custom(
                        "an action mapping needs `delete: {...}` or `command: [...]`",
                    )),
                }
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// The profiles in `text`, a YAML list of profiles.
pub fn parse_profiles(text: &str) -> Result<Vec<ProfileSpec>, String> {
    serde_saphyr::from_str(text).map_err(|e| e.to_string())
}
