//! The top-level entries of a surveyed `PATH` that no profile matched
//! (docs/reference.md, `survey`), each sized as the tally sizes a unit
//! ([`crate::tally::size::Cursor`]): allocated bytes, each inode once, the
//! newest change, no symlink followed, no mount crossed. Read-only, and
//! advisory: what is sized here is never judged or acted on.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;

use crate::engine::{DEFAULT_MAX_ENTRIES, Reason, is_private_tmp};
use crate::host::{Base, DirListing, Host, MonoTime};
use crate::index::{Identity, MountKey};
use crate::resolve::{ResolveError, Want, is_mount_point, resolve_in};
use crate::tally::budget::Budget;
use crate::tally::size::{Cursor, Sized, Step};

/// One unmatched top-level entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its name in `PATH`.
    pub name: OsString,
    pub size: Size,
}

/// What sizing an entry came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Sized(Sized),
    /// Not sized: a tree the eligibility walk would refuse as it is (too
    /// many entries, one unreadable), the deadline or a signal first, or
    /// a host error (logged).
    Unsized(Unsized),
}

/// Why an entry has no size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsized {
    Refused(Reason),
    Stopped,
    HostError,
}

/// The unmatched entries of `path`: its names, less those in `matched`
/// (the first component below `path` of every candidate the plan judged),
/// less `systemd-private-*` (never statted, docs/safety.md, PrivateTmp)
/// and mount points (another filesystem: not this one's space), each
/// sized until `deadline` or a signal. `Ok(None)`: `path`'s mount chain is
/// one diskgc does not look into (a FUSE, network or automount mount: the
/// mount screen, docs/safety.md, Before the walk), said in one log line.
/// `Err`: `path` could not be listed.
pub fn entries<H: Host>(
    host: &H,
    path: &Path,
    matched: &BTreeSet<OsString>,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
) -> Result<Option<Vec<Entry>>, String> {
    let mounts = host
        .mountinfo()
        .map_err(|e| format!("read the mount table: {e}"))?;
    let dir = match resolve_in(host, &mounts, path, Want::Directory) {
        Ok(dir) => dir,
        Err(ResolveError::Screened { why, .. }) => {
            log(&format!(
                "survey: {}: its unmatched entries are not listed: {why}",
                path.display()
            ));
            return Ok(None);
        }
        Err(e) => return Err(e.to_string()),
    };
    let names = match host.read_dir(&dir.fd, DEFAULT_MAX_ENTRIES) {
        Ok(DirListing::Names(names)) => names,
        Ok(DirListing::TooMany) => {
            return Err(format!(
                "more than {DEFAULT_MAX_ENTRIES} entries: not listed"
            ));
        }
        Err(e) => return Err(format!("list: {e}")),
    };
    let Some(parent_ino) = dir.stat.ino else {
        return Err("the kernel reported no inode number".to_owned());
    };
    let mount = MountKey::of(&dir.mount);
    let mut names: Vec<OsString> = names
        .into_iter()
        .filter(|name| !matched.contains(name) && !is_private_tmp(name))
        .collect();
    names.sort();
    let mut out = Vec::with_capacity(names.len());
    let mut mount_points = 0_usize;
    for name in names {
        let at = path.join(&name);
        if is_mount_point(&mounts, &at) {
            mount_points += 1;
            continue;
        }
        let size = if stopped(host, deadline) {
            Size::Unsized(Unsized::Stopped)
        } else {
            match size_one(
                host, &mounts, &mount, &dir.fd, &at, &name, parent_ino, deadline,
            ) {
                Ok(Some(size)) => size,
                // Gone since it was listed: not an entry any more.
                Ok(None) => continue,
                Err(why) => {
                    log(&format!("survey: {}: not sized: {why}", at.display()));
                    Size::Unsized(Unsized::HostError)
                }
            }
        };
        out.push(Entry { name, size });
    }
    if mount_points > 0 {
        log(&format!(
            "survey: {}: {mount_points} mount point(s) below it not counted (another filesystem)",
            path.display()
        ));
    }
    Ok(Some(out))
}

/// Whether a signal or the deadline stops the sizing.
fn stopped<H: Host>(host: &H, deadline: Option<MonoTime>) -> bool {
    host.interrupted().is_some() || deadline.is_some_and(|end| host.now() >= end)
}

/// Sizes the entry `name` of `dir` (at `at`) in batches of the background
/// tally's size ([`Budget::batch`]), so a signal or the deadline stops a
/// big tree within one batch. `Ok(None)`: gone.
#[allow(clippy::too_many_arguments)]
fn size_one<H: Host>(
    host: &H,
    mounts: &[crate::host::MountEntry],
    mount: &MountKey,
    dir: &H::Fd,
    at: &Path,
    name: &OsString,
    parent_ino: u64,
    deadline: Option<MonoTime>,
) -> Result<Option<Size>, String> {
    let stat = match host.statx(Base::Dir(dir), Path::new(name)) {
        Ok(stat) => stat,
        Err(e) if crate::index::collapse::means_gone(&e) => return Ok(None),
        Err(e) => return Err(format!("statx: {e}")),
    };
    let Some(identity) = Identity::of(&stat, parent_ino) else {
        return Err("the kernel reported no inode number".to_owned());
    };
    let mut cursor = Cursor::new(mount.clone(), at.to_owned(), identity);
    loop {
        if stopped(host, deadline) {
            return Ok(Some(Size::Unsized(Unsized::Stopped)));
        }
        let mut budget = Budget::batch(host.now());
        match cursor.step(host, mounts, &mut budget, DEFAULT_MAX_ENTRIES) {
            Ok(Step::Paused) => continue,
            Ok(Step::Done(sized)) => return Ok(Some(Size::Sized(sized))),
            Ok(Step::Gone) => return Ok(None),
            Ok(Step::Refused(reason)) => return Ok(Some(Size::Unsized(Unsized::Refused(reason)))),
            Err(e) => return Err(e.to_string()),
        }
    }
}
