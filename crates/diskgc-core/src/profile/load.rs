//! The static rules of the profile schema (docs/reference.md, Profiles,
//! Predicates, Actions, Built-in profile library and User profiles; the
//! "profile names", "roots fixed vs required" and "predicates supported by
//! the kind" items of docs/safety.md, Validation). Every problem of every
//! profile is reported, each with where it is.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::spec::{
    ActionSpec, Cwd, EntryType, ExamplesSpec, FindSpec, ProfileSpec, RootsSpec, WhereSpec,
    parse_profiles,
};
use super::temp_roots;
use super::{Action, Body, Examples, Finds, IdleFor, Origin, Profile, Roots, Source, Where};
use crate::engine::Checkouts;
use crate::kind::{Kind, PredicateName};
use crate::select::{Syntax, is_name};

/// One problem, and where: `built-in profile `cargo-target``,
/// `profiles[2] (`zephyr-run`)`, or the YAML as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub at: String,
    pub message: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.at, self.message)
    }
}

/// Every problem found, in order. Never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadErrors(pub Vec<LoadError>);

impl fmt::Display for LoadErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, error) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            error.fmt(f)?;
        }
        Ok(())
    }
}

impl std::error::Error for LoadErrors {}

/// The built-ins parsed from `builtin_yaml`, then `user`, checked and keyed
/// by name.
pub(super) fn library(
    builtin_yaml: &str,
    user: &[ProfileSpec],
) -> Result<BTreeMap<String, Profile>, LoadErrors> {
    let builtin = parse_profiles(builtin_yaml).map_err(|message| {
        LoadErrors(vec![LoadError {
            at: "built-in profiles".to_owned(),
            message,
        }])
    })?;
    let builtin_names: BTreeSet<&str> = builtin.iter().map(|s| s.name.as_str()).collect();
    let specs = builtin
        .iter()
        .map(|spec| {
            (
                Origin::BuiltIn,
                format!("built-in profile `{}`", spec.name),
                spec,
            )
        })
        .chain(user.iter().enumerate().map(|(i, spec)| {
            (
                Origin::User,
                format!("profiles[{i}] (`{}`)", spec.name),
                spec,
            )
        }));
    let mut errors = Vec::new();
    let mut profiles: BTreeMap<String, Profile> = BTreeMap::new();
    let mut defined_at: BTreeMap<&str, String> = BTreeMap::new();
    let mut user_names = BTreeSet::new();
    for (origin, at, spec) in specs {
        let mut problems = Vec::new();
        let origin = match origin {
            Origin::BuiltIn => {
                if let Some(first) = defined_at.get(spec.name.as_str()) {
                    problems.push(format!(
                        "the name `{}` is already used by {first}",
                        spec.name
                    ));
                }
                Origin::BuiltIn
            }
            Origin::User | Origin::UserOverride => {
                if !user_names.insert(spec.name.as_str()) {
                    let first = defined_at
                        .get(spec.name.as_str())
                        .map(String::as_str)
                        .unwrap_or("an earlier user profile");
                    problems.push(format!(
                        "the name `{}` is already used by {first}",
                        spec.name
                    ));
                }
                match (builtin_names.contains(spec.name.as_str()), spec.r#override) {
                    (true, Some(true)) => Origin::UserOverride,
                    (true, _) => {
                        problems.push(format!(
                            "a built-in profile is named `{}`; use `override: true` to replace it",
                            spec.name
                        ));
                        Origin::User
                    }
                    (false, Some(true)) => {
                        problems.push(format!(
                            "`override: true` names no built-in profile `{}`",
                            spec.name
                        ));
                        Origin::User
                    }
                    (false, _) => Origin::User,
                }
            }
        };
        match profile(spec, origin) {
            Ok(profile) if problems.is_empty() => {
                defined_at.insert(&spec.name, at.clone());
                profiles.insert(profile.name.clone(), profile);
            }
            Ok(_) => {}
            Err(more) => {
                problems.extend(more);
                // A later duplicate is reported against this one even if
                // this one is broken.
                defined_at.entry(&spec.name).or_insert_with(|| at.clone());
            }
        }
        errors.extend(problems.into_iter().map(|message| LoadError {
            at: at.clone(),
            message,
        }));
    }
    if errors.is_empty() {
        Ok(profiles)
    } else {
        Err(LoadErrors(errors))
    }
}

/// One profile, or every problem with it.
fn profile(spec: &ProfileSpec, origin: Origin) -> Result<Profile, Vec<String>> {
    let mut problems = Vec::new();
    if !is_name(&spec.name) {
        problems.push(Syntax::InvalidName(spec.name.clone()).to_string());
    }
    for (field, value) in [
        ("category", &spec.category),
        ("description", &spec.description),
        ("verified_with", &spec.verified_with),
    ] {
        if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
            problems.push(format!("`{field}` is empty"));
        }
    }
    match origin {
        Origin::BuiltIn => {
            if spec.r#override.is_some() {
                problems.push("`override` is for user profiles only".to_owned());
            }
            for (field, value) in [
                ("category", &spec.category),
                ("description", &spec.description),
            ] {
                if value.is_none() {
                    problems.push(format!("a built-in profile needs `{field}`"));
                }
            }
            if spec.find.is_none() && spec.verified_with.is_some() {
                problems.push(
                    "a filter profile has no `verified_with`: it names the tool a source \
                     profile was checked against"
                        .to_owned(),
                );
            }
        }
        Origin::User | Origin::UserOverride => {
            if spec.verified_with.is_some() {
                problems.push("`verified_with` is for built-in profiles only".to_owned());
            }
        }
    }
    let where_ = spec
        .r#where
        .as_ref()
        .map(|w| where_(w, &mut problems))
        .unwrap_or_default();
    let body = match &spec.find {
        Some(find) => source(spec, find, where_, origin, &mut problems)
            .map(Box::new)
            .map(Body::Source),
        None => filter(spec, where_, &mut problems).map(Body::Filter),
    };
    match body {
        Some(body) if problems.is_empty() => Ok(Profile {
            name: spec.name.clone(),
            category: spec.category.clone(),
            description: spec.description.clone(),
            origin,
            body,
        }),
        _ => Err(problems),
    }
}

/// A profile without `find`: a filter, only `where`.
fn filter(spec: &ProfileSpec, where_: Where, problems: &mut Vec<String>) -> Option<Where> {
    if spec.r#where.is_none() {
        problems.push(
            "a profile has `find` (a source profile) or `where` (a filter profile)".to_owned(),
        );
        return None;
    }
    if where_.is_empty() {
        problems.push("a filter's `where` names at least one predicate".to_owned());
    }
    if where_.app_not_running.is_some() {
        problems.push("`app_not_running` is a source predicate (the lock path belongs to the application whose cache the source finds)".to_owned());
    }
    if where_.lacks_entry.is_some() {
        problems.push("`where.lacks_entry` is a source predicate, not a tier filter".to_owned());
    }
    for (field, present, why) in [
        (
            "kind",
            spec.kind.is_some(),
            "a filter narrows any kind its predicates support",
        ),
        (
            "action",
            spec.action.is_some(),
            "a filter narrows a source and removes nothing",
        ),
        (
            "examples",
            spec.examples.is_some(),
            "examples test a source's `find` patterns",
        ),
        (
            "checkouts",
            spec.checkouts.is_some(),
            "whether candidates may hold a checkout is the source's setting",
        ),
        (
            "min_idle_for",
            spec.min_idle_for.is_some(),
            "a mandatory minimum age is the source's setting",
        ),
    ] {
        if present {
            problems.push(format!("a filter profile has no `{field}`: {why}"));
        }
    }
    Some(where_)
}

/// A profile with `find`.
fn source(
    spec: &ProfileSpec,
    find: &FindSpec,
    where_: Where,
    origin: Origin,
    problems: &mut Vec<String>,
) -> Option<Source> {
    let kind = spec.kind.map_or(Kind::Fs, |k| k.0);
    let roots = roots(&find.roots, problems);
    let min_idle_for = spec.min_idle_for.map(|duration| IdleFor(duration.0));
    if let Some(minimum) = min_idle_for {
        if minimum.0.is_zero() {
            problems.push("`min_idle_for` must be greater than zero".to_owned());
        }
        if !PredicateName::IdleFor.supported_by(kind) {
            problems.push(format!("`min_idle_for` is not supported by kind {kind}"));
        }
    }
    for predicate in where_.predicates() {
        if predicate == PredicateName::IdleFor {
            problems.push(
                "a source profile's `where` takes only `has`, `has_not`, \
                 `app_not_running`, and `lacks_entry`: set a mandatory minimum \
                 with `min_idle_for`, or a tier policy with a filter \
                 (`&& filter:idle-3d`)"
                    .to_owned(),
            );
        } else if !predicate.supported_by(kind) {
            let kinds: Vec<&str> = predicate.kinds().iter().map(|k| k.name()).collect();
            problems.push(format!(
                "`where` uses `{predicate}`, which kind {kind} does not support (`{predicate}` \
                 works on: {})",
                kinds.join(", ")
            ));
        }
    }
    if origin == Origin::BuiltIn && spec.verified_with.is_none() {
        problems.push(
            "a built-in source profile needs `verified_with` (the tool version it was checked \
             against)"
                .to_owned(),
        );
    }
    let (mut finds, action, examples) = match kind {
        Kind::Fs => fs(spec, find, problems)?,
        Kind::CargoTarget => {
            for (field, present) in [
                ("find.paths", find.paths.is_some()),
                ("find.exclude", find.exclude.is_some()),
                ("find.type", find.r#type.is_some()),
                ("examples", spec.examples.is_some()),
            ] {
                if present {
                    problems.push(format!(
                        "kind cargo-target finds Cargo target directories by their structure \
                         (docs/safety.md, cargo-target), so it has no `{field}`"
                    ));
                }
            }
            if spec.action.is_some() {
                problems.push(
                    "kind cargo-target fixes its action (the Cargo lock protocol); remove \
                     `action`"
                        .to_owned(),
                );
            }
            (
                Finds::CargoTargets,
                Action::CargoLockProtocol,
                Examples::default(),
            )
        }
        Kind::DockerBuildCache | Kind::DockerImages | Kind::NixStore => {
            problems.push(format!("kind {kind} is not supported yet"));
            return None;
        }
    };
    if let Finds::Paths { patterns, .. } = &mut finds {
        match temp_roots::effective(origin, &spec.name, patterns.clone()) {
            Ok(effective) => *patterns = effective,
            Err(problem) => problems.push(problem),
        }
    }
    if examples.root.is_some() && !matches!(roots.as_ref(), Some(Roots::Required)) {
        problems.push("`examples.root` is only for `find.roots: required`".to_owned());
    }
    Some(Source {
        roots: roots?,
        finds,
        where_,
        min_idle_for,
        action,
        // Absent: refused (fail closed).
        checkouts: spec.checkouts.map_or_else(Checkouts::default, |c| c.0),
        examples,
        verified_with: spec.verified_with.clone(),
    })
}

/// The parts of a kind `fs` source.
fn fs(
    spec: &ProfileSpec,
    find: &FindSpec,
    problems: &mut Vec<String>,
) -> Option<(Finds, Action, Examples)> {
    let patterns = match &find.paths {
        None => {
            problems.push("kind fs needs `find.paths`".to_owned());
            None
        }
        Some(paths) if paths.is_empty() => {
            problems.push("`find.paths` is empty".to_owned());
            None
        }
        Some(paths) => {
            for path in paths {
                if let Err(problem) = pattern(path) {
                    problems.push(problem);
                }
            }
            Some(paths.clone())
        }
    };
    let entry_type = find.r#type.or_else(|| {
        problems.push("kind fs needs `find.type` (dir, file or any)".to_owned());
        None
    });
    let exclude = find.exclude.as_deref().unwrap_or_default();
    for path in exclude {
        if let Err(problem) = pattern(path) {
            problems.push(format!("`find.exclude`: {problem}"));
        }
    }
    let action = match &spec.action {
        None => {
            problems.push(
                "kind fs needs `action` (`delete` or `{command: [...]}`); there is no default"
                    .to_owned(),
            );
            None
        }
        Some(ActionSpec::Delete { make_writable }) => Some(Action::Delete {
            make_writable: *make_writable,
        }),
        Some(ActionSpec::Command { argv, cwd }) => command(argv, *cwd, entry_type, problems),
    };
    let examples = examples(spec.examples.as_ref(), problems);
    Some((
        Finds::Paths {
            patterns: patterns?,
            exclude: exclude.to_vec(),
            entry_type: entry_type?,
        },
        action?,
        examples,
    ))
}

/// A command action, its `cwd` resolved where the entry type decides it
/// (docs/reference.md, Actions: `candidate` is the default for
/// directories).
fn command(
    argv: &[String],
    cwd: Option<Cwd>,
    entry_type: Option<EntryType>,
    problems: &mut Vec<String>,
) -> Option<Action> {
    match argv.first() {
        None => problems.push("`command` is empty: it needs at least the program".to_owned()),
        Some(program) if program.trim().is_empty() => {
            problems.push("`command`'s program is empty".to_owned());
        }
        Some(program) if program.contains("{path}") => problems.push(format!(
            "`command`'s program `{program}` contains `{{path}}`: the program is resolved from \
             PATH and shown before anything runs (docs/safety.md, Command actions), so it \
             cannot be the candidate; pass `{{path}}` as an argument"
        )),
        Some(_) => {}
    }
    let cwd = match (cwd, entry_type?) {
        (Some(Cwd::Candidate), found @ (EntryType::File | EntryType::Any)) => {
            let finds = match found {
                EntryType::File => "files",
                _ => "files as well as directories (`type: any`)",
            };
            problems.push(format!(
                "`cwd: candidate` needs directory candidates, and this profile finds {finds}; \
                 write `cwd: parent`"
            ));
            None
        }
        (Some(cwd), _) => Some(cwd),
        (None, EntryType::Dir) => Some(Cwd::Candidate),
        (None, EntryType::File) => Some(Cwd::Parent),
        (None, EntryType::Any) => None,
    };
    Some(Action::Command {
        argv: argv.to_vec(),
        cwd,
    })
}

/// A kind `fs` source's examples: required, built-in or not, with at least
/// one `match` (docs/reference.md, User profiles: profiles are written by
/// an AI and checked by the tool).
fn examples(spec: Option<&ExamplesSpec>, problems: &mut Vec<String>) -> Examples {
    let Some(spec) = spec else {
        problems.push(
            "kind fs needs `examples` (`{match: [...], no_match: [...]}`), with at least one \
             `match`"
                .to_owned(),
        );
        return Examples::default();
    };
    if spec.r#match.is_empty() {
        problems.push("`examples` needs at least one `match`".to_owned());
    }
    if let Some(root) = &spec.root
        && let Err(problem) = absolute_path(root)
    {
        problems.push(format!("example root {problem}"));
    }
    for path in spec.r#match.iter().chain(&spec.no_match) {
        if let Err(problem) = absolute_path(path) {
            problems.push(format!("example {problem}"));
        }
    }
    Examples {
        root: spec.root.clone(),
        matches: spec.r#match.clone(),
        no_match: spec.no_match.clone(),
    }
}

fn roots(spec: &RootsSpec, problems: &mut Vec<String>) -> Option<Roots> {
    match spec {
        RootsSpec::Required => Some(Roots::Required),
        RootsSpec::Fixed(list) if list.is_empty() => {
            problems.push(
                "`find.roots` is empty: list directories, or write `roots: required`".to_owned(),
            );
            None
        }
        RootsSpec::Fixed(list) => {
            let before = problems.len();
            for root in list {
                if let Err(problem) = absolute_path(root) {
                    problems.push(format!("root {problem}"));
                }
            }
            (problems.len() == before).then(|| Roots::Fixed(list.clone()))
        }
    }
}

fn where_(spec: &WhereSpec, problems: &mut Vec<String>) -> Where {
    for (predicate, name) in [
        ("has", &spec.has),
        ("has_not", &spec.has_not),
        ("lacks_entry", &spec.lacks_entry),
    ] {
        if let Some(name) = name
            && !is_file_name(name)
        {
            problems.push(format!(
                "`{predicate}: {name}` must name one file (no `/`, not `.` or `..`)"
            ));
        }
    }
    if let Some(path) = &spec.app_not_running {
        if let Err(problem) = placeholders(path) {
            problems.push(format!("`app_not_running`: {problem}"));
        }
        let names = path.strip_prefix('/').unwrap_or(path).split('/');
        if std::path::Path::new(path)
            .file_name()
            .is_none_or(|name| name != "SingletonLock")
            || names
                .into_iter()
                .any(|part| part.is_empty() || part == "." || part == "..")
            || path.contains('\0')
            || path.starts_with('~')
        {
            problems.push("`app_not_running` must name a normalized path ending in `SingletonLock` (absolute or relative to each source root)".to_owned());
        }
    }
    Where {
        idle_for: spec.idle_for.map(|d| IdleFor(d.0)),
        has: spec.has.clone(),
        has_not: spec.has_not.clone(),
        app_not_running: spec.app_not_running.clone(),
        lacks_entry: spec.lacks_entry.clone(),
    }
}

fn is_file_name(name: &str) -> bool {
    !name.trim().is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// A configured path (docs/reference.md, Configuration file: Paths):
/// absolute, no `~`, `{uid}` the only placeholder.
pub(super) fn absolute_path(path: &str) -> Result<(), String> {
    if path.starts_with('~') {
        return Err(format!(
            "`{path}`: `~` is not allowed (it means another home for each user); write an \
             absolute path"
        ));
    }
    if !path.starts_with('/') {
        return Err(format!("`{path}` is not an absolute path"));
    }
    placeholders(path)
}

/// A `find.paths` pattern: relative to the roots, one directory level per
/// segment, never leaving the root.
fn pattern(pattern: &str) -> Result<(), String> {
    if pattern.starts_with('/') {
        return Err(format!(
            "pattern `{pattern}` is absolute: patterns are relative to the roots"
        ));
    }
    for segment in pattern.split('/') {
        let problem = match segment {
            "" => "has an empty segment",
            "." | ".." => "has a `.` or `..` segment: a pattern stays below its roots",
            s if s.contains("**") => "uses `**`: each segment is one directory level",
            s if s.contains(crate::fs_kind::RESERVED) => {
                "uses `?`, `[`, `]` or `\\`, which are reserved: within a segment only `*` is \
                 special (any run of bytes within one name)"
            }
            _ => continue,
        };
        return Err(format!("pattern `{pattern}` {problem}"));
    }
    placeholders(pattern).map_err(|problem| format!("pattern {problem}"))
}

fn placeholders(text: &str) -> Result<(), String> {
    if text.replace("{uid}", "").contains(['{', '}']) {
        Err(format!("`{text}`: the only placeholder is `{{uid}}`"))
    } else {
        Ok(())
    }
}
