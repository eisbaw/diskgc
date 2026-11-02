//! The project dependency contracts' I/O (`liveness_contract: node-project`
//! and `python-project`; docs/safety.md, Producer liveness: Project
//! dependency directories). Two halves, so a large project's walk never
//! runs inside a deletion's freshness window:
//!
//! - [`gather`], before the process scan: whether the candidate is an
//!   instance (a manifest beside it, `pyvenv.cfg` in a virtual
//!   environment, no dependency directory around it and, for node, no
//!   outer `node_modules` up to the mount root that Node would resolve
//!   instead; otherwise `filtered`), which directory is its project (the
//!   nearest workspace root or version-control top level from its parent
//!   up to the mount root, else the parent; one above the source root, or
//!   a bind-mounted subdirectory's root reached without one, is
//!   `producer_unknown`), the project's walk with every dependency
//!   directory of the candidate's name, and rules 1 (no change: a file's
//!   mtime or ctime, a directory's birth time) and 2 (no read in those
//!   dependency directories, or project-wide when one links out; the
//!   narrowing is PENDING Mark's confirmation);
//! - [`visible_use`], on the scan: rule 3, no visible process uses the
//!   project (identity of every walked inode, the climbs of held
//!   directories, the path test of held files the walk left out, the
//!   environment strings and command-line elements).
//!
//! Every read is a `statx`, a listing, a `readlink` in a dependency
//! directory, or a `package.json` (workspace roots) or `.pth` (virtual
//! environments) read with `O_NOATIME`: diskgc never moves a regular
//! file's read time it judges.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::discover::{Found, check_deadline};
use super::judge::{mountinfo, skip_or_stop};
use crate::engine::{ProcessBlocker, ProcessEvidence, Reason, Skip, WalkRecord};
use crate::host::{
    Access, Base, Dev, DevIno, DirListing, Errno, FileType, Host, HostError, MonoTime, MountEntry,
    Stat,
};
use crate::inuse::{How, Scan};
use crate::liveness::project::{
    Activity, NODE_MODULES, Newest, PACKAGE_JSON, PARENT_NAMES_MAX, PNPM_WORKSPACE,
    PYTHON_MANIFESTS, PYVENV_CFG, VCS_METADATA, WORKSPACE_MANIFEST_MAX_BYTES,
};
use crate::outcome::{Judged, Stop};
use crate::profile::{IdleFor, LivenessContract};
use crate::resolve::{ResolveError, Want, open_beneath, resolve_in};

/// What rule 3 needs of a project the contract walked ([`gather`]).
#[derive(Debug, Clone)]
pub(crate) struct Tree {
    /// The project directory, in diskgc's view.
    pub path: PathBuf,
    /// Its path within its filesystem (its mount's root field and the path
    /// below the mount point), for the held-file path test.
    pub fs_path: PathBuf,
    /// The mount it was reached on (mountinfo's id) and that mount's
    /// filesystem device (mountinfo's).
    pub mount_dev: Dev,
    /// Its filesystem type (memory maps by inode on overlay and btrfs).
    pub fs_type: String,
    /// Every device the walks saw, and those of mounts under the project.
    pub devices: HashSet<Dev>,
    /// Every directory of the project and the candidate.
    pub dirs: HashSet<DevIno>,
    /// Every object of the project walk (the candidate's are the in-use
    /// check's).
    pub objects: HashSet<DevIno>,
}

/// A directory from the mount root down to the candidate's parent, as the
/// descent found it.
struct Level<F> {
    path: PathBuf,
    fd: F,
    stat: Stat,
    /// Above the source root (read for markers and outer dependency
    /// directories only, never walked).
    above_root: bool,
    marker: Option<String>,
}

fn contract_name(contract: LivenessContract) -> &'static str {
    contract.name()
}

/// Where a candidate's project is ([`locate`]): the directories from the
/// mount root down to the candidate's parent, each open, the one that is
/// the project, and the root's mount.
pub(crate) struct Located<F> {
    levels: Vec<Level<F>>,
    project_at: usize,
    mount_id: u64,
    root_mount: MountEntry,
    mounts: Vec<MountEntry>,
}

/// Whether the candidate is an instance of `contract` and where its
/// project is (see the module docs). `filtered` when it lies in another
/// dependency directory (a `node_modules` on its path, a directory from
/// the mount root down holding `pyvenv.cfg`), when an ancestor of its
/// parent holds a `node_modules` that Node would resolve once this one is
/// gone (`node-project`), or when it lacks its manifest (or
/// `pyvenv.cfg`); `producer_unknown` when a workspace manifest cannot be
/// judged, when the nearest project marker lies above the source root (the
/// project extends beyond what diskgc may walk), or when the climb reaches
/// the root of a bind-mounted subdirectory. Its own term applies it
/// ([`super::judge::Judge::contents`]), so a non-instance is never its
/// source's to delete; [`gather`] applies it again before the walk.
pub(crate) fn locate<H: Host>(
    host: &H,
    contract: LivenessContract,
    found: &Found,
    parent: &H::Fd,
    deadline: Option<MonoTime>,
) -> Judged<Located<H::Fd>> {
    let name = contract_name(contract);
    let skip = |reason, at: &Path, detail: String| {
        Ok(Err(Skip::new(
            reason,
            at.to_owned(),
            format!("liveness_contract {name}: {detail}"),
        )))
    };
    if let Some(outer) = crate::liveness::project::inside_node_modules(&found.path) {
        return skip(
            Reason::Filtered,
            &found.path,
            format!(
                "a nested dependency directory (inside {}): it belongs to a package, and \
                 removing it would let Node resolve another copy",
                outer.display()
            ),
        );
    }
    let failed = |path: &Path, error: HostError| Stop::Failed {
        path: path.to_owned(),
        error,
    };
    let refused = |path: &Path, what: &str, e: &HostError| -> Judged<Located<H::Fd>> {
        skip_or_stop(path.to_owned(), what, e).map(|s| {
            Err(Skip::new(
                s.reason,
                s.path,
                format!("liveness_contract {name}: {}", s.detail),
            ))
        })
    };
    let mounts = mountinfo(host)?;
    let Some(parent_path) = found.path.parent() else {
        return skip(
            Reason::UnsafePath,
            &found.path,
            "the candidate has no parent".to_owned(),
        );
    };
    if !parent_path.starts_with(&found.root) {
        return skip(
            Reason::UnsafePath,
            &found.path,
            format!(
                "its parent is not at or below its root {}",
                found.root.display()
            ),
        );
    }
    // The root as resolved (no symlink followed, no mount crossed on the
    // way): its mount is the project's.
    let root = match resolve_in(host, &mounts, &found.root, Want::Directory) {
        Ok(root) => root,
        Err(ResolveError::Host(e)) => return refused(&found.root, "open", &e),
        Err(e) => {
            return skip(
                e.skip_reason().unwrap_or(Reason::UnsafePath),
                &found.root,
                format!("the source root cannot be reached again: {e}"),
            );
        }
    };
    let Some(mount_id) = root.stat.mnt_id else {
        return Err(failed(
            &found.root,
            HostError::invalid("statx", found.root.display(), "no mount id reported"),
        ));
    };
    let root_mount = root.mount.clone();
    let Ok(below_mount) = parent_path.strip_prefix(&root_mount.mount_point) else {
        return skip(
            Reason::UnsafePath,
            &found.root,
            format!(
                "its root is not below its mount point {}",
                root_mount.mount_point.display()
            ),
        );
    };
    // The descent from the mount root to the parent: no symlink followed,
    // no mount crossed; the root and the parent reached must be the ones
    // resolved and judged.
    let top = if root_mount.mount_point == found.root {
        Level {
            path: found.root.clone(),
            fd: root.fd,
            stat: root.stat.clone(),
            above_root: false,
            marker: None,
        }
    } else {
        match resolve_in(host, &mounts, &root_mount.mount_point, Want::Directory) {
            Ok(top) if top.stat.mnt_id == Some(mount_id) => Level {
                path: root_mount.mount_point.clone(),
                fd: top.fd,
                stat: top.stat,
                above_root: true,
                marker: None,
            },
            Ok(_) => {
                return skip(
                    Reason::Changed,
                    &root_mount.mount_point,
                    "the root's mount point is another mount now".to_owned(),
                );
            }
            Err(ResolveError::Host(e)) => return refused(&root_mount.mount_point, "open", &e),
            Err(e) => {
                return skip(
                    e.skip_reason().unwrap_or(Reason::UnsafePath),
                    &root_mount.mount_point,
                    format!("the root's mount point cannot be reached: {e}"),
                );
            }
        }
    };
    let mut levels: Vec<Level<H::Fd>> = vec![top];
    for next in below_mount.iter() {
        check_deadline(host, deadline, &levels[levels.len() - 1].path)?;
        let here = &levels[levels.len() - 1];
        let path = here.path.join(next);
        let opened = open_beneath(
            host,
            &here.fd,
            mount_id,
            Path::new(next),
            Access::Path,
            Want::Directory,
        );
        let (fd, stat) = match opened {
            Ok(opened) => opened,
            Err(ResolveError::Host(e)) => return refused(&path, "open", &e),
            Err(e) => {
                return skip(
                    Reason::Changed,
                    &path,
                    format!("a directory on the way changed: {e}"),
                );
            }
        };
        let above_root = path != found.root && found.root.starts_with(&path);
        levels.push(Level {
            path,
            fd,
            stat,
            above_root,
            marker: None,
        });
    }
    let same = |a: &Stat, b: &Stat| a.dev_ino().is_some() && a.dev_ino() == b.dev_ino();
    let root_level = levels
        .iter()
        .find(|l| l.path == found.root)
        .expect("the root is on the way to the parent");
    if !same(&root_level.stat, &root.stat) {
        return skip(
            Reason::Changed,
            &found.root,
            "the root reached from its mount point is not the one resolved".to_owned(),
        );
    }
    let judged_parent = host
        .statx(Base::Dir(parent), Path::new(""))
        .map_err(|e| failed(parent_path, e))?;
    if !same(&levels[levels.len() - 1].stat, &judged_parent) {
        return skip(
            Reason::Changed,
            parent_path,
            "the parent reached again is not the one the candidate was judged in".to_owned(),
        );
    }
    // Dependency directories around it, from the mount root down: a
    // virtual environment (pyvenv.cfg), and for node an outer
    // node_modules that Node would resolve once this one is gone (any
    // proper ancestor of the parent).
    let last = levels.len() - 1;
    for (at, level) in levels.iter().enumerate() {
        check_deadline(host, deadline, &level.path)?;
        match regular_present(host, &level.fd, PYVENV_CFG) {
            Ok(false) => {}
            Ok(true) => {
                return skip(
                    Reason::Filtered,
                    &found.path,
                    format!(
                        "a nested dependency directory (inside the virtual environment {})",
                        level.path.display()
                    ),
                );
            }
            Err(e) => return refused(&level.path.join(PYVENV_CFG), "stat", &e),
        }
        if contract == LivenessContract::NodeProject && at < last {
            match any_present(host, &level.fd, NODE_MODULES) {
                Ok(false) => {}
                Ok(true) => {
                    return skip(
                        Reason::Filtered,
                        &found.path,
                        format!(
                            "{} exists: removing this one would let Node resolve that one \
                             instead (a hoisted copy, possibly another version), a silent \
                             switch; it goes once that one is gone",
                            level.path.join(NODE_MODULES).display()
                        ),
                    );
                }
                Err(e) => return refused(&level.path.join(NODE_MODULES), "stat", &e),
            }
        }
    }
    let parent_level = &levels[last];
    // The instance: the ecosystem's manifest beside it, and for a virtual
    // environment pyvenv.cfg in it.
    match manifest(host, contract, &parent_level.fd) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return skip(
                Reason::Filtered,
                &found.path,
                match contract {
                    LivenessContract::NodeProject => {
                        format!("no regular {PACKAGE_JSON} beside it: not a project's dependencies")
                    }
                    _ => format!(
                        "no Python project manifest ({}, requirements*.txt) beside it",
                        PYTHON_MANIFESTS.join(", ")
                    ),
                },
            );
        }
        Err(Listing::TooMany) => {
            return skip(
                Reason::Unreadable,
                parent_path,
                format!("more than {PARENT_NAMES_MAX} names (the listing was cut short)"),
            );
        }
        Err(Listing::Failed(e)) => return refused(parent_path, "list", &e),
    }
    let candidate_name = found.path.file_name().expect("a candidate has a name");
    if contract == LivenessContract::PythonProject {
        let venv = match open_beneath(
            host,
            &parent_level.fd,
            mount_id,
            Path::new(candidate_name),
            Access::Path,
            Want::Directory,
        ) {
            Ok((fd, stat)) if stat.dev_ino() == Some(found.identity.id) => fd,
            Ok(_) => {
                return skip(
                    Reason::Changed,
                    &found.path,
                    "replaced since it was walked".to_owned(),
                );
            }
            Err(ResolveError::Host(e)) => return refused(&found.path, "open", &e),
            Err(e) => {
                return skip(
                    Reason::Changed,
                    &found.path,
                    format!("cannot be reached: {e}"),
                );
            }
        };
        match regular_present(host, &venv, PYVENV_CFG) {
            Ok(true) => {}
            Ok(false) => {
                return skip(
                    Reason::Filtered,
                    &found.path,
                    format!("no regular {PYVENV_CFG} in it: not a virtual environment"),
                );
            }
            Err(e) => return refused(&found.path.join(PYVENV_CFG), "stat", &e),
        }
    }
    // The project: the nearest workspace root or version-control top
    // level, from the parent up to the mount root, else the parent. Only
    // the levels up to the nearest are read (a workspace manifest further
    // up that cannot be judged does not matter then).
    let mut project_at = None;
    for at in (0..levels.len()).rev() {
        check_deadline(host, deadline, &levels[at].path)?;
        match marker(host, contract, &levels[at].fd, &levels[at].path) {
            Ok(Ok(None)) => {}
            Ok(Ok(Some(marker))) => {
                levels[at].marker = Some(marker);
                project_at = Some(at);
                break;
            }
            Ok(Err(unknown)) => {
                return skip(Reason::ProducerUnknown, &levels[at].path, unknown);
            }
            Err((path, e)) => return refused(&path, "read", &e),
        }
    }
    if let Some(at) = project_at
        && levels[at].above_root
    {
        return skip(
            Reason::ProducerUnknown,
            &levels[at].path,
            format!(
                "the nearest project ({}) lies above the source root {}: the project extends \
                 beyond what diskgc may walk",
                levels[at].marker.as_deref().unwrap_or_default(),
                found.root.display()
            ),
        );
    }
    if project_at.is_none() && root_mount.fs_root != Path::new("/") {
        return skip(
            Reason::ProducerUnknown,
            &found.root,
            format!(
                "the project climb reached the root of a bind-mounted subdirectory ({} of its \
                 filesystem, mount {}), which can hide the real workspace root above it",
                root_mount.fs_root.display(),
                root_mount.id
            ),
        );
    }
    let project_at = project_at.unwrap_or(last);
    Ok(Ok(Located {
        levels,
        project_at,
        mount_id,
        root_mount,
        mounts,
    }))
}

/// The candidate's instance check and project ([`locate`]), the project's
/// walk with its dependency directories, and rules 1 and 2 (see the
/// module docs). `parent` is the candidate's parent directory (from the
/// judgement that walked it); `idle` the term's idle bound.
#[allow(clippy::too_many_arguments)]
pub(crate) fn gather<H: Host>(
    host: &H,
    contract: LivenessContract,
    found: &Found,
    parent: &H::Fd,
    record: &WalkRecord,
    idle: Option<IdleFor>,
    max_entries: usize,
    deadline: Option<MonoTime>,
) -> Judged<Tree> {
    let name = contract_name(contract);
    let skip = |reason, at: &Path, detail: String| {
        Ok(Err(Skip::new(
            reason,
            at.to_owned(),
            format!("liveness_contract {name}: {detail}"),
        )))
    };
    let Located {
        levels,
        project_at,
        mount_id,
        root_mount,
        mounts,
    } = judged!(locate(host, contract, found, parent, deadline));
    let candidate_name = found.path.file_name().expect("a candidate has a name");
    let project = &levels[project_at];
    let walked = match walk(
        host,
        contract,
        &project.fd,
        &project.stat,
        &project.path,
        mount_id,
        candidate_name,
        &mounts,
        max_entries,
        deadline,
    )? {
        Ok(walked) => walked,
        Err(skip) => {
            return Ok(Err(Skip::new(
                skip.reason,
                skip.path,
                format!(
                    "liveness_contract {name}: the project walk: {}",
                    skip.detail
                ),
            )));
        }
    };
    if !walked.dependency_dirs.contains(&found.identity.id) {
        return skip(
            Reason::Changed,
            &found.path,
            "the project walk did not meet the candidate it was judged for".to_owned(),
        );
    }
    // Rule 2's scope (PENDING Mark's confirmation): reads in the
    // project's dependency directories of the candidate's name, unless one
    // of them links out of itself (an npm link, an editable install),
    // whose packages are read where they are: then the whole project's.
    let (newest_read, scope) = match &walked.links_out {
        None => (
            walked.dependency_read.clone(),
            format!(
                "reads counted in the project's {} directories",
                candidate_name.display()
            ),
        ),
        Some(link) => (
            walked
                .dependency_read
                .clone()
                .max(walked.project_read.clone()),
            format!("reads counted project-wide: {link}, so its packages are read outside it"),
        ),
    };
    let activity = Activity {
        newest_change: walked.newest_change.clone(),
        newest_read,
        noatime: root_mount.noatime,
    };
    if let Err(detail) = crate::liveness::project::idle(&activity, idle, host.wall_clock()) {
        return skip(
            Reason::NotIdle,
            &found.path,
            format!(
                "{detail}; project {}{}; {scope}",
                project.path.display(),
                project
                    .marker
                    .as_ref()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default()
            ),
        );
    }
    let fs_path = match within_filesystem(&root_mount, &project.path) {
        Some(p) => p,
        None => {
            return skip(
                Reason::ProducerUnknown,
                &project.path,
                format!(
                    "its path within its filesystem cannot be told from mount {}",
                    root_mount.id
                ),
            );
        }
    };
    let mut dirs = walked.dirs;
    let mut devices = walked.devices;
    for entry in record.all_entries() {
        devices.insert(entry.identity.id.dev);
        if entry.identity.file_type == FileType::Directory {
            dirs.insert(entry.identity.id);
        }
    }
    Ok(Ok(Tree {
        path: project.path.clone(),
        fs_path,
        mount_dev: root_mount.dev,
        fs_type: root_mount.fs_type.clone(),
        devices,
        dirs,
        objects: walked.objects,
    }))
}

/// `path` (on `mount`, in diskgc's view) as a path within its filesystem:
/// the mount's root field and the rest after its mount point.
fn within_filesystem(mount: &MountEntry, path: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix(&mount.mount_point).ok()?;
    Some(if rest.as_os_str().is_empty() {
        mount.fs_root.clone()
    } else {
        mount.fs_root.join(rest)
    })
}

/// Whether `dir` holds a regular file `name` (`statx`, not followed).
fn regular_present<H: Host>(host: &H, dir: &H::Fd, name: &str) -> Result<bool, HostError> {
    match host.statx(Base::Dir(dir), Path::new(name)) {
        Ok(stat) => Ok(stat.file_type == Some(FileType::Regular)),
        Err(e) if e.is(Errno::ENOENT) || e.is(Errno::ENOTDIR) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether `dir` has an entry `name` of any type (`statx`, not followed).
fn any_present<H: Host>(host: &H, dir: &H::Fd, name: &str) -> Result<bool, HostError> {
    match host.statx(Base::Dir(dir), Path::new(name)) {
        Ok(_) => Ok(true),
        Err(e) if e.is(Errno::ENOENT) || e.is(Errno::ENOTDIR) => Ok(false),
        Err(e) => Err(e),
    }
}

/// What makes `dir` (at `path`) a project: a version-control top level
/// (any entry `.git`, `.hg`, `.svn`) or, for `node-project`, a workspace
/// root (`pnpm-workspace.yaml`, or a `package.json` with `workspaces`).
/// `Ok(Err)`: a workspace manifest that cannot be judged (over 1 MiB, not
/// a JSON object): `producer_unknown`. `Err`: a failed read.
#[allow(clippy::type_complexity)]
fn marker<H: Host>(
    host: &H,
    contract: LivenessContract,
    dir: &H::Fd,
    path: &Path,
) -> Result<Result<Option<String>, String>, (PathBuf, HostError)> {
    for vcs in VCS_METADATA {
        if any_present(host, dir, vcs).map_err(|e| (path.join(vcs), e))? {
            return Ok(Ok(Some(format!("version-control top level, {vcs}"))));
        }
    }
    if contract != LivenessContract::NodeProject {
        return Ok(Ok(None));
    }
    if regular_present(host, dir, PNPM_WORKSPACE).map_err(|e| (path.join(PNPM_WORKSPACE), e))? {
        return Ok(Ok(Some(format!("workspace root, {PNPM_WORKSPACE}"))));
    }
    let manifest = path.join(PACKAGE_JSON);
    let stat = match host.statx(Base::Dir(dir), Path::new(PACKAGE_JSON)) {
        Ok(stat) if stat.file_type == Some(FileType::Regular) => stat,
        Ok(_) => return Ok(Ok(None)),
        Err(e) if e.is(Errno::ENOENT) => return Ok(Ok(None)),
        Err(e) => return Err((manifest, e)),
    };
    if stat
        .size
        .is_none_or(|size| size > WORKSPACE_MANIFEST_MAX_BYTES as u64)
    {
        return Ok(Err(format!(
            "{} is over {WORKSPACE_MANIFEST_MAX_BYTES} bytes (or its size was not reported): \
             whether it declares workspaces is not read",
            manifest.display()
        )));
    }
    let Some(mount_id) = host
        .statx(Base::Dir(dir), Path::new(""))
        .map_err(|e| (path.to_owned(), e))?
        .mnt_id
    else {
        return Err((
            path.to_owned(),
            HostError::invalid("statx", path.display(), "no mount id reported"),
        ));
    };
    let fd = match open_beneath(
        host,
        dir,
        mount_id,
        Path::new(PACKAGE_JSON),
        Access::Path,
        Want::Any,
    ) {
        Ok((fd, again)) if again.dev_ino() == stat.dev_ino() => fd,
        Ok(_) => {
            return Ok(Err(format!(
                "{} was replaced while read",
                manifest.display()
            )));
        }
        Err(ResolveError::Host(e)) => return Err((manifest, e)),
        Err(e) => {
            return Ok(Err(format!(
                "{} cannot be reached: {e}",
                manifest.display()
            )));
        }
    };
    let content = host
        .read_head(&fd, WORKSPACE_MANIFEST_MAX_BYTES + 1)
        .map_err(|e| (manifest.clone(), e))?;
    if content.len() > WORKSPACE_MANIFEST_MAX_BYTES {
        return Ok(Err(format!(
            "{} grew over {WORKSPACE_MANIFEST_MAX_BYTES} bytes while read",
            manifest.display()
        )));
    }
    match crate::liveness::project::declares_workspaces(&content) {
        Ok(true) => Ok(Ok(Some(format!(
            "workspace root, {PACKAGE_JSON} workspaces"
        )))),
        Ok(false) => Ok(Ok(None)),
        Err(why) => Ok(Err(format!(
            "{} cannot be parsed ({why}): whether it declares workspaces is unknown",
            manifest.display()
        ))),
    }
}

/// Why a parent's manifest could not be told.
enum Listing {
    TooMany,
    Failed(HostError),
}

/// The ecosystem's manifest in the candidate's parent, as a regular file:
/// `package.json`; or a Python manifest (a fixed name, else a
/// `requirements*.txt` from a listing of the parent).
fn manifest<H: Host>(
    host: &H,
    contract: LivenessContract,
    parent: &H::Fd,
) -> Result<Option<OsString>, Listing> {
    let fixed: &[&str] = match contract {
        LivenessContract::NodeProject => &[PACKAGE_JSON],
        _ => &PYTHON_MANIFESTS,
    };
    for name in fixed {
        if regular_present(host, parent, name).map_err(Listing::Failed)? {
            return Ok(Some(OsString::from(name)));
        }
    }
    if contract == LivenessContract::NodeProject {
        return Ok(None);
    }
    let mut names = match host
        .read_dir(parent, PARENT_NAMES_MAX)
        .map_err(Listing::Failed)?
    {
        DirListing::Names(names) => names,
        DirListing::TooMany => return Err(Listing::TooMany),
    };
    names.sort();
    for name in names {
        if crate::liveness::project::is_requirements(name.as_bytes())
            && regular_present(host, parent, &name.to_string_lossy()).map_err(Listing::Failed)?
        {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// What the project walk found.
struct Walked {
    /// Rule 1: the newest change in the project and its dependency
    /// directories (a non-directory's mtime or ctime, a directory's birth
    /// time: removing entries is not activity, so diskgc's own deletions
    /// never make a project look used).
    newest_change: Newest,
    /// Rule 2, project-wide: the newest regular-file atime outside the
    /// dependency directories.
    project_read: Newest,
    /// Rule 2, narrowed: the newest regular-file atime inside the
    /// project's dependency directories of the candidate's name.
    dependency_read: Newest,
    dirs: HashSet<DevIno>,
    objects: HashSet<DevIno>,
    devices: HashSet<Dev>,
    /// The tops of the dependency directories met (the candidate among
    /// them).
    dependency_dirs: HashSet<DevIno>,
    /// The first link out of a dependency directory (an npm link, a
    /// `file:` dependency, an editable install), described.
    links_out: Option<String>,
}

/// Where in the project a listed directory lies.
#[derive(Clone)]
enum Part {
    /// Project content (VCS metadata and dependency directories are left
    /// out of it).
    Project,
    /// Inside the dependency directory at this path.
    Dependency(PathBuf),
}

/// The most bytes of a `.pth` file diskgc reads: a larger one is taken as
/// linking out (conservative).
const PTH_MAX_BYTES: usize = 64 * 1024;

/// Walks the project at `path` (open as `fd`, on mount `mount_id`) like a
/// candidate: fd-relative, sorted, no symlink followed, no mount crossed
/// (a mount under it adds its device and is neither entered nor
/// `statx`ed), bounded by `max_entries` (`too_many_entries`); leaving out
/// VCS metadata. Every directory named `candidate_name` (the candidate
/// among them) is a dependency directory: walked too, its reads counted
/// apart (rule 2's narrowed scope) and its symlinks and, for Python,
/// editable installs checked for links out of it. A failed listing or
/// `statx` is classified as the eligibility walk's are (`unreadable`,
/// `changed`).
#[allow(clippy::too_many_arguments)]
fn walk<H: Host>(
    host: &H,
    contract: LivenessContract,
    fd: &H::Fd,
    stat: &Stat,
    path: &Path,
    mount_id: u64,
    candidate_name: &OsStr,
    mounts: &[MountEntry],
    max_entries: usize,
    deadline: Option<MonoTime>,
) -> Judged<Walked> {
    let mut walk = ProjectWalk {
        out: Walked {
            newest_change: Newest::Known(None),
            project_read: Newest::Known(None),
            dependency_read: Newest::Known(None),
            dirs: HashSet::new(),
            objects: HashSet::new(),
            devices: HashSet::new(),
            dependency_dirs: HashSet::new(),
            links_out: None,
        },
        under: mounts
            .iter()
            .filter(|m| m.mount_point != path && m.mount_point.starts_with(path))
            .collect(),
        contract,
        mount_id,
        candidate_name,
        max_entries,
        entries: 1,
    };
    for m in &walk.under {
        walk.out.devices.insert(m.dev);
    }
    let Some(top) = stat.dev_ino() else {
        return Ok(Err(Skip::new(
            Reason::UnsupportedFs,
            path.to_owned(),
            "the kernel did not report its inode".to_owned(),
        )));
    };
    walk.out.dirs.insert(top);
    walk.out.objects.insert(top);
    walk.out.note(stat, path, &Part::Project);
    // Depth-first, sorted: the top, then each directory found, one open
    // descriptor per directory still to list.
    let mut todo = judged!(walk.list(host, fd, path, &Part::Project, deadline));
    while let Some((at, dir, part)) = todo.pop() {
        let below = judged!(walk.list(host, &dir, &at, &part, deadline));
        todo.extend(below);
    }
    Ok(Ok(walk.out))
}

/// The state of one project walk ([`walk`]).
struct ProjectWalk<'a> {
    out: Walked,
    /// Mounts whose mount point lies under the project.
    under: Vec<&'a MountEntry>,
    contract: LivenessContract,
    mount_id: u64,
    candidate_name: &'a OsStr,
    max_entries: usize,
    entries: usize,
}

impl Walked {
    /// Folds an entry's times into the newest change and read (see
    /// [`Walked::newest_change`]).
    fn note(&mut self, stat: &Stat, at: &Path, part: &Part) {
        self.devices.insert(stat.dev);
        let unknown = || Newest::Unknown(at.to_owned());
        let change = if stat.file_type == Some(FileType::Directory) {
            // Its birth time; where none is reported, its mtime or ctime
            // (which removing an entry also moves: only retains more).
            match (stat.btime, stat.mtime, stat.ctime) {
                (Some(b), _, _) => Newest::Known(Some((b, at.to_owned()))),
                (None, Some(m), Some(c)) => Newest::Known(Some((m.max(c), at.to_owned()))),
                _ => unknown(),
            }
        } else {
            match (stat.mtime, stat.ctime) {
                (Some(m), Some(c)) => Newest::Known(Some((m.max(c), at.to_owned()))),
                _ => unknown(),
            }
        };
        self.newest_change =
            std::mem::replace(&mut self.newest_change, Newest::Known(None)).max(change);
        if stat.file_type == Some(FileType::Regular) {
            let read = match stat.atime {
                Some(a) => Newest::Known(Some((a, at.to_owned()))),
                None => unknown(),
            };
            let slot = match part {
                Part::Project => &mut self.project_read,
                Part::Dependency(_) => &mut self.dependency_read,
            };
            *slot = std::mem::replace(slot, Newest::Known(None)).max(read);
        }
    }
}

/// `target` (a symlink's text in the directory `dir`) as a lexical
/// absolute path, `..` folded.
fn link_target(dir: &Path, target: &Path) -> Option<PathBuf> {
    let joined = if target.is_absolute() {
        target.to_owned()
    } else {
        dir.join(target)
    };
    crate::liveness::pytest::normalize(joined.as_os_str().as_bytes())
}

impl ProjectWalk<'_> {
    /// Lists the directory `dir` (at `at`, in `part`) and notes every
    /// entry; returns its subdirectories to walk, opened, in reverse name
    /// order (popped in order).
    fn list<H: Host>(
        &mut self,
        host: &H,
        dir: &H::Fd,
        at: &Path,
        part: &Part,
        deadline: Option<MonoTime>,
    ) -> Judged<Vec<(PathBuf, H::Fd, Part)>> {
        check_deadline(host, deadline, at)?;
        let mut names = match host.read_dir(dir, self.max_entries.saturating_sub(self.entries)) {
            Ok(DirListing::Names(names)) => names,
            Ok(DirListing::TooMany) => {
                return Ok(Err(Skip::new(
                    Reason::TooManyEntries,
                    at.to_owned(),
                    format!("the project has more than {} entries", self.max_entries),
                )));
            }
            Err(e) => return skip_or_stop(at.to_owned(), "list", &e).map(Err),
        };
        names.sort();
        let mut below: Vec<(PathBuf, H::Fd, Part)> = Vec::new();
        for name in names {
            let child = at.join(&name);
            if (matches!(part, Part::Project) && VCS_METADATA.iter().any(|v| name == *v))
                || self.under.iter().any(|m| m.mount_point == child)
            {
                continue;
            }
            let stat = match host.statx(Base::Dir(dir), Path::new(&name)) {
                Ok(stat) => stat,
                Err(e) => return skip_or_stop(child, "stat", &e).map(Err),
            };
            if stat.mnt_id != Some(self.mount_id) {
                // A mount the table did not show under it (a race): its
                // device counts, its content is not walked.
                self.out.devices.insert(stat.dev);
                continue;
            }
            self.entries += 1;
            let Some(id) = stat.dev_ino() else {
                return Ok(Err(Skip::new(
                    Reason::UnsupportedFs,
                    child,
                    "the kernel did not report its inode".to_owned(),
                )));
            };
            let is_dir = stat.file_type == Some(FileType::Directory);
            let child_part = match part {
                Part::Project if is_dir && name == self.candidate_name => {
                    self.out.dependency_dirs.insert(id);
                    Part::Dependency(child.clone())
                }
                other => other.clone(),
            };
            self.out.objects.insert(id);
            self.out.note(&stat, &child, &child_part);
            if let Part::Dependency(top) = &child_part
                && self.out.links_out.is_none()
            {
                self.out.links_out = judged!(self.link_out(host, dir, at, &name, &stat, top));
            }
            if !is_dir {
                continue;
            }
            self.out.dirs.insert(id);
            match open_beneath(
                host,
                dir,
                self.mount_id,
                Path::new(&name),
                Access::Path,
                Want::Directory,
            ) {
                Ok((sub, again)) if again.dev_ino() == Some(id) => {
                    below.push((child, sub, child_part));
                }
                Ok(_) => {
                    return Ok(Err(Skip::new(
                        Reason::Changed,
                        child,
                        "replaced while the project was walked".to_owned(),
                    )));
                }
                Err(ResolveError::Host(e)) => return skip_or_stop(child, "open", &e).map(Err),
                Err(e) => {
                    return Ok(Err(Skip::new(
                        Reason::Changed,
                        child,
                        format!("changed while the project was walked: {e}"),
                    )));
                }
            }
        }
        below.reverse();
        Ok(Ok(below))
    }

    /// Whether the entry `name` (in `dir` at `at`, inside the dependency
    /// directory `top`) links out of it, described: a symlink whose target
    /// lies outside `top` (an npm or yarn link, a `file:` dependency; a
    /// virtual environment's `bin/python*` link to its interpreter is not
    /// a package), and for Python an editable install (`__editable__*`,
    /// `*.egg-link`, or a `.pth` line naming a directory outside `top`).
    fn link_out<H: Host>(
        &self,
        host: &H,
        dir: &H::Fd,
        at: &Path,
        name: &OsStr,
        stat: &Stat,
        top: &Path,
    ) -> Judged<Option<String>> {
        let child = at.join(name);
        let python = self.contract == LivenessContract::PythonProject;
        let bytes = name.as_bytes();
        if stat.file_type == Some(FileType::Symlink) {
            if python && at == top.join("bin") && bytes.starts_with(b"python") {
                return Ok(Ok(None));
            }
            let target = match host.read_link(dir, name) {
                Ok(target) => target,
                Err(e) => return skip_or_stop(child, "read the link", &e).map(Err),
            };
            return Ok(Ok(match link_target(at, &target) {
                Some(real) if real.starts_with(top) => None,
                _ => Some(format!(
                    "{} links to {} outside {}",
                    child.display(),
                    target.display(),
                    top.display()
                )),
            }));
        }
        if !python || stat.file_type != Some(FileType::Regular) {
            return Ok(Ok(None));
        }
        if bytes.starts_with(b"__editable__") || bytes.ends_with(b".egg-link") {
            return Ok(Ok(Some(format!(
                "{} is an editable install",
                child.display()
            ))));
        }
        if !bytes.ends_with(b".pth") {
            return Ok(Ok(None));
        }
        if stat.size.is_none_or(|size| size > PTH_MAX_BYTES as u64) {
            return Ok(Ok(Some(format!(
                "{} is over {PTH_MAX_BYTES} bytes, so whether it adds a path outside is not read",
                child.display()
            ))));
        }
        let fd = match open_beneath(
            host,
            dir,
            self.mount_id,
            Path::new(name),
            Access::Path,
            Want::Any,
        ) {
            Ok((fd, again)) if again.dev_ino() == stat.dev_ino() => fd,
            Ok(_) => {
                return Ok(Err(Skip::new(
                    Reason::Changed,
                    child,
                    "replaced while the project was walked".to_owned(),
                )));
            }
            Err(ResolveError::Host(e)) => return skip_or_stop(child, "open", &e).map(Err),
            Err(e) => {
                return Ok(Err(Skip::new(
                    Reason::Changed,
                    child,
                    format!("changed while the project was walked: {e}"),
                )));
            }
        };
        let content = match host.read_head(&fd, PTH_MAX_BYTES + 1) {
            Ok(content) => content,
            Err(e) => return skip_or_stop(child, "read", &e).map(Err),
        };
        // site.py: blank lines and `#` comments are skipped, `import`
        // lines run; every other line is a directory added to sys.path.
        for line in content.split(|b| *b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let line = line.trim_ascii_end();
            if line.is_empty()
                || line.starts_with(b"#")
                || line.starts_with(b"import ")
                || line.starts_with(b"import\t")
            {
                continue;
            }
            let entry = Path::new(OsStr::from_bytes(line));
            match link_target(at, entry) {
                Some(real) if real.starts_with(top) => {}
                _ => {
                    return Ok(Ok(Some(format!(
                        "{} adds {} to sys.path, outside {}",
                        child.display(),
                        entry.display(),
                        top.display()
                    ))));
                }
            }
        }
        Ok(Ok(None))
    }
}

/// Rule 3: whether a visible process uses the project `tree` (see the
/// module docs), judged from `scan` (made with the project facts) and
/// diskgc's mount table `mounts`. `Err`: `in_use`, naming the process and
/// how; a held file or directory on one of the project's devices that
/// cannot be placed retains too.
pub(crate) fn visible_use(
    scan: &Scan,
    tree: &Tree,
    candidate: &Path,
    mounts: &[MountEntry],
    name: &str,
) -> Result<Result<(), Skip>, String> {
    let held = scan
        .held()
        .ok_or_else(|| "the process scan was made without the project facts".to_owned())?;
    let in_use = |pid: crate::host::Pid, evidence, detail: String| {
        Err(Skip::new(
            Reason::InUse,
            candidate.to_owned(),
            format!(
                "liveness_contract {name}: pid {pid} ({}) uses the project {}: {detail}",
                scan.command(pid),
                tree.path.display()
            ),
        )
        .with_process_blockers(vec![ProcessBlocker {
            pid: pid.0,
            command: scan.command_option(pid),
            evidence,
        }]))
    };
    let evidence = |how: How| match how {
        How::Fd(_) => ProcessEvidence::Fd,
        How::Cwd => ProcessEvidence::Cwd,
        How::Root => ProcessEvidence::Root,
        How::Exe => ProcessEvidence::Exe,
        How::Map => ProcessEvidence::Map,
    };
    // The first holder by pid, then link: reports do not depend on hash
    // order.
    let mut first: Option<(crate::host::Pid, How, String)> = None;
    let mut consider = |pid, how, detail: String| {
        if first.as_ref().is_none_or(|(p, h, _)| (pid, how) < (*p, *h)) {
            first = Some((pid, how, detail));
        }
    };
    for id in &tree.objects {
        if let Some(u) = scan.uses(*id).first() {
            consider(u.pid, u.how, format!("it has {id} {}", u.how));
        }
        if let Some(pid) = scan.mapped_by_inode(&tree.fs_type, id.ino) {
            consider(
                pid,
                How::Map,
                format!("it maps an object with inode number {}", id.ino),
            );
        }
    }
    for id in &tree.dirs {
        if let Some(u) = held.holder_above(*id) {
            consider(
                u.pid,
                u.how,
                format!("a directory it holds {} is at or below {id}", u.how),
            );
        }
    }
    for failed in &held.failed {
        if tree.devices.contains(&failed.id.dev) {
            consider(
                failed.pid,
                failed.how,
                format!(
                    "a directory it holds {} on the project's device could not be climbed \
                     ({}), so it may lie in the project",
                    failed.how, failed.why
                ),
            );
        }
    }
    if let Some((pid, how, detail)) = first {
        return Ok(in_use(pid, evidence(how), detail));
    }
    if let Some(e) = scan.environ_naming(&tree.path) {
        return Ok(in_use(
            e.pid,
            ProcessEvidence::Environment,
            format!(
                "its initial environment names it ({}={})",
                e.var.to_string_lossy(),
                e.path.display()
            ),
        ));
    }
    match argv_naming(&held.argv, &tree.path) {
        None => {}
        Some((pid, Ok(arg))) => {
            return Ok(in_use(
                pid,
                ProcessEvidence::CommandLine,
                format!(
                    "its command line names a path in it ({})",
                    Path::new(arg).display()
                ),
            ));
        }
        Some((pid, Err(why))) => {
            return Ok(in_use(
                pid,
                ProcessEvidence::Unreadable,
                format!("its command line could not be read ({why}), so it may name the project"),
            ));
        }
    }
    for (pid, file) in &held.files {
        if !tree.devices.contains(&file.id.dev) || tree.objects.contains(&file.id) {
            continue;
        }
        let text = file.text.as_bytes();
        let text = Path::new(OsStr::from_bytes(
            text.strip_suffix(b" (deleted)").unwrap_or(text),
        ));
        let mount = file
            .mnt_id
            .and_then(|id| mounts.iter().find(|m| m.id == id));
        let Some(mount) = mount else {
            // Through a mount outside diskgc's table (another mount
            // namespace): translated through the process's own table.
            let foreign = held.foreign.get(pid);
            let translated = match (file.mnt_id, foreign) {
                (None, _) => Err("its mount id was not reported".to_owned()),
                (_, None) => Err("its mount table was not read".to_owned()),
                (Some(id), Some(foreign)) => match &foreign.mounts {
                    Err(why) => Err(format!("its mount table could not be read: {why}")),
                    Ok(table) => translate(text, foreign.root.as_deref(), table, id),
                },
            };
            match translated {
                Ok((dev, fs_path)) if dev == tree.mount_dev => {
                    if fs_path.starts_with(&tree.fs_path) {
                        return Ok(in_use(
                            *pid,
                            evidence(file.how),
                            format!(
                                "it holds {} ({}) in another mount namespace, translated \
                                 through its mount table into the project ({})",
                                text.display(),
                                file.how,
                                fs_path.display()
                            ),
                        ));
                    }
                    continue;
                }
                Ok((dev, _)) => {
                    return Ok(in_use(
                        *pid,
                        evidence(file.how),
                        format!(
                            "it holds {} ({}) on the project's device through a mount of \
                             filesystem {dev} in another mount namespace, not the project's \
                             {}: it cannot be translated",
                            text.display(),
                            file.how,
                            tree.mount_dev
                        ),
                    ));
                }
                Err(why) => {
                    return Ok(in_use(
                        *pid,
                        evidence(file.how),
                        format!(
                            "it holds {} ({}) on the project's device through a mount not in \
                             diskgc's mount table (another mount namespace, or detached), which \
                             cannot be translated: {why}",
                            text.display(),
                            file.how
                        ),
                    ));
                }
            }
        };
        if mount.mount_point != tree.path && mount.mount_point.starts_with(&tree.path) {
            return Ok(in_use(
                *pid,
                evidence(file.how),
                format!(
                    "it holds {} ({}) on a mount inside the project",
                    text.display(),
                    file.how
                ),
            ));
        }
        let Some(fs_path) = within_filesystem(mount, text) else {
            return Ok(in_use(
                *pid,
                evidence(file.how),
                format!(
                    "it holds {} ({}) whose text is not below its mount point {}, which cannot \
                     be translated",
                    text.display(),
                    file.how,
                    mount.mount_point.display()
                ),
            ));
        };
        if mount.dev != tree.mount_dev {
            // On a device the project contains (through a mount under it)
            // but another filesystem than the project's: the project's
            // path within its filesystem cannot place it, as for a
            // foreign mount (Codex review of TASK-79).
            return Ok(in_use(
                *pid,
                evidence(file.how),
                format!(
                    "it holds {} ({}) on filesystem {} through mount {}, a device the project \
                     holds through a mount under it, which the project's path cannot place",
                    text.display(),
                    file.how,
                    mount.dev,
                    mount.id
                ),
            ));
        }
        if fs_path.starts_with(&tree.fs_path) {
            return Ok(in_use(
                *pid,
                evidence(file.how),
                format!(
                    "it holds {} ({}), inside the project in its filesystem",
                    text.display(),
                    file.how
                ),
            ));
        }
    }
    Ok(Ok(()))
}

/// The first process whose command line names a path at or below
/// `project` (lexically, `..` folded): each element, and the value of a
/// `--flag=VALUE` element; a relative one placed against each of the
/// process's working-directory link texts. A consumer started from
/// outside the project (`node ~/p/server.js`, `p/.venv/bin/python
/// p/server.py`) with its modules cached holds nothing in it. `Err` on
/// the first process whose command line could not be read: it retains.
fn argv_naming<'a>(
    argv: &'a std::collections::BTreeMap<crate::host::Pid, crate::inuse::held::Argv>,
    project: &Path,
) -> Option<(crate::host::Pid, Result<&'a OsStr, &'a str>)> {
    let inside = |text: &[u8], cwds: &[PathBuf]| -> bool {
        if text.is_empty() {
            return false;
        }
        let candidates: Vec<Option<PathBuf>> = if text.first() == Some(&b'/') {
            vec![crate::liveness::pytest::normalize(text)]
        } else {
            cwds.iter()
                .map(|cwd| {
                    let mut joined = cwd.as_os_str().as_bytes().to_vec();
                    joined.push(b'/');
                    joined.extend_from_slice(text);
                    crate::liveness::pytest::normalize(&joined)
                })
                .collect()
        };
        candidates
            .into_iter()
            .flatten()
            .any(|path| path.starts_with(project))
    };
    for (pid, (args, cwds)) in argv {
        let args = match args {
            Ok(args) => args,
            Err(why) => return Some((*pid, Err(why.as_str()))),
        };
        for arg in args {
            let bytes = arg.as_bytes();
            let value = bytes
                .starts_with(b"-")
                .then(|| {
                    bytes
                        .iter()
                        .position(|b| *b == b'=')
                        .map(|at| &bytes[at + 1..])
                })
                .flatten();
            if inside(bytes, cwds) || value.is_some_and(|v| inside(v, cwds)) {
                return Some((*pid, Ok(arg.as_os_str())));
            }
        }
    }
    None
}

/// A held file's link text (`text`, in diskgc's frame, ` (deleted)`
/// removed) through a mount of another namespace, as (that mount's
/// filesystem device, the path within it), by the process's own mount
/// table (docs/safety.md, A process's paths): unless the process's root
/// link text is `/`, it is removed from the front; the line whose id is
/// the object's mount id gives the mount point (relative to the process's
/// root) and the root field; the rest of the text after the mount point,
/// appended to the root field, is the path within the filesystem. `Err`:
/// untranslatable (no root text, no such id, text not below them).
pub(crate) fn translate(
    text: &Path,
    root: Option<&OsStr>,
    table: &[MountEntry],
    mnt_id: u64,
) -> Result<(Dev, PathBuf), String> {
    let root = Path::new(root.ok_or("its root link text was not read")?);
    let in_view = if root == Path::new("/") {
        text.to_owned()
    } else {
        let rest = text.strip_prefix(root).map_err(|_| {
            format!(
                "{} does not start with its root {}",
                text.display(),
                root.display()
            )
        })?;
        Path::new("/").join(rest)
    };
    let mount = table
        .iter()
        .find(|m| m.id == mnt_id)
        .ok_or_else(|| format!("mount id {mnt_id} is not in its mount table"))?;
    let rest = in_view.strip_prefix(&mount.mount_point).map_err(|_| {
        format!(
            "{} is not below its mount point {}",
            in_view.display(),
            mount.mount_point.display()
        )
    })?;
    Ok((
        mount.dev,
        if rest.as_os_str().is_empty() {
            mount.fs_root.clone()
        } else {
            mount.fs_root.join(rest)
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::translate;
    use crate::host::{Dev, MountEntry};
    use std::path::{Path, PathBuf};

    fn mount(id: u64, minor: u32, fs_root: &str, at: &str) -> MountEntry {
        MountEntry {
            id,
            parent_id: 1,
            dev: Dev { major: 254, minor },
            fs_root: fs_root.into(),
            mount_point: at.into(),
            fs_type: "ext4".into(),
            source: "/dev/x".into(),
            read_only: false,
            idmapped: false,
            noatime: false,
            upper_dir: None,
        }
    }

    /// A rootless container's pause process (the development host,
    /// 2026-11-02): its executable on the store, through its own copy of
    /// the root mount, is outside a project in /home.
    #[test]
    fn a_container_processes_store_executable_is_translated_outside() {
        let table = [mount(636, 0, "/", "/")];
        let (dev, path) = translate(
            Path::new("/nix/store/x-podman/bin/.podman-wrapped"),
            Some("/".as_ref()),
            &table,
            636,
        )
        .expect("translated");
        assert_eq!(
            dev,
            Dev {
                major: 254,
                minor: 0
            }
        );
        assert_eq!(
            path,
            PathBuf::from("/nix/store/x-podman/bin/.podman-wrapped")
        );
    }

    /// The tree bound at /work in a container, and the same inside a
    /// chroot to /jail (link text in diskgc's frame starts with the root).
    #[test]
    fn a_bind_into_a_container_and_a_chroot_inside_it_translate_into_the_tree() {
        let table = [mount(900, 2, "/", "/"), mount(968, 2, "/tester/p", "/work")];
        assert_eq!(
            translate(Path::new("/work/src/a.js"), Some("/".as_ref()), &table, 968),
            Ok((
                Dev {
                    major: 254,
                    minor: 2
                },
                PathBuf::from("/tester/p/src/a.js")
            ))
        );
        assert_eq!(
            translate(
                Path::new("/jail/work/src/a.js"),
                Some("/jail".as_ref()),
                &table,
                968
            ),
            Ok((
                Dev {
                    major: 254,
                    minor: 2
                },
                PathBuf::from("/tester/p/src/a.js")
            ))
        );
        // A path with a space, as mountinfo's parser has unescaped it.
        let spaced = [mount(970, 2, "/tester/my proj", "/work dir")];
        assert_eq!(
            translate(
                Path::new("/work dir/src/a.js"),
                Some("/".as_ref()),
                &spaced,
                970
            ),
            Ok((
                Dev {
                    major: 254,
                    minor: 2
                },
                PathBuf::from("/tester/my proj/src/a.js")
            ))
        );
    }

    #[test]
    fn what_cannot_be_placed_is_untranslatable() {
        let table = [mount(968, 2, "/tester/p", "/work")];
        let root = Some("/".as_ref());
        assert!(translate(Path::new("/work/a"), root, &table, 1).is_err());
        assert!(translate(Path::new("/elsewhere/a"), root, &table, 968).is_err());
        assert!(translate(Path::new("/work/a"), None, &table, 968).is_err());
        assert!(
            translate(
                Path::new("/other/work/a"),
                Some("/jail".as_ref()),
                &table,
                968
            )
            .is_err()
        );
        // Component-wise: /workx is not below /work.
        assert!(translate(Path::new("/workx/a"), root, &table, 968).is_err());
    }
}
