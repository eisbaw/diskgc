//! The release descriptor, `releases.json`: for the development build and
//! for each release, the commands it runs, the built-in catalog it narrows
//! to, the command actions its built-ins may run, and the files its package
//! ships. Data with one home, read by:
//!
//! - the binary (`crates/diskgc`): which commands it runs, what its help
//!   shows and what its refusals say ([`Entry`]), and through
//!   [`crate::invocation::Catalog`] the profile narrowing;
//! - flake.nix (`lib.importJSON`): what each package installs, the release
//!   notes' header, and the package's `passthru.diskgcBuild`, which the
//!   NixOS module asserts on (packaging/nixos/module.nix, which reads
//!   `watch_units` here too);
//! - tools/docs_vs_binary.py: which documented usages a binary leaves out.
//!
//! A release is named by its `major.minor`; a build of version `X.Y.Z` or
//! `X.Y.Z-dev` with the `release-catalog` feature is release `X.Y`'s
//! ([`release_of_version`]). So master (`0.2.0-dev`) builds the next
//! release's row, and the row of a tagged release stays as it shipped
//! (a test pins 0.1 to what v0.1.0's package held). Which built-ins a
//! catalog holds is builtin.yaml's (`releases:`); a row names the label it
//! narrows to (`catalog`), so a release whose catalog is not decided yet
//! reuses the previous one's.
//!
//! A release runs a built-in's command action (an external cleaner, trusted
//! code: docs/safety.md, Command actions) only when its row lists that
//! built-in with exactly that command (`command_actions`, [`CommandAction`]);
//! the catalog is refused otherwise ([`crate::profile::Library::release_catalog`]),
//! and so is a listed one its catalog does not hold with that command. So
//! admitting a cleaner to a release is two reviewed edits, its label in
//! builtin.yaml and its exact argv here, and changing an admitted cleaner's
//! argv fails until the row says the same (0.2: TASK-360).
//!
//! The rules the data must keep are checked when it is read
//! ([`Descriptor::parse`]); the embedded file is read by every test that
//! asks for a build, so a broken row fails `just check`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::profile::{Cwd, Release};

/// The descriptor as compiled in.
const EMBEDDED: &str = include_str!("releases.json");

/// The commands every row runs: `main.rs`'s help and usage assume them.
pub const EVERY_ROW_RUNS: [Command; 3] = [Command::Ensure, Command::Check, Command::Profiles];

/// A command a build may run, as a user types its first words. `--help`
/// and `--version` are in every build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Command {
    Ensure,
    /// A reclaiming `watch` (one without `--tally-only`).
    Watch,
    WatchTallyOnly,
    Check,
    Profiles,
    /// Read-only (docs/reference.md, `survey`).
    Survey,
}

impl Command {
    pub const ALL: [Self; 6] = [
        Self::Ensure,
        Self::Watch,
        Self::WatchTallyOnly,
        Self::Check,
        Self::Profiles,
        Self::Survey,
    ];

    /// As the descriptor and the help spell it.
    pub fn words(self) -> &'static str {
        match self {
            Self::Ensure => "ensure",
            Self::Watch => "watch",
            Self::WatchTallyOnly => "watch --tally-only",
            Self::Check => "check",
            Self::Profiles => "profiles",
            Self::Survey => "survey",
        }
    }
}

impl<'de> Deserialize<'de> for Command {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::ALL
            .into_iter()
            .find(|c| c.words() == text)
            .ok_or_else(|| {
                let words: Vec<&str> = Self::ALL.iter().map(|c| c.words()).collect();
                serde::de::Error::custom(format!(
                    "unknown command `{text}`, expected one of {}",
                    words.join(", ")
                ))
            })
    }
}

/// Where a shipped file comes from, relative to the source tree.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum FileSource {
    /// Installed byte for byte.
    Copy(String),
    /// The release notes, cut at build time from this document's draft
    /// section (flake.nix, mkReleasePackage).
    ReleaseNotes(String),
}

impl FileSource {
    fn source(&self) -> &str {
        match self {
            Self::Copy(path) | Self::ReleaseNotes(path) => path,
        }
    }
}

/// A command action a release's built-ins may run: one built-in's exact
/// argv and working directory, as builtin.yaml resolves them (`cwd`
/// spelled out). Compared with the built-in's loaded action, never trusted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandAction {
    pub command: Vec<String>,
    pub cwd: Cwd,
}

/// The command actions of a release row, by built-in name.
pub type CommandActions = BTreeMap<String, CommandAction>;

/// One build's row.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    /// The annotated tag a release was cut at; `null` until it is.
    #[serde(default)]
    pub tag: Option<String>,
    /// What it runs; anything else is refused with exit 2.
    pub commands: Vec<Command>,
    /// The builtin.yaml `releases:` label its catalog is narrowed to;
    /// `null`: every built-in and user profiles (development only).
    pub catalog: Option<Release>,
    /// Releases only: the built-ins whose command action this release
    /// runs, each with exactly that command (module docs). Absent: none,
    /// as in 0.1.
    #[serde(default)]
    pub command_actions: CommandActions,
    /// Package paths, each a key of [`Descriptor::files`].
    pub ships: Vec<String>,
}

impl Build {
    pub fn allows(&self, command: Command) -> bool {
        self.commands.contains(&command)
    }

    /// Its commands in a sentence, in [`Command`] order: "ensure, watch
    /// --tally-only, check and profiles". A build with both `watch` modes
    /// says `watch` once.
    pub fn commands_phrase(&self) -> String {
        let mut commands: Vec<Command> = self.commands.clone();
        commands.sort();
        if commands.contains(&Command::Watch) {
            commands.retain(|c| *c != Command::WatchTallyOnly);
        }
        let words: Vec<&str> = commands.iter().map(|c| c.words()).collect();
        match words.split_last() {
            Some((last, [])) => (*last).to_owned(),
            Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
            None => String::new(),
        }
    }
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    /// What the file is, for a reader of the JSON (which has no comments).
    pub about: String,
    pub files: BTreeMap<String, FileSource>,
    /// The `watch` unit templates (docs/architecture.md, The `watch`
    /// units), each a key of `files`: a row ships all or none of them, and
    /// all exactly when it runs a `watch`. The NixOS module asserts a
    /// package ships these (packaging/nixos/module.nix reads this key).
    pub watch_units: Vec<String>,
    pub development: Build,
    pub releases: BTreeMap<String, Build>,
}

impl Descriptor {
    /// Parses and checks a descriptor; every broken rule at once.
    pub fn parse(text: &str) -> Result<Self, String> {
        let descriptor: Self =
            serde_json::from_str(text).map_err(|e| format!("releases.json: {e}"))?;
        let problems = descriptor.problems();
        if problems.is_empty() {
            Ok(descriptor)
        } else {
            Err(format!("releases.json: {}", problems.join("; ")))
        }
    }

    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for (path, source) in &self.files {
            for (what, p) in [("package path", path.as_str()), ("source", source.source())] {
                if !relative_and_plain(p) {
                    problems.push(format!(
                        "{what} `{p}` must be relative, without `.`, `..` or empty parts"
                    ));
                }
            }
        }
        let notes = self
            .files
            .values()
            .filter(|s| matches!(s, FileSource::ReleaseNotes(_)))
            .count();
        if notes > 1 {
            problems.push(format!("{notes} release-notes files; at most one"));
        }
        if self.watch_units.is_empty() {
            problems.push("`watch_units` is empty".into());
        }
        for unit in &self.watch_units {
            if !matches!(self.files.get(unit), Some(FileSource::Copy(_))) {
                problems.push(format!(
                    "watch unit `{unit}` is not a copied file of `files`"
                ));
            }
        }
        let mut shipped = BTreeSet::new();
        let rows = std::iter::once(("development", &self.development))
            .chain(self.releases.iter().map(|(n, b)| (n.as_str(), b)));
        for (name, build) in rows {
            let development = name == "development";
            problems.extend(build_problems(name, build, self));
            shipped.extend(build.ships.iter().map(String::as_str));
            if development {
                if build.catalog.is_some()
                    || build.tag.is_some()
                    || !build.command_actions.is_empty()
                {
                    problems.push(
                        "development: no `catalog` (it has every built-in), no `command_actions` \
                         (it runs every one) and no `tag`"
                            .into(),
                    );
                }
                // flake.nix installs only copies into the development
                // package (installCopies; the notes are cut for a release).
                for path in &build.ships {
                    if !matches!(self.files.get(path), Some(FileSource::Copy(_)) | None) {
                        problems.push(format!(
                            "development: ships `{path}`, which is not a copied file"
                        ));
                    }
                }
            } else {
                if !release_name(name) {
                    problems.push(format!("release `{name}` is not named MAJOR.MINOR"));
                }
                if build.catalog.is_none() {
                    problems.push(format!(
                        "release {name}: a `catalog` is required (a release always narrows)"
                    ));
                }
                if let Some(tag) = &build.tag
                    && !tag.strip_prefix(&format!("v{name}.")).is_some_and(digits)
                {
                    problems.push(format!("release {name}: tag `{tag}` is not v{name}.PATCH"));
                }
            }
        }
        for path in self.files.keys() {
            if !shipped.contains(path.as_str()) {
                problems.push(format!("`{path}` is shipped by no build"));
            }
        }
        problems
    }
}

/// The rules of one row.
fn build_problems(name: &str, build: &Build, descriptor: &Descriptor) -> Vec<String> {
    let files = &descriptor.files;
    let mut problems = Vec::new();
    for command in EVERY_ROW_RUNS {
        if !build.allows(command) {
            problems.push(format!(
                "{name}: does not run `{}`, which every row runs",
                command.words()
            ));
        }
    }
    let mut seen = BTreeSet::new();
    for command in &build.commands {
        if !seen.insert(command) {
            problems.push(format!("{name}: `{}` twice", command.words()));
        }
    }
    for (profile, action) in &build.command_actions {
        if action
            .command
            .first()
            .is_none_or(|program| program.trim().is_empty())
        {
            problems.push(format!(
                "{name}: command action of `{profile}` has no program"
            ));
        }
    }
    let mut seen = BTreeSet::new();
    for path in &build.ships {
        if !seen.insert(path) {
            problems.push(format!("{name}: ships `{path}` twice"));
        }
        if !files.contains_key(path) {
            problems.push(format!(
                "{name}: ships `{path}`, which `files` does not name"
            ));
        }
    }
    // The bug this file was made for (TASK-289): a build that runs `watch`
    // without the templates its NixOS instances are drop-ins on, or the
    // reverse.
    let all = &descriptor.watch_units;
    let units = all.iter().filter(|u| build.ships.contains(u)).count();
    let watch = build.allows(Command::Watch) || build.allows(Command::WatchTallyOnly);
    if units != 0 && units != all.len() {
        problems.push(format!(
            "{name}: ships some `watch` unit templates; all or none"
        ));
    } else if watch && units == 0 {
        problems.push(format!(
            "{name}: runs `watch` but ships no unit templates ({})",
            all.join(", ")
        ));
    } else if !watch && units != 0 {
        problems.push(format!(
            "{name}: ships the `watch` unit templates but runs no `watch`"
        ));
    }
    problems
}

fn relative_and_plain(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn release_name(name: &str) -> bool {
    name.split_once('.')
        .is_some_and(|(major, minor)| digits(major) && digits(minor))
}

/// The embedded descriptor, parsed once per process. An error is a bug in
/// the shipped file, which every test that asks for a build reads.
pub fn descriptor() -> Result<&'static Descriptor, String> {
    static PARSED: OnceLock<Result<Descriptor, String>> = OnceLock::new();
    PARSED
        .get_or_init(|| Descriptor::parse(EMBEDDED))
        .as_ref()
        .map_err(Clone::clone)
}

/// A build's row in the embedded descriptor: the development build
/// (`release: None`) or a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub release: Option<&'static str>,
    pub build: &'static Build,
}

impl std::ops::Deref for Entry {
    type Target = Build;

    fn deref(&self) -> &Build {
        self.build
    }
}

/// The development build's row.
pub fn development() -> Result<Entry, String> {
    Ok(Entry {
        release: None,
        build: &descriptor()?.development,
    })
}

/// Release `name`'s row (`0.1`).
pub fn release(name: &str) -> Result<Entry, String> {
    let descriptor = descriptor()?;
    let (name, build) = descriptor
        .releases
        .get_key_value(name)
        .ok_or_else(|| format!("releases.json has no release {name}"))?;
    Ok(Entry {
        release: Some(name.as_str()),
        build,
    })
}

/// The row a release-catalog build of `version` (`0.2.0-dev`, `0.1.0`)
/// runs: its `major.minor`'s.
pub fn release_of_version(version: &str) -> Result<Entry, String> {
    let mut parts = version.split(['.', '-', '+']);
    let name = match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) if digits(major) && digits(minor) => {
            format!("{major}.{minor}")
        }
        _ => return Err(format!("version `{version}` is not MAJOR.MINOR.PATCH")),
    };
    release(&name).map_err(|why| format!("diskgc {version}: {why}"))
}

#[cfg(test)]
mod tests;
