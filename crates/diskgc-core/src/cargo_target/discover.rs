//! Discovery (docs/safety.md, `cargo-target`: Discovery): Cargo target
//! directories below the roots, by a valid `CACHEDIR.TAG` and a profile
//! directory holding a `.cargo-*lock*` file; [`inspect`], what a target
//! holds; [`reach`], a found target reached again from its root.
//!
//! The walk of a root is bounded by [`DISCOVERY_DEPTH`] and by the
//! engine's listing budget per directory; it never follows a symlink,
//! never crosses a mount (every directory is opened with `open_beneath`
//! below its parent, and only directories on the root's mount are
//! entered), never descends into a found target, and refuses a root on a
//! network filesystem without reading it. Hidden directories are walked
//! like any other: agent worktrees (`~/.claude/worktrees/*/target`) hold
//! targets too.

use std::collections::HashSet;
use std::convert::Infallible;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use super::{
    CACHEDIR_TAG_SIGNATURE, Dir, Inspected, Judged, LockFile, ProfileDir, ProfileEntry, Stop,
    Target, newest_change,
};
use crate::engine::{
    self, CACHEDIR_TAG, DEFAULT_MAX_ENTRIES, Identity, KEEP_MARKER, Reason, Skip, WalkStop,
    is_cargo_lock_name, is_network_fs,
};
use crate::host::{Access, Base, DevIno, DirListing, FileType, Host, HostError, MonoTime, Stat};
use crate::resolve::{ResolveError, Resolved, Unsafe, Want, open_beneath, resolve};

/// How far below a root discovery looks: a target at most this many
/// directories down (the root itself is depth 0).
pub const DISCOVERY_DEPTH: usize = 6;

/// What [`discover`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Every target found, once each (by identity), under the outermost
    /// root that reached it, in the order found.
    pub targets: Vec<Target>,
    /// Roots, directories and targets that could not be read or must not
    /// be (a root on a network filesystem, an unreadable directory, a
    /// target that cannot be inspected), each with its reason.
    pub refused: Vec<Skip>,
    /// Paths of targets found again through another root (overlapping
    /// roots, a bind mount of the same directory): the same object as a
    /// target listed in `targets`, so not listed twice.
    pub duplicates: Vec<PathBuf>,
}

/// Finds the Cargo targets below `roots` (absolute paths). Errors of the
/// run (a failing disk, no fds left) stop it; anything about one root or
/// directory is a refusal in the result.
///
/// Roots are walked outermost first (fewest path components; the order
/// given otherwise), so a target that several roots reach is recorded
/// under the outermost of them: keep markers count from a target's root
/// down ([`reach`]), and the outermost root's way down covers every inner
/// root's. Without that, the order of the roots given would decide whether
/// a marker above an inner root is honoured.
pub fn discover<H: Host>(
    host: &H,
    roots: &[PathBuf],
    deadline: Option<MonoTime>,
) -> Result<Discovery, Stop> {
    let mut found = Discovery::default();
    let mut seen = HashSet::new();
    let mut ordered: Vec<&PathBuf> = roots.iter().collect();
    ordered.sort_by_key(|root| root.components().count());
    for root in ordered {
        let resolved = match resolve(host, root, Want::Directory) {
            Ok(resolved) => resolved,
            Err(e) => {
                found.refused.push(unreachable_root(root, e)?);
                continue;
            }
        };
        if is_network_fs(&resolved.mount.fs_type) {
            found
                .refused
                .push(network_fs(root, &resolved.mount.fs_type));
            continue;
        }
        Walk {
            host,
            root,
            mount_id: resolved.mount.id,
            deadline,
            found: &mut found,
            seen: &mut seen,
        }
        .run(resolved.fd)?;
    }
    Ok(found)
}

fn network_fs(path: &Path, fs_type: &str) -> Skip {
    Skip::new(
        Reason::UnsupportedFs,
        path.to_owned(),
        format!(
            "on a network filesystem ({fs_type}): Cargo does not lock there, and a dead server \
             would hang the walk"
        ),
    )
}

/// A root that cannot be reached: its skip reason, or an error of the run.
fn unreachable_root(root: &Path, e: ResolveError) -> Result<Skip, Stop> {
    let reason = match &e {
        ResolveError::UnsafePath { .. } => Reason::UnsafePath,
        ResolveError::UnsupportedFs { .. } => Reason::UnsupportedFs,
        ResolveError::Missing { .. } => Reason::Missing,
        ResolveError::Host(error) => {
            return match failure::<Infallible>(root.to_owned(), "open", error)? {
                Err(skip) => Ok(skip),
                Ok(never) => match never {},
            };
        }
    };
    Ok(Skip::new(reason, root.to_owned(), e.to_string()))
}

/// A host error at `path` while reading what must be checked, classified
/// as the eligibility walk does (one rule): `ENOENT` is `changed`,
/// `EACCES`/`EPERM` `unreadable`, anything else an error of the run.
pub(super) fn failure<T>(path: PathBuf, what: &str, e: &HostError) -> Judged<T> {
    match engine::host_failure(path, what, e) {
        WalkStop::Skipped(skip) => Ok(Err(skip)),
        WalkStop::Failed { path, error } => Err(Stop::Failed { path, error }),
        WalkStop::DeadlinePassed { path } => Err(Stop::DeadlinePassed { path }),
    }
}

pub(super) fn check_deadline<H: Host>(
    host: &H,
    deadline: Option<MonoTime>,
    path: &Path,
) -> Result<(), Stop> {
    if deadline.is_some_and(|d| host.now() >= d) {
        return Err(Stop::DeadlinePassed {
            path: path.to_owned(),
        });
    }
    Ok(())
}

/// The names in the directory `fd` (at `path`), sorted, within the
/// engine's listing budget (`too_many_entries` beyond it).
pub(super) fn list<H: Host>(
    host: &H,
    fd: &H::Fd,
    path: &Path,
    deadline: Option<MonoTime>,
) -> Judged<Vec<OsString>> {
    check_deadline(host, deadline, path)?;
    match host.read_dir(fd, DEFAULT_MAX_ENTRIES) {
        Ok(DirListing::Names(mut names)) => {
            names.sort();
            Ok(Ok(names))
        }
        Ok(DirListing::TooMany) => Ok(Err(Skip::new(
            Reason::TooManyEntries,
            path.to_owned(),
            format!("more than {DEFAULT_MAX_ENTRIES} entries"),
        ))),
        Err(e) => failure(path.to_owned(), "list", &e),
    }
}

/// The identity in `stat`, or `unsupported_fs` when the kernel did not
/// report it.
pub(super) fn identity_of(stat: &Stat, path: &Path) -> Result<Identity, Skip> {
    Identity::of(stat).ok_or_else(|| {
        Skip::new(
            Reason::UnsupportedFs,
            path.to_owned(),
            format!("the kernel did not report its type, inode and owner: {stat:?}"),
        )
    })
}

/// `name` below the open directory `dir` when it is a directory on mount
/// `mount_id`: `Ok(Ok(None))` for anything else but a mount (a symlink, a
/// file), `contains_mount` for a directory of another mount.
fn subdir<H: Host>(
    host: &H,
    dir: &H::Fd,
    mount_id: u64,
    name: &OsString,
    path: &Path,
) -> Judged<Option<Stat>> {
    let stat = match host.statx(Base::Dir(dir), Path::new(name)) {
        Ok(stat) => stat,
        Err(e) => return failure(path.to_owned(), "stat", &e),
    };
    let (Some(file_type), Some(mnt_id)) = (stat.file_type, stat.mnt_id) else {
        return Ok(Err(Skip::new(
            Reason::UnsupportedFs,
            path.to_owned(),
            format!("the kernel did not report its type and mount id: {stat:?}"),
        )));
    };
    if mnt_id != mount_id {
        return Ok(Err(Skip::new(
            Reason::ContainsMount,
            path.to_owned(),
            format!("on mount {mnt_id}, not the target's mount {mount_id}"),
        )));
    }
    Ok(Ok((file_type == FileType::Directory).then_some(stat)))
}

/// Opens the directory `name` below `dir` (`O_PATH`, no symlink, no mount
/// crossing). It was a directory a moment ago, so a symlink or something
/// else there now is a change.
pub(super) fn open_dir<H: Host>(
    host: &H,
    dir: &H::Fd,
    mount_id: u64,
    name: &Path,
    path: &Path,
) -> Judged<(H::Fd, Stat)> {
    match open_beneath(host, dir, mount_id, name, Access::Path, Want::Directory) {
        Ok(opened) => Ok(Ok(opened)),
        Err(ResolveError::Host(e)) => failure(path.to_owned(), "open", &e),
        Err(
            e @ ResolveError::UnsafePath {
                why: Unsafe::CrossesMount | Unsafe::LeftMount { .. },
                ..
            },
        ) => Ok(Err(Skip::new(
            Reason::ContainsMount,
            path.to_owned(),
            e.to_string(),
        ))),
        Err(e @ ResolveError::UnsupportedFs { .. }) => Ok(Err(Skip::new(
            Reason::UnsupportedFs,
            path.to_owned(),
            e.to_string(),
        ))),
        Err(e) => Ok(Err(Skip::new(
            Reason::Changed,
            path.to_owned(),
            format!("no longer the directory it was: {e}"),
        ))),
    }
}

/// Whether the directory `dir` (at `path`) holds a valid `CACHEDIR.TAG`: a
/// regular file starting with the specification's signature. Read through
/// an `O_PATH` fd of the object that was checked to be a regular file.
fn has_valid_tag<H: Host>(host: &H, dir: &H::Fd, mount_id: u64, path: &Path) -> Judged<bool> {
    let tag = path.join(CACHEDIR_TAG);
    let (fd, stat) = match open_beneath(
        host,
        dir,
        mount_id,
        Path::new(CACHEDIR_TAG),
        Access::Path,
        Want::Any,
    ) {
        Ok(opened) => opened,
        Err(ResolveError::Host(e)) => return failure(tag, "open", &e),
        // A symlink, gone, another mount: no valid tag here.
        Err(_) => return Ok(Ok(false)),
    };
    if stat.file_type != Some(FileType::Regular) {
        return Ok(Ok(false));
    }
    match host.read_head(&fd, CACHEDIR_TAG_SIGNATURE.len()) {
        Ok(head) => Ok(Ok(head == CACHEDIR_TAG_SIGNATURE)),
        Err(e) => failure(tag, "read", &e),
    }
}

/// What the target directory open as `target` (at `path`, on mount
/// `mount_id`) holds: its profile directories, one level down
/// (`<t>/<p>/`) or two (`<t>/<triple>/<p>/`), each recognised by a
/// `.cargo-*lock*` file in it, known name or not; the other directories
/// directly in it; and a keep marker in it or in one of those. A directory
/// of another mount one or two levels down (where a profile directory
/// could hide) is `contains_mount`; anything unreadable fails closed.
pub fn inspect<H: Host>(
    host: &H,
    target: &H::Fd,
    mount_id: u64,
    path: &Path,
    deadline: Option<MonoTime>,
) -> Judged<Inspected> {
    let names = judged!(list(host, target, path, deadline));
    let mut inspected = Inspected {
        profiles: Vec::new(),
        dirs: Vec::new(),
        keep_marker: names
            .iter()
            .any(|n| n == KEEP_MARKER)
            .then(|| PathBuf::from(KEEP_MARKER)),
    };
    for name in &names {
        let rel = PathBuf::from(name);
        let at = path.join(name);
        if judged!(subdir(host, target, mount_id, name, &at)).is_none() {
            continue;
        }
        let (fd, stat) = judged!(open_dir(host, target, mount_id, &rel, &at));
        let inside = judged!(list(host, &fd, &at, deadline));
        if inside.iter().any(|n| is_cargo_lock_name(n)) {
            inspected
                .profiles
                .push(judged!(profile(host, &fd, rel, &at, &stat, inside)));
            continue;
        }
        if inspected.keep_marker.is_none() && inside.iter().any(|n| n == KEEP_MARKER) {
            inspected.keep_marker = Some(rel.join(KEEP_MARKER));
        }
        let identity = match identity_of(&stat, &at) {
            Ok(identity) => identity,
            Err(skip) => return Ok(Err(skip)),
        };
        inspected.dirs.push(Dir {
            rel: rel.clone(),
            identity,
        });
        for child in &inside {
            let child_rel = rel.join(child);
            let child_at = path.join(&child_rel);
            if judged!(subdir(host, &fd, mount_id, child, &child_at)).is_none() {
                continue;
            }
            let (child_fd, child_stat) =
                judged!(open_dir(host, &fd, mount_id, Path::new(child), &child_at));
            let grandchildren = judged!(list(host, &child_fd, &child_at, deadline));
            if grandchildren.iter().any(|n| is_cargo_lock_name(n)) {
                inspected.profiles.push(judged!(profile(
                    host,
                    &child_fd,
                    child_rel,
                    &child_at,
                    &child_stat,
                    grandchildren
                )));
            }
        }
    }
    inspected.profiles.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(Ok(inspected))
}

/// The profile directory open as `fd` (at `path`, `rel` below its target),
/// whose sorted listing is `names`: its lock files and everything else,
/// each statted (not followed), so that only real directories count as a
/// layout's directories.
fn profile<H: Host>(
    host: &H,
    fd: &H::Fd,
    rel: PathBuf,
    path: &Path,
    stat: &Stat,
    names: Vec<OsString>,
) -> Judged<ProfileDir> {
    let identity = match identity_of(stat, path) {
        Ok(identity) => identity,
        Err(skip) => return Ok(Err(skip)),
    };
    let mut locks = Vec::new();
    let mut entries = Vec::new();
    for name in names {
        let at = path.join(&name);
        let stat = match host.statx(Base::Dir(fd), Path::new(&name)) {
            Ok(stat) => stat,
            Err(e) => return failure(at, "stat", &e),
        };
        let entry_identity = match identity_of(&stat, &at) {
            Ok(identity) => identity,
            Err(skip) => return Ok(Err(skip)),
        };
        if !is_cargo_lock_name(&name) {
            entries.push(ProfileEntry {
                name,
                file_type: entry_identity.file_type,
            });
            continue;
        }
        let (lock, lock_identity) = (stat, entry_identity);
        locks.push(LockFile {
            name,
            identity: lock_identity,
            changed: newest_change(&lock),
        });
    }
    Ok(Ok(ProfileDir {
        rel,
        identity,
        changed: newest_change(stat),
        locks,
        entries,
    }))
}

/// A walk of one root.
struct Walk<'a, H: Host> {
    host: &'a H,
    root: &'a Path,
    mount_id: u64,
    deadline: Option<MonoTime>,
    found: &'a mut Discovery,
    seen: &'a mut HashSet<DevIno>,
}

/// A directory being walked: its fd and the subdirectories still to visit
/// (opened one at a time, so at most one fd per level is held).
struct Frame<F> {
    fd: F,
    path: PathBuf,
    depth: usize,
    pending: std::vec::IntoIter<OsString>,
}

impl<H: Host> Walk<'_, H> {
    fn run(mut self, root_fd: H::Fd) -> Result<(), Stop> {
        let mut stack: Vec<Frame<H::Fd>> = Vec::new();
        if let Some(frame) = self.visit(root_fd, self.root.to_owned(), 0)? {
            stack.push(frame);
        }
        while let Some(top) = stack.last_mut() {
            let Some(name) = top.pending.next() else {
                stack.pop();
                continue;
            };
            let path = top.path.join(&name);
            let depth = top.depth + 1;
            match open_dir(self.host, &top.fd, self.mount_id, Path::new(&name), &path)? {
                Ok((fd, _)) => {
                    if let Some(frame) = self.visit(fd, path, depth)? {
                        stack.push(frame);
                    }
                }
                Err(skip) => self.refuse(skip),
            }
        }
        Ok(())
    }

    /// A refusal worth reporting; a directory that vanished while the walk
    /// ran (`changed`) is not: it holds nothing any more.
    fn refuse(&mut self, skip: Skip) {
        if skip.reason != Reason::Changed {
            self.found.refused.push(skip);
        }
    }

    /// Looks at the directory open as `fd`: a target is recorded and not
    /// descended into; otherwise its subdirectories on the root's mount are
    /// returned to visit, unless it is at the depth limit.
    fn visit(
        &mut self,
        fd: H::Fd,
        path: PathBuf,
        depth: usize,
    ) -> Result<Option<Frame<H::Fd>>, Stop> {
        let names = match list(self.host, &fd, &path, self.deadline)? {
            Ok(names) => names,
            Err(skip) => {
                self.refuse(skip);
                return Ok(None);
            }
        };
        if names.iter().any(|n| n == CACHEDIR_TAG) {
            match self.target_at(&fd, &path)? {
                Ok(true) => return Ok(None),
                Ok(false) => {}
                Err(skip) => {
                    self.refuse(skip);
                    return Ok(None);
                }
            }
        }
        if depth >= DISCOVERY_DEPTH {
            return Ok(None);
        }
        let mut pending = Vec::new();
        for name in names {
            let at = path.join(&name);
            match subdir(self.host, &fd, self.mount_id, &name, &at)? {
                Ok(Some(_)) => pending.push(name),
                Ok(None) => {}
                // A mount below the root is not crossed, and not refused:
                // it is simply not this root's.
                Err(skip) if skip.reason == Reason::ContainsMount => {}
                Err(skip) => self.refuse(skip),
            }
        }
        Ok(Some(Frame {
            fd,
            path,
            depth,
            pending: pending.into_iter(),
        }))
    }

    /// Records the directory open as `fd` as a target if it is one (a valid
    /// tag and a profile directory); `Ok(Ok(true))` then, so it is not
    /// descended into.
    fn target_at(&mut self, fd: &H::Fd, path: &Path) -> Judged<bool> {
        if !judged!(has_valid_tag(self.host, fd, self.mount_id, path)) {
            return Ok(Ok(false));
        }
        let inspected = judged!(inspect(self.host, fd, self.mount_id, path, self.deadline));
        if inspected.profiles.is_empty() {
            return Ok(Ok(false));
        }
        let stat = match self.host.statx(Base::Dir(fd), Path::new("")) {
            Ok(stat) => stat,
            Err(e) => return failure(path.to_owned(), "stat", &e),
        };
        let identity = match identity_of(&stat, path) {
            Ok(identity) => identity,
            Err(skip) => return Ok(Err(skip)),
        };
        if self.seen.insert(identity.id) {
            self.found.targets.push(Target {
                root: self.root.to_owned(),
                path: path.to_owned(),
                identity,
                inspected,
            });
        } else {
            self.found.duplicates.push(path.to_owned());
        }
        Ok(Ok(true))
    }
}

/// A found target reached again: its directory, and what the process
/// heuristics need of its project directory (its parent).
#[derive(Debug)]
pub struct Reached<F> {
    /// The target directory (`O_PATH`), on its root's mount.
    pub target: Resolved<F>,
    pub project: Project,
}

/// The project directory of a target: its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Project {
    /// Its (device, inode), for matching a working directory there exactly
    /// (`None`: its parent is above the root and could not be reached).
    pub id: Option<DevIno>,
    /// It holds no `Cargo.toml` that is a regular file (or that cannot be
    /// told): the target is **detached**, put there by `--target-dir`,
    /// `CARGO_TARGET_DIR`, a
    /// `target-dir` in `.cargo/config.toml` or a tool's nested target, so
    /// the project directory says nothing about which Cargo uses it
    /// (docs/safety.md, `cargo-target`: In use).
    pub detached: bool,
}

/// The name whose presence in a target's parent makes it the target of a
/// Cargo project there (Cargo puts `target/` next to the workspace's
/// manifest).
pub const CARGO_MANIFEST: &str = "Cargo.toml";

/// Whether the directory `dir` (at `path`) holds a `Cargo.toml` that is a
/// regular file (`statx`, not followed). A directory, a symlink (dangling or
/// not) or anything else of that name does not count, nor does one not
/// found or not readable: no is the conservative answer for
/// [`Project::detached`]. Any other failure is an error of the run.
fn has_manifest<H: Host>(host: &H, dir: &H::Fd, path: &Path) -> Result<bool, Stop> {
    match host.statx(Base::Dir(dir), Path::new(CARGO_MANIFEST)) {
        Ok(stat) => Ok(stat.file_type == Some(FileType::Regular)),
        Err(e) => match failure::<Infallible>(path.join(CARGO_MANIFEST), "stat", &e)? {
            Err(_missing_or_unreadable) => Ok(false),
            Ok(never) => match never {},
        },
    }
}

/// Reaches `target` again from its root, as discovery did: the root
/// resolved (`unsafe_path`, `missing`, `unsupported_fs` for a network
/// filesystem), then one directory at a time below it, no symlink, no
/// mount crossing. Every directory from the root down to the target's
/// parent is listed for a keep marker (`kept`), and the directory reached
/// must still be the one found (`changed` otherwise). The project
/// directory is the target's parent: its identity, and whether it holds a
/// `Cargo.toml` ([`Project`]).
pub fn reach<H: Host>(
    host: &H,
    target: &Target,
    deadline: Option<MonoTime>,
) -> Judged<Reached<H::Fd>> {
    let root = match resolve(host, &target.root, Want::Directory) {
        Ok(root) => root,
        Err(e) => return Ok(Err(unreachable_root(&target.root, e)?)),
    };
    if is_network_fs(&root.mount.fs_type) {
        return Ok(Err(network_fs(&target.root, &root.mount.fs_type)));
    }
    let rel = target
        .path
        .strip_prefix(&target.root)
        .map_err(|_| Stop::Failed {
            path: target.path.clone(),
            error: HostError::invalid(
                "reach",
                target.path.display(),
                format!("not below its root {}", target.root.display()),
            ),
        })?;
    let names: Vec<&std::ffi::OsStr> = rel
        .components()
        .map(|c| match c {
            Component::Normal(name) => Ok(name),
            other => Err(Stop::Failed {
                path: target.path.clone(),
                error: HostError::invalid(
                    "reach",
                    target.path.display(),
                    format!("component {other:?} below the root is not a name"),
                ),
            }),
        })
        .collect::<Result<_, _>>()?;
    let mount = root.mount;
    let mut at = target.root.clone();
    let mut dir = root.fd;
    let mut dir_stat = root.stat;
    let mut parent = None;
    for name in names {
        let listing = judged!(list(host, &dir, &at, deadline));
        if listing.iter().any(|n| n == KEEP_MARKER) {
            return Ok(Err(Skip::new(
                Reason::Kept,
                at.join(KEEP_MARKER),
                "a keep marker between the root and the target",
            )));
        }
        at.push(name);
        let (fd, stat) = match open_beneath(
            host,
            &dir,
            mount.id,
            Path::new(name),
            Access::Path,
            Want::Directory,
        ) {
            Ok(opened) => opened,
            Err(ResolveError::Host(e)) => return failure(at, "open", &e),
            Err(ResolveError::Missing { .. }) => {
                return Ok(Err(Skip::new(Reason::Missing, at, "gone since discovery")));
            }
            Err(e) => {
                return Ok(Err(Skip::new(
                    Reason::Changed,
                    at,
                    format!("not reachable as found: {e}"),
                )));
            }
        };
        parent = Some((std::mem::replace(&mut dir, fd), dir_stat));
        dir_stat = stat;
    }
    let identity = match identity_of(&dir_stat, &target.path) {
        Ok(identity) => identity,
        Err(skip) => return Ok(Err(skip)),
    };
    if identity != target.identity {
        return Ok(Err(Skip::new(
            Reason::Changed,
            target.path.clone(),
            format!(
                "not the directory found (found {:?}, now {:?})",
                target.identity, identity
            ),
        )));
    }
    let project = match parent {
        Some((fd, stat)) => {
            let path = target.path.parent().unwrap_or(&target.root);
            Project {
                id: stat.dev_ino(),
                detached: !has_manifest(host, &fd, path)?,
            }
        }
        // The target is the root: its parent is above it.
        None => match target.path.parent() {
            Some(path) => match resolve(host, path, Want::Directory) {
                Ok(parent) => Project {
                    id: parent.stat.dev_ino(),
                    detached: !has_manifest(host, &parent.fd, path)?,
                },
                Err(ResolveError::Host(e)) => {
                    // Only an error of the run stops; a parent that cannot
                    // be reached costs the exact match, and the target
                    // counts as detached.
                    match failure::<Infallible>(path.to_owned(), "open", &e)? {
                        Err(_unreadable) => Project {
                            id: None,
                            detached: true,
                        },
                        Ok(never) => match never {},
                    }
                }
                Err(_) => Project {
                    id: None,
                    detached: true,
                },
            },
            None => Project {
                id: None,
                detached: true,
            },
        },
    };
    Ok(Ok(Reached {
        target: Resolved {
            fd: dir,
            mount,
            stat: dir_stat,
        },
        project,
    }))
}
