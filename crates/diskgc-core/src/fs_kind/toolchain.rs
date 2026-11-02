//! The `protect_toolchains` retention guard (docs/safety.md, Toolchain
//! pins; TASK-94): a rustup or elan toolchain directory is kept while its
//! manager could select it from state diskgc can read.
//!
//! The data, read afresh at every judgement (the plan, the tally and the
//! reclaim's locked recheck), never cached:
//!
//! - the manager's `settings.toml` beside `toolchains/` (version 12 only):
//!   its `default_toolchain` and every `[overrides]` value;
//! - elan only: every project listed in its `known-projects` file, by that
//!   project's `lean-toolchain`;
//! - the pin files below the bound root, at relative depths 1 through
//!   [`PIN_DEPTH`] (`rust-toolchain`, `rust-toolchain.toml`;
//!   `lean-toolchain`), found by the same discovery walk as candidates (no
//!   symlink followed, no mount crossed);
//! - rustup only: a Kani installation's `toolchain` link
//!   (`.kani/<version>/toolchain` below the bound root), through the link
//!   guard (`linked_current`).
//!
//! What a name pins is the manager's own rule, reduced to what can be told
//! offline: rustup compares channel and date and ignores the target
//! triple; elan normalises a name as `elan` does and keeps every release
//! of an origin for a channel name (`stable`, `beta`, `nightly`,
//! `lean-toolchain`), which only the network resolves. Any state that
//! cannot be read completely keeps the candidate (`pinned_toolchain`, the
//! detail naming the cause). Pins the walk does not reach (outside the
//! root, deeper, behind a mount or an unreadable directory, in an
//! environment variable or a script) are the profile's documented
//! residual: the manager installs the toolchain again on its next use.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::Find;
use super::discover::{check_deadline, discover};
use super::judge::protect_link_directory;
use crate::engine::{DEFAULT_MAX_ENTRIES, Reason, Skip};
use crate::host::{Base, DirListing, FileType, Host, MonoTime, MountEntry};
use crate::outcome::{Judged, Stop};
use crate::profile::{EntryType, ToolchainManager, Where};
use crate::resolve::{ResolveError, Want, resolve_in};

/// Pin files are searched at relative depths 1 through 5 below the bound
/// root, as the project-scoped built-ins search their directories.
pub(crate) const PIN_DEPTH: usize = 5;
/// `settings.toml` and `known-projects`: larger is not the manager's file.
const MAX_STATE_BYTES: usize = 1024 * 1024;
/// One pin file.
const MAX_PIN_BYTES: usize = 64 * 1024;
/// elan's default origin for a name without one.
const ELAN_DEFAULT_ORIGIN: &str = "leanprover/lean4";

/// Where a pin was read and the toolchain text it holds.
struct Pin {
    from: String,
    named: Named,
}

/// What a pin's text names: a toolchain name, or a path toolchain (a
/// directory the manager runs in place, never one it installed).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Named {
    Name(String),
    Path(String),
}

/// The guard on one found toolchain directory: `Ok(Ok(()))` when no state
/// diskgc can read lets its manager select it.
pub(super) fn protect<H: Host>(
    host: &H,
    found: &super::Found,
    manager: ToolchainManager,
    deadline: Option<MonoTime>,
) -> Judged<()> {
    let retain = |detail: String| Skip::new(Reason::PinnedToolchain, found.path.clone(), detail);
    let Some(name) = found.path.file_name().and_then(OsStr::to_str) else {
        return Ok(Err(retain(
            "the toolchain directory's name is not UTF-8".to_owned(),
        )));
    };
    let Some(home) = manager_home(&found.path) else {
        return Ok(Err(retain(format!(
            "not directly in a `toolchains` directory, so not a {} toolchain",
            manager.name()
        ))));
    };
    check_deadline(host, deadline, &found.path)?;
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            return Ok(Err(retain(format!("cannot read the mount table: {error}"))));
        }
    };

    let settings = home.join("settings.toml");
    let text = match read_state(host, &mounts, &settings, MAX_STATE_BYTES) {
        Ok(Some(text)) => text,
        Ok(None) => {
            return Ok(Err(retain(format!(
                "{} is missing: the default toolchain cannot be told",
                settings.display()
            ))));
        }
        Err(why) => return Ok(Err(retain(why))),
    };
    let pins = match settings_pins(&text) {
        Ok(pins) => pins
            .into_iter()
            .map(|(what, spec)| Pin {
                from: format!("{} {what}", settings.display()),
                named: Named::Name(spec),
            })
            .collect::<Vec<_>>(),
        Err(why) => return Ok(Err(retain(format!("{}: {why}", settings.display())))),
    };
    if let Some(detail) = first_pin(manager, name, &pins) {
        return Ok(Err(retain(detail)));
    }

    if manager == ToolchainManager::Elan {
        let pins = match known_project_pins(host, &mounts, &home, deadline)? {
            Ok(pins) => pins,
            Err(why) => return Ok(Err(retain(why))),
        };
        if let Some(detail) = first_pin(manager, name, &pins) {
            return Ok(Err(retain(detail)));
        }
    }

    let pins = match root_pins(host, &mounts, &found.root, manager, deadline)? {
        Ok(pins) => pins,
        Err(why) => return Ok(Err(retain(why))),
    };
    if let Some(detail) = first_pin(manager, name, &pins) {
        return Ok(Err(retain(detail)));
    }

    if manager == ToolchainManager::Rustup {
        judged!(kani_links(host, &mounts, found, deadline));
    }
    Ok(Ok(()))
}

/// The manager's home: the parent of the `toolchains` directory the
/// candidate lies directly in.
fn manager_home(candidate: &Path) -> Option<PathBuf> {
    let toolchains = candidate.parent()?;
    if toolchains.file_name()? != "toolchains" {
        return None;
    }
    toolchains.parent().map(Path::to_owned)
}

/// The detail of the first pin that names the toolchain directory `dir`,
/// or of the first whose text cannot be understood (which keeps it too).
fn first_pin(manager: ToolchainManager, dir: &str, pins: &[Pin]) -> Option<String> {
    pins.iter()
        .find_map(|pin| match names(manager, &pin.named, dir) {
            Ok(true) => Some(format!("{} names {}", pin.from, describe(&pin.named))),
            Ok(false) => None,
            Err(why) => Some(format!("{}: {why}", pin.from)),
        })
}

fn describe(named: &Named) -> String {
    match named {
        Named::Name(name) => format!("toolchain {name:?}"),
        Named::Path(path) => format!("path toolchain {path:?}"),
    }
}

/// Whether the pin `named` selects the toolchain directory `dir`.
fn names(manager: ToolchainManager, named: &Named, dir: &str) -> Result<bool, String> {
    match named {
        // A path toolchain runs in place; one that names a directory in
        // `toolchains/` by path keeps it (compared by its last name: a
        // lexical comparison of the whole path could miss a symlinked
        // spelling of the same place, and keeping more is safe).
        Named::Path(path) => Ok(Path::new(path).file_name() == Some(OsStr::new(dir))),
        Named::Name(spec) => match manager {
            ToolchainManager::Rustup => Ok(rustup_names(spec, dir)),
            ToolchainManager::Elan => elan_names(spec, dir),
        },
    }
}

// ---- rustup names

/// Whether the rustup toolchain name `spec` (a default, an override or a
/// pin's channel) selects the toolchain directory `dir`. Official names
/// (`stable`, `beta`, `nightly` or a version, then an optional date, then
/// an optional, possibly partial, target triple that rustup completes from
/// the host) compare by channel and date: the triple is ignored, so a
/// partial triple (`stable-gnu`) still keeps `stable-x86_64-unknown-
/// linux-gnu`, at the price of keeping the same channel for another
/// target too. A custom name compares whole.
pub(crate) fn rustup_names(spec: &str, dir: &str) -> bool {
    let spec = spec.trim();
    match (rustup_official(spec), rustup_official(dir)) {
        (Some(pinned), Some(installed)) => pinned == installed,
        _ => spec == dir,
    }
}

/// The channel and date of an official rustup toolchain name, or `None`
/// for a custom name.
fn rustup_official(name: &str) -> Option<(&str, Option<&str>)> {
    let channel_len = ["stable", "beta", "nightly"]
        .into_iter()
        .find(|word| name.starts_with(word))
        .map(str::len)
        .or_else(|| rustup_version_len(name))?;
    let (channel, rest) = name.split_at(channel_len);
    if rest.is_empty() {
        return Some((channel, None));
    }
    let rest = rest.strip_prefix('-')?;
    if let Some(date) = rest.get(..10).filter(|date| is_date(date)) {
        let after = &rest[10..];
        if after.is_empty() || after.starts_with('-') {
            return Some((channel, Some(date)));
        }
    }
    Some((channel, None))
}

/// The length of a leading rustup version channel: `1`, `1.90`, `1.90.0`,
/// optionally `-beta` or `-beta.N`.
fn rustup_version_len(name: &str) -> Option<usize> {
    let bytes = name.as_bytes();
    let digits = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    let mut at = digits(0);
    if at == 0 {
        return None;
    }
    for _ in 0..2 {
        if bytes.get(at) == Some(&b'.') && digits(at + 1) > 0 {
            at += 1 + digits(at + 1);
        }
    }
    if name[at..].starts_with("-beta") {
        let mut end = at + "-beta".len();
        if bytes.get(end) == Some(&b'.') && digits(end + 1) > 0 {
            end += 1 + digits(end + 1);
        }
        if matches!(bytes.get(end), None | Some(b'-')) {
            at = end;
        }
    }
    Some(at)
}

/// `YYYY-MM-DD`, digits only (rustup's archive date).
fn is_date(text: &str) -> bool {
    text.len() == 10
        && text.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}

// ---- elan names

/// What an elan toolchain name selects among installed directories.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ElanTarget {
    /// One directory, by its encoded name.
    Dir(String),
    /// Every directory of this encoded origin: a channel the network
    /// resolves.
    Origin(String),
    /// A path toolchain.
    Path(String),
}

/// Whether the elan toolchain name `spec` selects the directory `dir`.
pub(crate) fn elan_names(spec: &str, dir: &str) -> Result<bool, String> {
    Ok(match elan_target(spec)? {
        ElanTarget::Dir(name) => name == dir,
        ElanTarget::Origin(origin) => dir
            .strip_prefix(origin.as_str())
            .is_some_and(|rest| rest.starts_with("---")),
        ElanTarget::Path(path) => Path::new(&path).file_name() == Some(OsStr::new(dir)),
    })
}

/// elan 4.2's reading of a toolchain name (`lookup_unresolved_toolchain_
/// desc`): `origin:release` or a bare release of leanprover/lean4; a
/// release starting with `nightly` moves to the origin's `-nightly`
/// repository; a release starting with a digit gains a `v`; the directory
/// is the name with `/` written `--` and `:` written `---`. A name that is
/// neither, but looks like a path, is a path toolchain.
fn elan_target(spec: &str) -> Result<ElanTarget, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("an empty toolchain name".to_owned());
    }
    let (origin, release) = match spec.split_once(':') {
        Some((origin, release)) if elan_origin(origin) && elan_release(release) => {
            (origin.to_owned(), release)
        }
        None if elan_release(spec) => (ELAN_DEFAULT_ORIGIN.to_owned(), spec),
        _ if spec.starts_with(['/', '.']) || spec.contains('/') => {
            return Ok(ElanTarget::Path(spec.to_owned()));
        }
        _ => return Err(format!("{spec:?} is not an elan toolchain name")),
    };
    let origin = if release.starts_with("nightly") && !origin.ends_with("-nightly") {
        format!("{origin}-nightly")
    } else {
        origin
    };
    if ["stable", "beta", "nightly", "lean-toolchain"].contains(&release) {
        return Ok(ElanTarget::Origin(elan_encode(&origin)));
    }
    let release = if release.starts_with(|c: char| c.is_ascii_digit()) {
        format!("v{release}")
    } else {
        release.to_owned()
    };
    Ok(ElanTarget::Dir(elan_encode(&format!("{origin}:{release}"))))
}

/// `owner/repository`, each `[A-Za-z0-9_-]+`.
fn elan_origin(text: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    text.split_once('/')
        .is_some_and(|(owner, repo)| part(owner) && part(repo))
}

/// `[A-Za-z0-9.-]+`.
fn elan_release(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

fn elan_encode(name: &str) -> String {
    name.replace('/', "--").replace(':', "---")
}

// ---- the state files

/// The default and override names of a manager's `settings.toml` (rustup
/// and elan share the layout; only version 12 is read: rustup documents
/// the file as private, so another version may mean something else).
fn settings_pins(text: &str) -> Result<Vec<(String, String)>, String> {
    let doc = toml_edit::Document::parse(text)
        .map_err(|error| format!("not TOML: {}", error.message()))?;
    let table = doc.as_table();
    match table.get("version").map(|item| item.as_str()) {
        Some(Some("12")) => {}
        Some(Some(version)) => {
            return Err(format!("settings version {version:?}; only \"12\" is read"));
        }
        Some(None) => return Err("`version` is not a string".to_owned()),
        None => return Err("no `version`".to_owned()),
    }
    let mut pins = Vec::new();
    if let Some(item) = table.get("default_toolchain") {
        let name = item.as_str().ok_or("`default_toolchain` is not a string")?;
        pins.push(("default_toolchain".to_owned(), name.to_owned()));
    }
    if let Some(item) = table.get("overrides") {
        let overrides = item.as_table_like().ok_or("`overrides` is not a table")?;
        for (dir, value) in overrides.iter() {
            let name = value
                .as_str()
                .ok_or_else(|| format!("the override of {dir:?} is not a string"))?;
            pins.push((format!("override of {dir:?}"), name.to_owned()));
        }
    }
    Ok(pins)
}

/// What one pin file names: `Ok(None)` when it names no toolchain (a
/// `rust-toolchain.toml` with only components: the default applies).
fn pin_file(manager: ToolchainManager, file: &str, text: &str) -> Result<Option<Named>, String> {
    if text.lines().count() == 0 {
        return Err("empty".to_owned());
    }
    match manager {
        // elan reads the first line.
        ToolchainManager::Elan => {
            let first = text.lines().next().unwrap_or_default().trim();
            if first.is_empty() {
                return Err("its first line is empty".to_owned());
            }
            Ok(Some(Named::Name(first.to_owned())))
        }
        // rustup: a one-line legacy `rust-toolchain` is a channel name;
        // anything else is TOML with a `[toolchain]` table.
        ToolchainManager::Rustup => {
            if file == "rust-toolchain" && text.lines().count() == 1 {
                let channel = text.trim();
                if channel.is_empty() {
                    return Err("empty".to_owned());
                }
                return Ok(Some(Named::Name(channel.to_owned())));
            }
            let doc = toml_edit::Document::parse(text)
                .map_err(|error| format!("not TOML: {}", error.message()))?;
            let Some(toolchain) = doc.as_table().get("toolchain") else {
                return Ok(None);
            };
            let toolchain = toolchain
                .as_table_like()
                .ok_or("`toolchain` is not a table")?;
            let text_of = |key: &str| -> Result<Option<String>, String> {
                toolchain
                    .get(key)
                    .map(|item| {
                        item.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| format!("`toolchain.{key}` is not a string"))
                    })
                    .transpose()
            };
            Ok(match (text_of("channel")?, text_of("path")?) {
                (Some(channel), _) => Some(Named::Name(channel)),
                (None, Some(path)) => Some(Named::Path(path)),
                (None, None) => None,
            })
        }
    }
}

impl ToolchainManager {
    /// The pin file names this manager reads in a project directory.
    fn pin_files(self) -> &'static [&'static str] {
        match self {
            Self::Rustup => &["rust-toolchain", "rust-toolchain.toml"],
            Self::Elan => &["lean-toolchain"],
        }
    }
}

/// elan's `known-projects` (one absolute project directory per line,
/// written when elan runs in a project): each project's `lean-toolchain`.
/// A project or file that is gone pins nothing, as for elan's own gc.
fn known_project_pins<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    home: &Path,
    deadline: Option<MonoTime>,
) -> Result<Result<Vec<Pin>, String>, Stop> {
    let list = home.join("known-projects");
    let text = match read_state(host, mounts, &list, MAX_STATE_BYTES) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(Ok(Vec::new())),
        Err(why) => return Ok(Err(why)),
    };
    let mut pins = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let project = Path::new(line);
        if !project.is_absolute() {
            return Ok(Err(format!(
                "{} lists a relative project {line:?}",
                list.display()
            )));
        }
        let file = project.join("lean-toolchain");
        check_deadline(host, deadline, &file)?;
        match read_state(host, mounts, &file, MAX_PIN_BYTES) {
            Ok(Some(text)) => match pin_file(ToolchainManager::Elan, "lean-toolchain", &text) {
                Ok(Some(named)) => pins.push(Pin {
                    from: format!("{} (a project in {})", file.display(), list.display()),
                    named,
                }),
                Ok(None) => {}
                Err(why) => return Ok(Err(format!("{}: {why}", file.display()))),
            },
            Ok(None) => {}
            Err(why) => return Ok(Err(why)),
        }
    }
    Ok(Ok(pins))
}

/// The pin files below `root` at relative depths 1 through [`PIN_DEPTH`],
/// found by discovery (no symlink followed, no mount crossed: a pin file
/// that is a symlink, or lies behind a mount or an unreadable directory,
/// is not seen). One found but not readable or not understood is an
/// error, which keeps the candidate.
fn root_pins<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    root: &Path,
    manager: ToolchainManager,
    deadline: Option<MonoTime>,
) -> Result<Result<Vec<Pin>, String>, Stop> {
    let patterns: Vec<String> = (0..PIN_DEPTH)
        .flat_map(|depth| {
            manager
                .pin_files()
                .iter()
                .map(move |file| format!("{}{file}", "*/".repeat(depth)))
        })
        .collect();
    let find = Find::new(&patterns, EntryType::File, &Where::default(), 0);
    let (discovery, stopped) = discover(host, &[root.to_owned()], &find, deadline);
    if let Some(stop) = stopped {
        return Err(stop);
    }
    let mut pins = Vec::new();
    for (path, result) in discovery.candidates {
        if let Err(skip) = result {
            return Ok(Err(format!(
                "pin file {} cannot be read safely ({}: {})",
                path.display(),
                skip.reason,
                skip.detail
            )));
        }
        let file = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        check_deadline(host, deadline, &path)?;
        match read_state(host, mounts, &path, MAX_PIN_BYTES) {
            Ok(Some(text)) => match pin_file(manager, file, &text) {
                Ok(Some(named)) => pins.push(Pin {
                    from: path.display().to_string(),
                    named,
                }),
                Ok(None) => {}
                Err(why) => return Ok(Err(format!("{}: {why}", path.display()))),
            },
            // Gone since it was found: it pins nothing now.
            Ok(None) => {}
            Err(why) => return Ok(Err(why)),
        }
    }
    Ok(Ok(pins))
}

/// Kani (`cargo kani setup`) installs a pinned nightly with rustup and
/// links it as `.kani/<version>/toolchain`; every symlink directly in each
/// `.kani/<version>` directory is checked by the link guard, which keeps
/// the toolchain it resolves into (`linked_current`).
fn kani_links<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    found: &super::Found,
    deadline: Option<MonoTime>,
) -> Judged<()> {
    let retain = |detail: String| Skip::new(Reason::LinkedCurrent, found.path.clone(), detail);
    let kani = found.root.join(".kani");
    check_deadline(host, deadline, &kani)?;
    let dir = match resolve_in(host, mounts, &kani, Want::Directory) {
        Ok(dir) => dir,
        Err(ResolveError::Missing { .. }) => return Ok(Ok(())),
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot reach Kani's directory {}: {error}",
                kani.display()
            ))));
        }
    };
    let names = match host.read_dir(&dir.fd, DEFAULT_MAX_ENTRIES) {
        Ok(DirListing::Names(names)) => names,
        Ok(DirListing::TooMany) => {
            return Ok(Err(retain(format!(
                "Kani's directory {} has too many entries to check",
                kani.display()
            ))));
        }
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot list Kani's directory {}: {error}",
                kani.display()
            ))));
        }
    };
    for name in names {
        let stat = match host.statx(Base::Dir(&dir.fd), Path::new(&name)) {
            Ok(stat) => stat,
            Err(error) => {
                return Ok(Err(retain(format!(
                    "cannot stat {}: {error}",
                    kani.join(&name).display()
                ))));
            }
        };
        if stat.file_type == Some(FileType::Directory) {
            judged!(protect_link_directory(
                host,
                found,
                &kani.join(&name),
                deadline
            ));
        }
    }
    Ok(Ok(()))
}

/// The text of a small regular file reached from `/` without a symlink or
/// mount crossing (network mounts are screened before anything there is
/// opened), unchanged while read: `Ok(None)` when it or a directory on its
/// way does not exist; `Err` with the cause otherwise.
fn read_state<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    path: &Path,
    limit: usize,
) -> Result<Option<String>, String> {
    let opened = match resolve_in(host, mounts, path, Want::Any) {
        Ok(opened) => opened,
        Err(ResolveError::Missing { .. }) => return Ok(None),
        Err(error) => return Err(format!("cannot reach {}: {error}", path.display())),
    };
    if opened.stat.file_type != Some(FileType::Regular) {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let bytes = host
        .read_head(&opened.fd, limit + 1)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!("{} exceeds the {limit} byte limit", path.display()));
    }
    let after = resolve_in(host, mounts, path, Want::Any)
        .map_err(|error| format!("cannot recheck {}: {error}", path.display()))?;
    if after.stat != opened.stat {
        return Err(format!("{} changed while read", path.display()));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("{} is not UTF-8", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "x86_64-unknown-linux-gnu";

    #[test]
    fn rustup_compares_channel_and_date_not_the_triple() {
        for (spec, dir, expected) in [
            (
                "nightly-x86_64-unknown-linux-gnu",
                format!("nightly-{HOST}"),
                true,
            ),
            ("nightly", format!("nightly-{HOST}"), true),
            ("nightly", format!("nightly-2025-11-21-{HOST}"), false),
            (
                "nightly-2025-11-21",
                format!("nightly-2025-11-21-{HOST}"),
                true,
            ),
            (
                "nightly-2025-11-21",
                format!("nightly-2025-11-22-{HOST}"),
                false,
            ),
            ("1.90.0", format!("1.90.0-{HOST}"), true),
            ("1.90.0", format!("1.90-{HOST}"), false),
            ("1.90", format!("1.90-{HOST}"), true),
            ("1.90", format!("1.90.0-{HOST}"), false),
            ("stable", format!("stable-{HOST}"), true),
            ("stable-gnu", format!("stable-{HOST}"), true),
            ("stable", format!("beta-{HOST}"), false),
            ("beta-2025-01-01", format!("beta-2025-01-01-{HOST}"), true),
            ("1.80.0-beta.3", format!("1.80.0-beta.3-{HOST}"), true),
            ("1.80.0-beta.3", format!("1.80.0-{HOST}"), false),
            (" 1.90.0\n", format!("1.90.0-{HOST}"), true),
            ("custom", "custom".to_owned(), true),
            ("custom", "custom-2".to_owned(), false),
            ("stage1", format!("stable-{HOST}"), false),
        ] {
            assert_eq!(rustup_names(spec, &dir), expected, "{spec:?} vs {dir:?}");
        }
    }

    #[test]
    fn elan_normalises_names_as_elan_does() {
        for (spec, dir, expected) in [
            (
                "leanprover/lean4:v4.16.0",
                "leanprover--lean4---v4.16.0",
                true,
            ),
            ("v4.16.0", "leanprover--lean4---v4.16.0", true),
            ("4.16.0", "leanprover--lean4---v4.16.0", true),
            (
                "leanprover/lean4:4.16.0",
                "leanprover--lean4---v4.16.0",
                true,
            ),
            (
                "leanprover/lean4:v4.16.0",
                "leanprover--lean4---v4.17.0",
                false,
            ),
            (
                "leanprover/lean4:v4.30.0-rc1",
                "leanprover--lean4---v4.30.0-rc1",
                true,
            ),
            (
                "nightly-2025-01-01",
                "leanprover--lean4-nightly---nightly-2025-01-01",
                true,
            ),
            (
                "leanprover/lean4:nightly-2025-01-01",
                "leanprover--lean4-nightly---nightly-2025-01-01",
                true,
            ),
            ("stable", "leanprover--lean4---v4.16.0", true),
            (
                "leanprover/lean4:stable",
                "leanprover--lean4---v4.30.0-rc1",
                true,
            ),
            (
                "stable",
                "leanprover--lean4-nightly---nightly-2025-01-01",
                false,
            ),
            (
                "nightly",
                "leanprover--lean4-nightly---nightly-2025-01-01",
                true,
            ),
            ("nightly", "leanprover--lean4---v4.16.0", false),
            ("lean-toolchain", "leanprover--lean4---v4.16.0", true),
            ("other/fork:stable", "leanprover--lean4---v4.16.0", false),
            ("other/fork:stable", "other--fork---v1.0.0", true),
            ("../lean4/build/release/stage1", "stage1", true),
            ("/opt/lean", "leanprover--lean4---v4.16.0", false),
        ] {
            assert_eq!(elan_names(spec, dir), Ok(expected), "{spec:?} vs {dir:?}");
        }
        for bad in ["", "  ", "lean4:v1", "a b"] {
            assert!(
                elan_names(bad, "leanprover--lean4---v4.16.0").is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn settings_are_read_only_at_version_12() {
        let pins = settings_pins(
            "version = \"12\"\ndefault_toolchain = \"nightly-x86_64-unknown-linux-gnu\"\n\
             [overrides]\n\"/w\" = \"1.85.0-x86_64-unknown-linux-gnu\"\n",
        )
        .unwrap();
        assert_eq!(
            pins.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(),
            [
                "nightly-x86_64-unknown-linux-gnu",
                "1.85.0-x86_64-unknown-linux-gnu"
            ]
        );
        assert_eq!(settings_pins("version = \"12\"\n").unwrap(), []);
        for bad in [
            "version = \"13\"\n",
            "version = \"2\"\n",
            "default_toolchain = \"stable\"\n",
            "version = 12\n",
            "version = \"12\"\ndefault_toolchain = 1\n",
            "version = \"12\"\noverrides = 1\n",
            "version = \"12\"\n[overrides]\n\"/w\" = 1\n",
            "version = \"12\n",
        ] {
            assert!(settings_pins(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pin_files_are_read_as_their_manager_reads_them() {
        let rustup = |file, text| pin_file(ToolchainManager::Rustup, file, text);
        let name = |n: &str| Ok(Some(Named::Name(n.to_owned())));
        assert_eq!(
            rustup("rust-toolchain", "nightly-2024-06-01\n"),
            name("nightly-2024-06-01")
        );
        assert_eq!(
            rustup(
                "rust-toolchain.toml",
                "[toolchain]\nchannel = \"1.90.0\"\ncomponents = [\"rustfmt\"]\n"
            ),
            name("1.90.0")
        );
        assert_eq!(
            rustup("rust-toolchain", "[toolchain]\nchannel = \"stable\"\n"),
            name("stable")
        );
        assert_eq!(
            rustup("rust-toolchain.toml", "[toolchain]\npath = \"/opt/rust\"\n"),
            Ok(Some(Named::Path("/opt/rust".to_owned())))
        );
        assert_eq!(
            rustup(
                "rust-toolchain.toml",
                "[toolchain]\ncomponents = [\"miri\"]\n"
            ),
            Ok(None)
        );
        assert_eq!(rustup("rust-toolchain.toml", "# nothing\n\n"), Ok(None));
        for bad in [
            "",
            "[toolchain\nchannel = 1\n",
            "[toolchain]\nchannel = 1\n",
            "toolchain = 1\n",
        ] {
            assert!(rustup("rust-toolchain.toml", bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            pin_file(
                ToolchainManager::Elan,
                "lean-toolchain",
                "leanprover/lean4:v4.16.0\nignored\n"
            ),
            name("leanprover/lean4:v4.16.0")
        );
        assert!(pin_file(ToolchainManager::Elan, "lean-toolchain", "\nv4.16.0\n").is_err());
    }

    #[test]
    fn the_manager_home_is_beside_toolchains() {
        assert_eq!(
            manager_home(Path::new("/h/.rustup/toolchains/stable")),
            Some(PathBuf::from("/h/.rustup"))
        );
        assert_eq!(manager_home(Path::new("/h/.rustup/other/stable")), None);
    }
}
