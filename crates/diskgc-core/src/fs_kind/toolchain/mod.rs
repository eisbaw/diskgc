//! The `protect_toolchains` retention guard (TASK-94). Its rules, their
//! evidence and limits have one home: docs/safety.md, Toolchain pins.
//!
//! Two halves, each read afresh at every judgement:
//!
//! - [`protect`], with the other retention guards (the plan, the tally,
//!   the reclaim under the candidate's lock, the union veto): the
//!   candidate is a toolchain its manager can install again
//!   ([`names`]), its binaries were not read recently, and no pin the
//!   host holds selects it ([`pins`]: settings, elan's known projects, the
//!   pin files below every bound root);
//! - [`live`], on the in-use check's process scan (the plan's, and the
//!   reclaim's last one right before the delete): no rustup or elan
//!   process is running, and no live process selects it (its selection
//!   variable, the pin files above its working directory).
//!
//! Links into a toolchain (a `rustup toolchain link` alias, Kani's
//! `.kani/<version>/toolchain`) are the generic link guard's
//! (`protect_link_dirs` in the built-ins).

mod names;
mod pins;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Find;
use super::discover::{Found, check_deadline};
use super::judge::{Followed, follow_links};
use crate::engine::{DEFAULT_MAX_ENTRIES, Reason, Skip};
use crate::host::{Base, DirListing, FileType, Host, MonoTime, MountEntry};
use crate::inuse::Scan;
use crate::outcome::Judged;
use crate::profile::ToolchainManager;
use crate::resolve::{ResolveError, Want, resolve_in};
use crate::units::format_duration;

/// A toolchain one of whose binaries was read within this long is kept.
/// `relatime` (the Linux default) updates a file's atime at most once a
/// day, so a day is the measure's grain; two weeks spans a holiday or a
/// release cycle of a project that pins its toolchain, and losing an
/// unused toolchain after that costs one download. Under `noatime` the
/// atime stays at install time, so the floor holds only for two weeks
/// after an install and the pins decide alone.
pub(crate) const RECENT_USE: Duration = Duration::from_secs(14 * 24 * 3600);

/// The guard on one found toolchain directory, without the process scan.
/// `roots` are the source's bound roots in this term (the pin walk's).
pub(super) fn protect<H: Host>(
    host: &H,
    found: &Found,
    manager: ToolchainManager,
    roots: &[PathBuf],
    deadline: Option<MonoTime>,
) -> Judged<()> {
    let retain = |detail: String| Skip::new(Reason::PinnedToolchain, found.path.clone(), detail);
    let (name, home) = match place(found, manager) {
        Ok(place) => place,
        Err(why) => return Ok(Err(retain(why))),
    };
    check_deadline(host, deadline, &found.path)?;
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            return Ok(Err(retain(format!("cannot read the mount table: {error}"))));
        }
    };
    let mut set = pins::settings(host, &mounts, manager, &home);
    if manager == ToolchainManager::Elan {
        set.extend(pins::known_projects(host, &mounts, &home, deadline)?);
    }
    if let Some(detail) = set.selects(manager, &found.path, name) {
        return Ok(Err(retain(detail)));
    }
    judged!(recently_used(host, &mounts, found, deadline));
    let own_root = [found.root.clone()];
    let roots = if roots.is_empty() {
        &own_root[..]
    } else {
        roots
    };
    let walked = pins::walk_roots(host, &mounts, manager, roots, deadline)?;
    if let Some(detail) = walked.selects(manager, &found.path, name) {
        return Ok(Err(retain(detail)));
    }
    Ok(Ok(()))
}

/// The live half, judged from `scan` (the in-use check's): a no-op for a
/// source without the guard.
pub(crate) fn live<H: Host>(host: &H, scan: &Scan, find: &Find, found: &Found) -> Judged<()> {
    let Some(manager) = find.protect_toolchains else {
        return Ok(Ok(()));
    };
    let retain = |detail: String| Skip::new(Reason::PinnedToolchain, found.path.clone(), detail);
    let (name, home) = match place(found, manager) {
        Ok(place) => place,
        Err(why) => return Ok(Err(retain(why))),
    };
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            return Ok(Err(retain(format!("cannot read the mount table: {error}"))));
        }
    };
    if let Some(detail) = manager_running(host, &mounts, manager, &home, scan) {
        return Ok(Err(retain(detail)));
    }
    let set = pins::live(host, &mounts, manager, scan);
    Ok(match set.selects(manager, &found.path, name) {
        Some(detail) => Err(retain(detail)),
        None => Ok(()),
    })
}

/// The candidate's name and its manager's home, or why it is kept: not
/// directly in a `toolchains` directory, a name that is not UTF-8, or a
/// custom toolchain the manager cannot install again.
fn place(found: &Found, manager: ToolchainManager) -> Result<(&str, PathBuf), String> {
    let name = found
        .path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or("the toolchain directory's name is not UTF-8")?;
    let toolchains = found
        .path
        .parent()
        .filter(|dir| dir.file_name() == Some(OsStr::new("toolchains")));
    let Some(home) = toolchains.and_then(Path::parent) else {
        return Err(format!(
            "not directly in a `toolchains` directory, so not a {} toolchain",
            manager.name()
        ));
    };
    let reinstallable = match manager {
        ToolchainManager::Rustup => names::rustup_reinstallable(name),
        ToolchainManager::Elan => names::elan_reinstallable(name),
    };
    if !reinstallable {
        return Err(format!(
            "custom toolchain: the manager cannot reinstall it ({} installs only official \
             names{})",
            manager.name(),
            match manager {
                ToolchainManager::Rustup => " with a full target triple",
                ToolchainManager::Elan => " of a release, origin---release",
            }
        ));
    }
    Ok((name, home.to_owned()))
}

/// Kept (`not_idle`) when an entry of the candidate's `bin/` was read
/// within [`RECENT_USE`] by its atime; an atime the kernel does not
/// report, or a `bin/` that cannot be listed, keeps it too. No `bin/`: no
/// evidence either way.
fn recently_used<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    found: &Found,
    deadline: Option<MonoTime>,
) -> Judged<()> {
    let keep = |detail: String| Skip::new(Reason::NotIdle, found.path.clone(), detail);
    let bin = found.path.join("bin");
    check_deadline(host, deadline, &bin)?;
    let dir = match resolve_in(host, mounts, &bin, Want::Directory) {
        Ok(dir) => dir,
        Err(ResolveError::Missing { .. }) => return Ok(Ok(())),
        Err(error) => {
            return Ok(Err(keep(format!(
                "cannot reach {}: {error}",
                bin.display()
            ))));
        }
    };
    let names = match host.read_dir(&dir.fd, DEFAULT_MAX_ENTRIES) {
        Ok(DirListing::Names(names)) => names,
        Ok(DirListing::TooMany) => {
            return Ok(Err(keep(format!("{} has too many entries", bin.display()))));
        }
        Err(error) => return Ok(Err(keep(format!("cannot list {}: {error}", bin.display())))),
    };
    let now = host.wall_clock();
    for name in names {
        let stat = match host.statx(Base::Dir(&dir.fd), Path::new(&name)) {
            Ok(stat) => stat,
            Err(error) => {
                return Ok(Err(keep(format!(
                    "cannot stat {}: {error}",
                    bin.join(&name).display()
                ))));
            }
        };
        if stat.file_type != Some(FileType::Regular) {
            continue;
        }
        let Some(atime) = stat.atime else {
            return Ok(Err(keep(format!(
                "the kernel reports no access time for {}",
                bin.join(&name).display()
            ))));
        };
        let age = now.secs.saturating_sub(atime.secs);
        if age < RECENT_USE.as_secs() as i64 {
            return Ok(Err(keep(format!(
                "{} was read {} ago (atime); a toolchain read within {} is kept",
                bin.join(&name).display(),
                format_duration(Duration::from_secs(age.max(0) as u64)),
                format_duration(RECENT_USE)
            ))));
        }
    }
    Ok(Ok(()))
}

/// Why a running rustup or elan process keeps every toolchain of its
/// manager: it may be installing or updating one (a component download
/// runs outside the toolchain directory and holds nothing in it). Known
/// by its executable's name (`rustup`, `rustup-init`; `elan`,
/// `elan-init`) or its identity: a proxy (`cargo`, `lake`) is the
/// manager's binary under another name, the same inode as
/// `~/.cargo/bin/rustup` or `~/.elan/bin/elan`.
fn manager_running<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    manager: ToolchainManager,
    home: &Path,
    scan: &Scan,
) -> Option<String> {
    let binary = match manager {
        ToolchainManager::Rustup => home.parent().map(|user| user.join(".cargo/bin/rustup")),
        ToolchainManager::Elan => Some(home.join("bin/elan")),
    };
    let binary_id = match binary
        .as_deref()
        .map(|path| follow_links(host, mounts, path))
    {
        Some(Ok(Followed::Reached(path))) => match resolve_in(host, mounts, &path, Want::Any) {
            Ok(reached) => reached.stat.dev_ino(),
            Err(error) => {
                return Some(format!(
                    "cannot reach the {} binary: {error}",
                    manager.name()
                ));
            }
        },
        Some(Ok(Followed::Dangling { .. })) | None => None,
        Some(Err(why)) => {
            return Some(format!("cannot reach the {} binary: {why}", manager.name()));
        }
    };
    scan.readable().find_map(|(pid, seen)| {
        let by_name = manager
            .commands()
            .iter()
            .any(|command| seen.command == OsStr::new(command));
        let by_identity = binary_id.is_some_and(|id| seen.exe_ids.contains(&id));
        (by_name || by_identity).then(|| {
            format!(
                "{} is running (pid {pid}, {}): it may be installing or updating a toolchain",
                manager.name(),
                seen.command.to_string_lossy()
            )
        })
    })
}
