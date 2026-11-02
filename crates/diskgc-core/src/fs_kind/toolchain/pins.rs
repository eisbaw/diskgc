//! What selects a toolchain, gathered from the host (docs/safety.md,
//! Toolchain pins): the data first, a [`PinSet`], then the one pure
//! question asked of it ([`PinSet::selects`]).
//!
//! Gatherers: the manager's `settings.toml` (and rustup's system fallback
//! default), elan's `known-projects`, the pin files below the source's
//! bound roots ([`walk_roots`]), and the live processes of the in-use scan
//! (their selection variables and working directories, [`live`]). Every
//! read that cannot be completed is recorded as uncertainty, which keeps
//! every candidate.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::names::{ElanTarget, elan_target, rustup_selects};
use crate::engine::DEFAULT_MAX_ENTRIES;
use crate::fs_kind::discover::check_deadline;
use crate::fs_kind::judge::{
    Followed, follow_links, normalize_link_target, read_small_stable_file,
};
use crate::host::{Access, Base, DevIno, DirListing, Errno, FileType, Host, MonoTime, MountEntry};
use crate::inuse::Scan;
use crate::outcome::Stop;
use crate::profile::ToolchainManager;
use crate::resolve::{
    MountScreenError, ResolveError, Want, is_mount_point, open_beneath, resolve_in,
    screen_mount_chain,
};

/// Pin files are read at relative depths 1 through 5 below a bound root,
/// as the project-scoped built-ins search their directories.
pub(crate) const PIN_DEPTH: usize = 5;
/// `settings.toml`, `known-projects`: larger is not the manager's file.
const MAX_STATE_BYTES: usize = 1024 * 1024;
/// One pin file.
const MAX_PIN_BYTES: usize = 64 * 1024;
/// rustup's distributor/administrator fallback (`UNIX_FALLBACK_SETTINGS`),
/// whose default applies when the user's settings name none.
pub(crate) const RUSTUP_FALLBACK_SETTINGS: &str = "/etc/rustup/settings.toml";
/// The top-level keys each manager's settings.toml 12 may hold (rustup
/// 1.29.1 `Settings`, elan 4.2.4 `Settings`); another key is a format
/// diskgc has not read, and keeps everything.
const RUSTUP_SETTINGS_KEYS: &[&str] = &[
    "version",
    "default_host_triple",
    "default_host_tuple",
    "default_toolchain",
    "profile",
    "overrides",
    "pgp_keys",
    "auto_self_update",
    "auto_install",
];
const ELAN_SETTINGS_KEYS: &[&str] = &["version", "default_toolchain", "overrides", "telemetry"];

/// What one pin names, already resolved where it is a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Named {
    /// A toolchain name, matched by the manager's grammar.
    Name(String),
    /// A path toolchain or link, resolved to the object it leads to.
    Path(PathBuf),
    /// Any installed toolchain may be the one (an elan pin that names
    /// another repository's `lean-toolchain`).
    Any,
}

/// One pin and where it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pin {
    pub from: String,
    pub named: Named,
}

/// Everything gathered: the pins, and the places whose pins could not be
/// read (each keeps every candidate).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PinSet {
    pub pins: Vec<Pin>,
    pub unknown: Vec<String>,
}

impl PinSet {
    pub(crate) fn extend(&mut self, other: PinSet) {
        self.pins.extend(other.pins);
        self.unknown.extend(other.unknown);
    }

    fn unknown(&mut self, why: String) {
        self.unknown.push(why);
    }

    /// The detail of why the installed toolchain directory `candidate`
    /// (named `dir`) is selected or cannot be shown not to be, or `None`.
    pub(crate) fn selects(
        &self,
        manager: ToolchainManager,
        candidate: &Path,
        dir: &str,
    ) -> Option<String> {
        if let Some(first) = self.unknown.first() {
            return Some(format!(
                "pin state not completely known ({} place(s)), e.g. {first}",
                self.unknown.len()
            ));
        }
        self.pins.iter().find_map(|pin| {
            let selected = match &pin.named {
                Named::Any => true,
                Named::Path(target) => target.starts_with(candidate),
                Named::Name(spec) => match manager {
                    ToolchainManager::Rustup => rustup_selects(spec, dir),
                    ToolchainManager::Elan => match elan_target(spec) {
                        Ok(ElanTarget::Dir(name)) => name == dir,
                        Ok(ElanTarget::Origin(origin)) => dir
                            .strip_prefix(origin.as_str())
                            .is_some_and(|rest| rest.starts_with("---")),
                        Ok(ElanTarget::Remote(_)) => true,
                        // Classified when gathered: a path is resolved, a
                        // refused name is uncertainty.
                        Ok(ElanTarget::Path(_)) | Err(_) => true,
                    },
                },
            };
            selected.then(|| match &pin.named {
                Named::Name(spec) => format!("{} names toolchain {spec:?}", pin.from),
                Named::Path(target) => {
                    format!("{} leads to {}", pin.from, target.display())
                }
                Named::Any => format!(
                    "{} names another repository's lean-toolchain, resolved through the \
                     network: any installed toolchain may be the one",
                    pin.from
                ),
            })
        })
    }
}

/// What every gatherer reads through: the host, its mount table (read once
/// per judgement) and the manager whose pins are wanted.
pub(crate) struct Reader<'a, H> {
    pub host: &'a H,
    pub mounts: &'a [MountEntry],
    pub manager: ToolchainManager,
}

/// Where a pin's text came from and how to read it.
pub(crate) struct Source<'a> {
    /// For the detail.
    pub from: String,
    /// The text is the TOML `path` field (always a path).
    pub path_field: bool,
    /// The directory a relative path is relative to (the pin file's);
    /// `None` where nothing anchors one.
    pub base: Option<&'a Path>,
}

/// Classify the pin text `text`: a name, or a path resolved safely.
pub(crate) fn classify<H: Host>(
    reader: &Reader<'_, H>,
    source: Source<'_>,
    text: &str,
    out: &mut PinSet,
) {
    let Reader {
        host,
        mounts,
        manager,
    } = *reader;
    let Source {
        from,
        path_field,
        base,
    } = source;
    let text = text.trim();
    let path = match manager {
        ToolchainManager::Rustup if path_field || Path::new(text).is_absolute() => {
            Some(text.to_owned())
        }
        ToolchainManager::Rustup => None,
        ToolchainManager::Elan => match elan_target(text) {
            Ok(ElanTarget::Path(path)) => Some(path),
            Ok(ElanTarget::Remote(_)) => {
                out.pins.push(Pin {
                    from,
                    named: Named::Any,
                });
                return;
            }
            Ok(_) => None,
            Err(why) => {
                out.unknown(format!("{from}: {why}"));
                return;
            }
        },
    };
    let Some(path) = path else {
        out.pins.push(Pin {
            from,
            named: Named::Name(text.to_owned()),
        });
        return;
    };
    let path = Path::new(&path);
    // As a link's text: a leading `..` climbs from the pin file's
    // directory; one after a named component (which may be a symlink the
    // kernel follows first) cannot be folded, and is uncertainty.
    let absolute = match (path.is_absolute(), base) {
        (false, None) => {
            out.unknown(format!(
                "{from}: a relative path toolchain {} has no directory to resolve against",
                path.display()
            ));
            return;
        }
        (_, base) => match normalize_link_target(base.unwrap_or(Path::new("/")), path) {
            Some(absolute) => absolute,
            None => {
                out.unknown(format!(
                    "{from}: path toolchain {} cannot be resolved safely",
                    path.display()
                ));
                return;
            }
        },
    };
    match follow_links(host, mounts, &absolute) {
        Ok(Followed::Reached(target)) => out.pins.push(Pin {
            from,
            named: Named::Path(target),
        }),
        // A path toolchain that is not there selects nothing installed.
        Ok(Followed::Dangling { .. }) => {}
        Err(why) => out.unknown(format!(
            "{from}: path toolchain {}: {why}",
            absolute.display()
        )),
    }
}

/// The pins of a manager's `settings.toml` at `home` (version 12 only, no
/// key diskgc has not read): `default_toolchain` and every `[overrides]`
/// value. For rustup without a default of its own, the system fallback's
/// default ([`RUSTUP_FALLBACK_SETTINGS`]; missing or not readable is none,
/// as for rustup).
pub(crate) fn settings<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    manager: ToolchainManager,
    home: &Path,
) -> PinSet {
    let mut out = PinSet::default();
    let path = home.join("settings.toml");
    let text = match read_text(host, mounts, &path, MAX_STATE_BYTES) {
        Ok(Some(text)) => text,
        Ok(None) => {
            out.unknown(format!(
                "{} is missing: the default toolchain cannot be told",
                path.display()
            ));
            return out;
        }
        Err(why) => {
            out.unknown(why);
            return out;
        }
    };
    let keys = match manager {
        ToolchainManager::Rustup => RUSTUP_SETTINGS_KEYS,
        ToolchainManager::Elan => ELAN_SETTINGS_KEYS,
    };
    let parsed = match settings_names(&text, keys) {
        Ok(parsed) => parsed,
        Err(why) => {
            out.unknown(format!("{}: {why}", path.display()));
            return out;
        }
    };
    let has_default = parsed.iter().any(|(what, _)| what == "default_toolchain");
    for (what, name) in parsed {
        classify(
            &Reader {
                host,
                mounts,
                manager,
            },
            Source {
                from: format!("{} {what}", path.display()),
                path_field: false,
                base: None,
            },
            &name,
            &mut out,
        );
    }
    if manager == ToolchainManager::Rustup && !has_default {
        let fallback = Path::new(RUSTUP_FALLBACK_SETTINGS);
        match read_text(host, mounts, fallback, MAX_STATE_BYTES) {
            Ok(Some(text)) => match fallback_default(&text) {
                Ok(Some(name)) => classify(
                    &Reader {
                        host,
                        mounts,
                        manager,
                    },
                    Source {
                        from: format!("{} default_toolchain", fallback.display()),
                        path_field: false,
                        base: None,
                    },
                    &name,
                    &mut out,
                ),
                Ok(None) => {}
                Err(why) => out.unknown(format!("{}: {why}", fallback.display())),
            },
            Ok(None) => {}
            // rustup ignores a fallback it may not read.
            Err(why) if why.contains("EACCES") || why.contains("Permission denied") => {}
            Err(why) => out.unknown(why),
        }
    }
    out
}

/// The default and override names of a `settings.toml` text, refusing a
/// version other than 12 and any top-level key outside `keys`.
pub(crate) fn settings_names(text: &str, keys: &[&str]) -> Result<Vec<(String, String)>, String> {
    let doc = toml_edit::Document::parse(text)
        .map_err(|error| format!("not TOML: {}", error.message()))?;
    let table = doc.as_table();
    if let Some((key, _)) = table.iter().find(|(key, _)| !keys.contains(key)) {
        return Err(format!("an unknown key `{key}`"));
    }
    match table.get("version").map(|item| item.as_str()) {
        Some(Some("12")) => {}
        Some(Some(version)) => {
            return Err(format!("settings version {version:?}; only \"12\" is read"));
        }
        Some(None) => return Err("`version` is not a string".to_owned()),
        None => return Err("no `version`".to_owned()),
    }
    let mut names = Vec::new();
    if let Some(item) = table.get("default_toolchain") {
        let name = item.as_str().ok_or("`default_toolchain` is not a string")?;
        names.push(("default_toolchain".to_owned(), name.to_owned()));
    }
    if let Some(item) = table.get("overrides") {
        let overrides = item.as_table_like().ok_or("`overrides` is not a table")?;
        for (dir, value) in overrides.iter() {
            let name = value
                .as_str()
                .ok_or_else(|| format!("the override of {dir:?} is not a string"))?;
            names.push((format!("override of {dir:?}"), name.to_owned()));
        }
    }
    Ok(names)
}

/// rustup's `FallbackSettings`: an optional `default_toolchain`.
fn fallback_default(text: &str) -> Result<Option<String>, String> {
    let doc = toml_edit::Document::parse(text)
        .map_err(|error| format!("not TOML: {}", error.message()))?;
    match doc.as_table().get("default_toolchain") {
        None => Ok(None),
        Some(item) => item
            .as_str()
            .map(|name| Some(name.to_owned()))
            .ok_or_else(|| "`default_toolchain` is not a string".to_owned()),
    }
}

/// elan's `known-projects` at `home` (absolute project directories, one
/// per line, which elan records as it runs in them): each one's
/// `lean-toolchain`. A project or file that is gone pins nothing, as for
/// elan's own gc.
pub(crate) fn known_projects<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    home: &Path,
    deadline: Option<MonoTime>,
) -> Result<PinSet, Stop> {
    let mut out = PinSet::default();
    let list = home.join("known-projects");
    let text = match read_text(host, mounts, &list, MAX_STATE_BYTES) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(out),
        Err(why) => {
            out.unknown(why);
            return Ok(out);
        }
    };
    for line in text.lines().filter(|line| !line.is_empty()) {
        let project = Path::new(line);
        if !project.is_absolute() {
            out.unknown(format!(
                "{} lists a relative project {line:?}",
                list.display()
            ));
            continue;
        }
        let file = project.join("lean-toolchain");
        check_deadline(host, deadline, &file)?;
        let from = format!("{} (a project in {})", file.display(), list.display());
        pin_file(host, mounts, ToolchainManager::Elan, &file, from, &mut out);
    }
    Ok(out)
}

/// Read the pin file at `path` (following a symlinked pin file as the
/// manager would, safely) and classify what it names. Not there: nothing.
pub(crate) fn pin_file<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    manager: ToolchainManager,
    path: &Path,
    from: String,
    out: &mut PinSet,
) {
    let target = match follow_links(host, mounts, path) {
        Ok(Followed::Reached(target)) => target,
        Ok(Followed::Dangling { .. }) => return,
        Err(why) => {
            out.unknown(format!("pin file {}: {why}", path.display()));
            return;
        }
    };
    let text = match read_text(host, mounts, &target, MAX_PIN_BYTES) {
        Ok(Some(text)) => text,
        Ok(None) => return,
        Err(why) => {
            out.unknown(format!("pin file {}: {why}", path.display()));
            return;
        }
    };
    let file = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
    // Relative paths in a pin are the pin file's directory's (the name's,
    // as the manager opened it, not its link target's).
    let base = path.parent();
    match parse_pin_file(manager, file, &text) {
        Ok(Some((name, path_field))) => {
            classify(
                &Reader {
                    host,
                    mounts,
                    manager,
                },
                Source {
                    from,
                    path_field,
                    base,
                },
                &name,
                out,
            );
        }
        Ok(None) => {}
        Err(why) => out.unknown(format!("pin file {}: {why}", path.display())),
    }
}

/// What a pin file's text names (and whether it is the TOML `path` field):
/// `Ok(None)` when it names no toolchain (a `rust-toolchain.toml` with
/// only components: the default applies).
pub(crate) fn parse_pin_file(
    manager: ToolchainManager,
    file: &str,
    text: &str,
) -> Result<Option<(String, bool)>, String> {
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
            Ok(Some((first.to_owned(), false)))
        }
        // rustup: a one-line legacy `rust-toolchain` is a channel name or
        // an absolute path; anything else is TOML with a `[toolchain]`.
        ToolchainManager::Rustup => {
            if file == "rust-toolchain" && text.lines().count() == 1 {
                let channel = text.trim();
                if channel.is_empty() {
                    return Err("empty".to_owned());
                }
                return Ok(Some((channel.to_owned(), false)));
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
                (Some(channel), _) => Some((channel, false)),
                (None, Some(path)) => Some((path, true)),
                (None, None) => None,
            })
        }
    }
}

impl ToolchainManager {
    /// The pin file names this manager reads in a project directory.
    pub(crate) fn pin_files(self) -> &'static [&'static str] {
        match self {
            Self::Rustup => &["rust-toolchain", "rust-toolchain.toml"],
            Self::Elan => &["lean-toolchain"],
        }
    }

    /// The variable that selects a toolchain for every command.
    pub(crate) fn variable(self) -> &'static str {
        match self {
            Self::Rustup => "RUSTUP_TOOLCHAIN",
            Self::Elan => "ELAN_TOOLCHAIN",
        }
    }

    /// The names a running manager or installer has.
    pub(crate) fn commands(self) -> &'static [&'static str] {
        match self {
            Self::Rustup => &["rustup", "rustup-init"],
            Self::Elan => &["elan", "elan-init"],
        }
    }
}

/// The pin files below every root in `roots` at relative depths 1
/// through [`PIN_DEPTH`]. The walk follows no symlinked directory, crosses
/// local mounts (it only reads pin files) and never opens a network, FUSE
/// or automount mount (screened from mountinfo: not walked, a documented
/// residual). A directory it cannot list or an entry it cannot stat is
/// uncertainty (the root's pins cannot be told), as is a pin file it finds
/// but cannot read.
pub(crate) fn walk_roots<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    manager: ToolchainManager,
    roots: &[PathBuf],
    deadline: Option<MonoTime>,
) -> Result<PinSet, Stop> {
    let mut out = PinSet::default();
    let names = manager.pin_files();
    // Each directory once at its shallowest depth (a bind mount, or a root
    // below another root, can show one twice), by the identity its open
    // returned: a shallower visit reaches as deep or deeper.
    let mut seen_dirs: HashMap<DevIno, usize> = HashMap::new();
    let mut first_visit = |id: Option<DevIno>, depth: usize| match id {
        None => true,
        Some(id) => match seen_dirs.get(&id) {
            Some(&shallowest) if shallowest <= depth => false,
            _ => {
                seen_dirs.insert(id, depth);
                true
            }
        },
    };
    for root in roots {
        let start = match resolve_in(host, mounts, root, Want::Directory) {
            Ok(start) => start,
            Err(ResolveError::Missing { .. }) => continue,
            Err(error) => {
                out.unknown(format!("bound root {}: {error}", root.display()));
                continue;
            }
        };
        if !first_visit(start.stat.dev_ino(), 0) {
            continue;
        }
        let mut stack = vec![(root.clone(), start.fd, start.mount.id, 0usize)];
        while let Some((dir, fd, mount_id, depth)) = stack.pop() {
            check_deadline(host, deadline, &dir)?;
            let entries = match host.read_dir(&fd, DEFAULT_MAX_ENTRIES) {
                Ok(DirListing::Names(entries)) => entries,
                Ok(DirListing::TooMany) => {
                    out.unknown(format!("{} has too many entries to list", dir.display()));
                    continue;
                }
                Err(error) => {
                    out.unknown(format!("cannot list {}: {error}", dir.display()));
                    continue;
                }
            };
            for name in entries {
                let child = dir.join(&name);
                if is_mount_point(mounts, &child) {
                    if depth + 1 >= PIN_DEPTH {
                        continue;
                    }
                    match screen_mount_chain(mounts, &child) {
                        Ok(()) => {}
                        Err(MountScreenError::Networked { .. }) => continue,
                        Err(why) => {
                            out.unknown(format!("mount {}: {why}", child.display()));
                            continue;
                        }
                    }
                    match resolve_in(host, mounts, &child, Want::Directory) {
                        Ok(below) if !first_visit(below.stat.dev_ino(), depth + 1) => {}
                        Ok(below) => stack.push((child, below.fd, below.mount.id, depth + 1)),
                        Err(ResolveError::Missing { .. }) => {}
                        Err(error) => {
                            out.unknown(format!("mount {}: {error}", child.display()));
                        }
                    }
                    continue;
                }
                let is_pin = name.to_str().is_some_and(|n| names.contains(&n));
                if !is_pin && depth + 1 >= PIN_DEPTH {
                    continue;
                }
                let stat = match host.statx(Base::Dir(&fd), Path::new(&name)) {
                    Ok(stat) => stat,
                    Err(error) if error.is(Errno::ENOENT) => continue,
                    Err(error) => {
                        out.unknown(format!("cannot stat {}: {error}", child.display()));
                        continue;
                    }
                };
                match stat.file_type {
                    Some(FileType::Regular | FileType::Symlink) if is_pin => {
                        let from = child.display().to_string();
                        pin_file(host, mounts, manager, &child, from, &mut out);
                    }
                    Some(FileType::Directory) if depth + 1 < PIN_DEPTH => {
                        match open_beneath(
                            host,
                            &fd,
                            mount_id,
                            Path::new(&name),
                            Access::Path,
                            Want::Directory,
                        ) {
                            Ok((_, stat)) if !first_visit(stat.dev_ino(), depth + 1) => {}
                            Ok((below, _)) => stack.push((child, below, mount_id, depth + 1)),
                            Err(ResolveError::Missing { .. }) => {}
                            Err(error) => {
                                out.unknown(format!("cannot open {}: {error}", child.display()));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(out)
}

/// What the live processes select (the in-use scan's read of each
/// visible process of the users): `RUSTUP_TOOLCHAIN` / `ELAN_TOOLCHAIN`
/// in its initial environment, and the pin files the manager would find
/// from its working directory upward (rustup's and elan's own lookup:
/// every ancestor's pin files are read, which keeps more than the first).
/// A process whose root is not diskgc's has working-directory text of
/// another view: its directories are not looked up (a documented
/// residual), as is a process whose environment could not be read.
pub(crate) fn live<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    manager: ToolchainManager,
    scan: &Scan,
) -> PinSet {
    let mut out = PinSet::default();
    let own_root = resolve_in(host, mounts, Path::new("/"), Want::Directory)
        .ok()
        .and_then(|root| root.stat.dev_ino());
    let mut looked: HashSet<PathBuf> = HashSet::new();
    for (pid, seen) in scan.readable() {
        let who = format!("pid {pid} ({})", seen.command.to_string_lossy());
        // An environment that could not be read is no evidence, as for the
        // in-use check's environment rule (another user's process, one
        // that ended mid-read): a documented residual.
        for (var, value) in &seen.toolchain_env {
            if var == manager.variable() {
                match value.to_str() {
                    Some(value) if !value.trim().is_empty() => classify(
                        &Reader {
                            host,
                            mounts,
                            manager,
                        },
                        Source {
                            from: format!("{} of {who}", manager.variable()),
                            path_field: false,
                            base: None,
                        },
                        value,
                        &mut out,
                    ),
                    Some(_) => {}
                    None => out.unknown(format!("{} of {who} is not UTF-8", manager.variable())),
                }
            }
        }
        if own_root.is_none() || seen.roots.iter().any(|root| Some(*root) != own_root) {
            continue;
        }
        for cwd in &seen.cwds {
            let Some(text) = cwd.to_str() else { continue };
            if text.ends_with(" (deleted)") || !cwd.is_absolute() {
                continue;
            }
            for dir in cwd.ancestors() {
                if !looked.insert(dir.to_owned()) {
                    break;
                }
                for file in manager.pin_files() {
                    let path = dir.join(file);
                    let from = format!("{} (above the working directory of {who})", path.display());
                    pin_file(host, mounts, manager, &path, from, &mut out);
                }
            }
        }
    }
    out
}

/// A small text file (see [`read_small_stable_file`]), UTF-8.
fn read_text<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    path: &Path,
    limit: usize,
) -> Result<Option<String>, String> {
    match read_small_stable_file(host, mounts, path, limit)? {
        None => Ok(None),
        Some(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| format!("{} is not UTF-8", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_read_only_at_version_12_with_known_keys() {
        let names = settings_names(
            "version = \"12\"\ndefault_toolchain = \"nightly-x86_64-unknown-linux-gnu\"\n\
             profile = \"default\"\n[overrides]\n\"/w\" = \"1.85.0-x86_64-unknown-linux-gnu\"\n",
            RUSTUP_SETTINGS_KEYS,
        )
        .unwrap();
        assert_eq!(
            names.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(),
            [
                "nightly-x86_64-unknown-linux-gnu",
                "1.85.0-x86_64-unknown-linux-gnu"
            ]
        );
        assert_eq!(
            settings_names(
                "telemetry = false\nversion = \"12\"\n[overrides]\n",
                ELAN_SETTINGS_KEYS
            )
            .unwrap(),
            []
        );
        for bad in [
            "version = \"13\"\n",
            "version = \"2\"\n",
            "default_toolchain = \"stable\"\n",
            "version = 12\n",
            "version = \"12\"\ndefault_toolchain = 1\n",
            "version = \"12\"\noverrides = 1\n",
            "version = \"12\"\n[overrides]\n\"/w\" = 1\n",
            "version = \"12\n",
            "version = \"12\"\nnew_feature = true\n",
        ] {
            assert!(
                settings_names(bad, RUSTUP_SETTINGS_KEYS).is_err(),
                "{bad:?}"
            );
        }
        assert!(settings_names("version = \"12\"\nprofile = \"x\"\n", ELAN_SETTINGS_KEYS).is_err());
    }

    #[test]
    fn pin_files_are_read_as_their_manager_reads_them() {
        let rustup = |file, text| parse_pin_file(ToolchainManager::Rustup, file, text);
        let name = |n: &str| Ok(Some((n.to_owned(), false)));
        assert_eq!(
            rustup("rust-toolchain", "nightly-2024-06-01\n"),
            name("nightly-2024-06-01")
        );
        assert_eq!(rustup("rust-toolchain", "/opt/rust\n"), name("/opt/rust"));
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
            rustup("rust-toolchain.toml", "[toolchain]\npath = \"../rust\"\n"),
            Ok(Some(("../rust".to_owned(), true)))
        );
        assert_eq!(
            rustup(
                "rust-toolchain.toml",
                "[toolchain]\ncomponents = [\"miri\"]\n"
            ),
            Ok(None)
        );
        for bad in [
            "",
            "[toolchain\nchannel = 1\n",
            "[toolchain]\nchannel = 1\n",
            "toolchain = 1\n",
        ] {
            assert!(rustup("rust-toolchain.toml", bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            parse_pin_file(
                ToolchainManager::Elan,
                "lean-toolchain",
                "leanprover/lean4:v4.16.0\nx\n"
            ),
            name("leanprover/lean4:v4.16.0")
        );
        assert!(parse_pin_file(ToolchainManager::Elan, "lean-toolchain", "\nv4.16.0\n").is_err());
    }

    #[test]
    fn a_pin_set_selects_by_name_path_or_any_and_unknown_keeps_all() {
        let pin = |named| Pin {
            from: "f".to_owned(),
            named,
        };
        let home = Path::new("/h/.rustup/toolchains");
        let stable = home.join("stable-x86_64-unknown-linux-gnu");
        let set = PinSet {
            pins: vec![pin(Named::Path(stable.clone()))],
            unknown: vec![],
        };
        assert!(
            set.selects(
                ToolchainManager::Rustup,
                &stable,
                "stable-x86_64-unknown-linux-gnu"
            )
            .is_some()
        );
        let beta = home.join("beta-x86_64-unknown-linux-gnu");
        assert!(
            set.selects(
                ToolchainManager::Rustup,
                &beta,
                "beta-x86_64-unknown-linux-gnu"
            )
            .is_none()
        );
        let any = PinSet {
            pins: vec![pin(Named::Any)],
            unknown: vec![],
        };
        assert!(any.selects(ToolchainManager::Elan, &beta, "x").is_some());
        let unknown = PinSet {
            pins: vec![],
            unknown: vec!["cannot list /h/p".to_owned()],
        };
        assert!(
            unknown
                .selects(
                    ToolchainManager::Rustup,
                    &beta,
                    "beta-x86_64-unknown-linux-gnu"
                )
                .is_some()
        );
    }
}
