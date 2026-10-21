//! Roots fixed vs required (docs/reference.md, Configuration file: Roots,
//! and `protect`). A fixed-root profile always looks where it says; a
//! `roots: required` profile looks only where a tier's `roots:`, `--roots`
//! (with `--select`) or a `protect` entry says. Both mistakes are load-time
//! errors: a required profile left without roots, and roots given where no
//! profile takes them (they would silently do nothing).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::load::absolute_path;
use super::{Library, Roots};
use crate::select::{Checked, Factor};

/// Why roots cannot be bound. The caller says where they were given (a
/// tier's name, `--roots`, a `protect` entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootsError {
    /// `roots: required` profiles, and no roots given.
    Missing { profiles: Vec<String> },
    /// Roots given, and every profile has fixed roots.
    Unused { profiles: Vec<String> },
    /// An empty list of roots.
    Empty,
    /// A root that is not a usable absolute path.
    Path(String),
    /// A name the library does not have as a source profile (the
    /// expression was checked against another catalog).
    NotASource(String),
}

impl fmt::Display for RootsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { profiles } => write!(
                f,
                "{} {} `roots: required`, and no roots were given",
                names(profiles),
                if profiles.len() == 1 { "has" } else { "have" }
            ),
            Self::Unused { profiles } => write!(
                f,
                "roots were given, but {} {} fixed roots: only `roots: required` profiles \
                 take roots",
                names(profiles),
                if profiles.len() == 1 { "has" } else { "have" }
            ),
            Self::Empty => f.write_str("the list of roots is empty"),
            Self::Path(problem) => write!(f, "root {problem}"),
            Self::NotASource(name) => write!(f, "no source profile is named `{name}`"),
        }
    }
}

impl std::error::Error for RootsError {}

fn names(profiles: &[String]) -> String {
    let quoted: Vec<String> = profiles.iter().map(|p| format!("`{p}`")).collect();
    quoted.join(", ")
}

impl Library {
    /// Where each profile of a checked selection looks: every source and
    /// every `!profile:` (a negated profile takes the tier's roots if it
    /// needs them). `supplied` is the tier's `roots:` or `--roots`; it
    /// applies to the `roots: required` profiles only, and fixed-root
    /// profiles in the same selection keep their own.
    pub fn bind_roots(
        &self,
        checked: &Checked,
        supplied: Option<&[String]>,
    ) -> Result<BTreeMap<String, Vec<String>>, RootsError> {
        let mut names: Vec<&str> = Vec::new();
        for term in &checked.expr().terms {
            names.push(&term.source);
            for factor in &term.factors {
                if let Factor::NotProfile(name) = factor {
                    names.push(name);
                }
            }
        }
        self.bind_names(&names, supplied)
    }

    /// [`Self::bind_roots`] for one profile (a `protect` entry: a bare name
    /// for a fixed-root profile, `{profile, roots}` for a required one).
    pub fn roots_of(
        &self,
        name: &str,
        supplied: Option<&[String]>,
    ) -> Result<Vec<String>, RootsError> {
        let mut bound = self.bind_names(&[name], supplied)?;
        Ok(bound.remove(name).unwrap_or_default())
    }

    fn bind_names(
        &self,
        names: &[&str],
        supplied: Option<&[String]>,
    ) -> Result<BTreeMap<String, Vec<String>>, RootsError> {
        if let Some(supplied) = supplied {
            if supplied.is_empty() {
                return Err(RootsError::Empty);
            }
            validate_root_list(supplied).map_err(RootsError::Path)?;
        }
        let mut bound = BTreeMap::new();
        let mut required = Vec::new();
        let mut fixed = Vec::new();
        for name in names {
            let source = self
                .source(name)
                .ok_or_else(|| RootsError::NotASource((*name).to_owned()))?;
            let roots = match &source.roots {
                Roots::Fixed(roots) => {
                    fixed.push((*name).to_owned());
                    roots.clone()
                }
                Roots::Required => {
                    required.push((*name).to_owned());
                    supplied.map(<[String]>::to_vec).unwrap_or_default()
                }
                Roots::Backend => {
                    fixed.push((*name).to_owned());
                    Vec::new()
                }
            };
            bound.insert((*name).to_owned(), roots);
        }
        dedup(&mut required);
        dedup(&mut fixed);
        match supplied {
            None if !required.is_empty() => Err(RootsError::Missing { profiles: required }),
            Some(_) if required.is_empty() => Err(RootsError::Unused { profiles: fixed }),
            _ => Ok(bound),
        }
    }
}

/// Validate the spelling of a root before it reaches the path resolver.
/// Refusing aliases here keeps policy meaning independent of lexical cleanup;
/// inode aliases are handled later, after safe host resolution.
pub(super) fn validate_root_list(roots: &[String]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for root in roots {
        absolute_path(root)?;
        if root != "/"
            && root
                .split('/')
                .skip(1)
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(format!(
                "`{root}` is not normalized: roots cannot contain empty, `.` or `..` components"
            ));
        }
        if root.contains('\0') {
            return Err(format!("`{root}` contains a NUL byte"));
        }
        if !seen.insert(root) {
            return Err(format!("duplicate root `{root}`"));
        }
    }
    Ok(())
}

/// In first-seen order, each once.
fn dedup(names: &mut Vec<String>) {
    let mut seen = std::collections::BTreeSet::new();
    names.retain(|name| seen.insert(name.clone()));
}
