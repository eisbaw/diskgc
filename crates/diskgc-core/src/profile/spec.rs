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

use crate::kind::Kind;
use crate::units::parse_duration;

/// One profile as written. Exactly one of `find` (a source profile) or
/// `where` alone (a filter profile) makes sense; [`super::load`] says which
/// combinations are allowed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSpec {
    pub name: String,
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
    #[serde(default)]
    pub action: Option<ActionSpec>,
    #[serde(default)]
    pub examples: Option<ExamplesSpec>,
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
}

/// `examples:`
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExamplesSpec {
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

/// A [`Kind`] by its YAML name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindSpec(pub Kind);

/// A duration in the syntax of [`parse_duration`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationSpec(pub Duration);

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
