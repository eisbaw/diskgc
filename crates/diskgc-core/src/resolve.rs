//! Path traversal (docs/safety.md, Path traversal): reaching a configured
//! path, and descending below an open directory, without following a
//! symlink or crossing a mount.
//!
//! [`resolve`] reaches an absolute path:
//!
//! 1. pick the deepest mount point that is a path prefix in mountinfo
//!    ([`mount_for`]; stacked mounts: the topmost);
//! 2. open it (`O_PATH | O_DIRECTORY | O_NOFOLLOW`, `RESOLVE_NO_SYMLINKS`)
//!    and confirm with `statx` that it is that mount: `STATX_ATTR_MOUNT_ROOT`
//!    set and `stx_mnt_id` equal to the mountinfo id;
//! 3. open the rest relative to it with [`open_beneath`] (`.` if the path is
//!    the mount point).
//!
//! [`open_beneath`] is step 3 on its own, also used to descend from an open
//! directory: `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
//! RESOLVE_NO_XDEV)`, then a `statx` confirming the object is still on the
//! starting mount.
//!
//! A check the kernel does not answer (no mount id, no mount-root attribute
//! in the masks) is `unsupported_fs`, never assumed.

use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::engine::{Reason, may_be_networked, path_in_private_tmp};
use crate::host::{
    Access, Base, Errno, FileType, Host, HostError, MountEntry, OpenHow, ResolveFlags, Stat,
};

/// What the resolved object must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// A directory (`O_DIRECTORY`): roots, candidates that are directories.
    Directory,
    /// Any non-symlink object.
    Any,
}

/// A path reached safely: an `O_PATH` fd, the mount it is on, and its stat.
#[derive(Debug)]
pub struct Resolved<F> {
    pub fd: F,
    pub mount: MountEntry,
    pub stat: Stat,
}

/// Why a path is unsafe to use (`unsafe_path`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsafe {
    /// Not an absolute path.
    NotAbsolute,
    /// Holds `.` or `..` components (or, below a directory, is absolute).
    NotNormal,
    /// The kernel refused a symlink on the way (`ELOOP` under
    /// `RESOLVE_NO_SYMLINKS`).
    Symlink,
    /// The kernel refused a mount crossing (`EXDEV` under `RESOLVE_NO_XDEV`
    /// or `RESOLVE_BENEATH`).
    CrossesMount,
    /// No mount in mountinfo contains the path.
    NoMount,
    /// Several mounts at the deepest mount point, none clearly on top.
    AmbiguousMounts(Vec<u64>),
    /// The directory at the mount point is not the mount mountinfo names
    /// (shadowed by a later mount, or the table changed meanwhile).
    MountMismatch {
        expected: u64,
        found: u64,
        mount_root: bool,
    },
    /// The opened object is on another mount than the one it was opened
    /// below.
    LeftMount { expected: u64, found: u64 },
}

impl fmt::Display for Unsafe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAbsolute => f.write_str("not an absolute path"),
            Self::NotNormal => f.write_str("contains '.' or '..' components"),
            Self::Symlink => f.write_str("a symlink on the way (ELOOP)"),
            Self::CrossesMount => f.write_str("crosses a mount (EXDEV)"),
            Self::NoMount => f.write_str("no mount in mountinfo contains it"),
            Self::AmbiguousMounts(ids) => {
                write!(f, "stacked mounts {ids:?} with no single topmost one")
            }
            Self::MountMismatch {
                expected,
                found,
                mount_root,
            } => write!(
                f,
                "mount point is mount id {found} (mount root: {mount_root}), mountinfo says {expected}"
            ),
            Self::LeftMount { expected, found } => {
                write!(
                    f,
                    "opened object is on mount id {found}, expected {expected}"
                )
            }
        }
    }
}

/// Why a path could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// `unsafe_path`: symlinks, mount crossings, odd mount tables.
    UnsafePath { path: PathBuf, why: Unsafe },
    /// `unsupported_fs`: the kernel cannot answer a check this needs.
    UnsupportedFs {
        path: PathBuf,
        missing: &'static str,
    },
    /// `missing`: the path (or a directory on it) does not exist.
    Missing { path: PathBuf, errno: Errno },
    /// Anything else (permissions, I/O errors, an unreadable mount table).
    Host(HostError),
}

impl ResolveError {
    /// The skip reason (docs/reference.md, JSON output); `None` for a
    /// host error, which is an error of the run, never a skip.
    pub fn skip_reason(&self) -> Option<Reason> {
        match self {
            Self::UnsafePath { .. } => Some(Reason::UnsafePath),
            Self::UnsupportedFs { .. } => Some(Reason::UnsupportedFs),
            Self::Missing { .. } => Some(Reason::Missing),
            Self::Host(_) => None,
        }
    }

    /// [`Self::skip_reason`]'s name, or `error`: a label for diagnostic
    /// transcripts (the container driver's), never a report value (a
    /// report's reason is a [`Reason`]).
    pub fn transcript_label(&self) -> &'static str {
        self.skip_reason().map_or("error", Reason::as_str)
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath { path, why } => {
                write!(f, "unsafe_path: {}: {why}", path.display())
            }
            Self::UnsupportedFs { path, missing } => write!(
                f,
                "unsupported_fs: {}: the kernel does not report {missing} here",
                path.display()
            ),
            Self::Missing { path, errno } => write!(f, "missing: {}: {errno}", path.display()),
            Self::Host(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// The mount a path is on: the deepest mount point that is a (component-
/// wise) prefix of `path`; of mounts stacked on that point, the topmost (the
/// one no other mount there is stacked on).
pub fn mount_for<'m>(mounts: &'m [MountEntry], path: &Path) -> Result<&'m MountEntry, Unsafe> {
    let depth = |m: &MountEntry| m.mount_point.components().count();
    let deepest = mounts
        .iter()
        .filter(|m| path.starts_with(&m.mount_point))
        .map(depth)
        .max()
        .ok_or(Unsafe::NoMount)?;
    let at_point: Vec<&MountEntry> = mounts
        .iter()
        .filter(|m| path.starts_with(&m.mount_point) && depth(m) == deepest)
        .collect();
    let topmost: Vec<&MountEntry> = at_point
        .iter()
        .copied()
        .filter(|m| !at_point.iter().any(|other| other.parent_id == m.id))
        .collect();
    match topmost.as_slice() {
        [one] => Ok(one),
        _ => Err(Unsafe::AmbiguousMounts(
            at_point.iter().map(|m| m.id).collect(),
        )),
    }
}

/// A reason to refuse a root before opening it or any ancestor. A local
/// bind mount at the final path does not make an unsafe parent traversable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountScreenError {
    Unsafe {
        at: PathBuf,
        why: Unsafe,
    },
    Networked {
        at: PathBuf,
        mount_point: PathBuf,
        fs_type: String,
    },
    PrivateTmp {
        at: PathBuf,
        mount_point: PathBuf,
        fs_root: PathBuf,
    },
}

impl fmt::Display for MountScreenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsafe { at, why } => write!(f, "mount at {} is unsafe: {why}", at.display()),
            Self::Networked {
                at,
                mount_point,
                fs_type,
            } => write!(
                f,
                "ancestor {} is on {} mount {} and may block on I/O",
                at.display(),
                fs_type,
                mount_point.display()
            ),
            Self::PrivateTmp {
                at,
                mount_point,
                fs_root,
            } => write!(
                f,
                "ancestor {} is on mount {} bound from systemd PrivateTmp root {}",
                at.display(),
                mount_point.display(),
                fs_root.display()
            ),
        }
    }
}

/// Screen the effective mount at every path prefix from the root down to
/// `path`, using mountinfo only. The caller may then safely try to resolve
/// the root; a local bind mount below NFS/FUSE or PrivateTmp still fails.
pub fn screen_mount_chain(mounts: &[MountEntry], path: &Path) -> Result<(), MountScreenError> {
    for at in path.ancestors() {
        let mount = mount_for(mounts, at).map_err(|why| MountScreenError::Unsafe {
            at: at.to_owned(),
            why,
        })?;
        if may_be_networked(&mount.fs_type) {
            return Err(MountScreenError::Networked {
                at: at.to_owned(),
                mount_point: mount.mount_point.clone(),
                fs_type: mount.fs_type.clone(),
            });
        }
        if path_in_private_tmp(&mount.fs_root) {
            return Err(MountScreenError::PrivateTmp {
                at: at.to_owned(),
                mount_point: mount.mount_point.clone(),
                fs_root: mount.fs_root.clone(),
            });
        }
    }
    Ok(())
}

/// `openat2` arguments for opening a mount point (step 2).
pub fn mount_point_how() -> OpenHow {
    OpenHow {
        access: Access::Path,
        directory: true,
        nofollow: true,
        resolve: ResolveFlags {
            beneath: false,
            no_symlinks: true,
            no_xdev: false,
        },
    }
}

/// `openat2` arguments for descending below an open directory (step 3, and
/// the walk). No `O_NOFOLLOW`: with it, `openat2` would return an fd *to* a
/// trailing symlink; without it, `RESOLVE_NO_SYMLINKS` refuses it (`ELOOP`).
pub fn beneath_how(access: Access, want: Want) -> OpenHow {
    OpenHow {
        access,
        directory: want == Want::Directory,
        nofollow: false,
        resolve: ResolveFlags {
            beneath: true,
            no_symlinks: true,
            no_xdev: true,
        },
    }
}

fn require<T>(value: Option<T>, path: &Path, missing: &'static str) -> Result<T, ResolveError> {
    value.ok_or_else(|| ResolveError::UnsupportedFs {
        path: path.to_owned(),
        missing,
    })
}

fn unsafe_path(path: &Path, why: Unsafe) -> ResolveError {
    ResolveError::UnsafePath {
        path: path.to_owned(),
        why,
    }
}

/// Maps an `openat2` failure on `path` to its skip reason.
fn open_error(path: &Path, e: HostError) -> ResolveError {
    match e.kernel_errno() {
        Some(Errno::ELOOP) => unsafe_path(path, Unsafe::Symlink),
        Some(Errno::EXDEV) => unsafe_path(path, Unsafe::CrossesMount),
        Some(errno @ (Errno::ENOENT | Errno::ENOTDIR)) => ResolveError::Missing {
            path: path.to_owned(),
            errno,
        },
        _ => ResolveError::Host(e),
    }
}

/// Reaches the absolute path `path` (steps 1 to 3 of docs/safety.md, Path
/// traversal) and returns an `O_PATH` fd to it.
pub fn resolve<H: Host>(
    host: &H,
    path: &Path,
    want: Want,
) -> Result<Resolved<H::Fd>, ResolveError> {
    check_path(path)?;
    let mounts = host.mountinfo().map_err(ResolveError::Host)?;
    resolve_in(host, &mounts, path, want)
}

/// [`resolve`] against the mount table `mounts`, which the caller read
/// (and may already have judged: the kind `fs` refuses a root on a network
/// filesystem by its mountinfo line before anything there is opened or
/// `statx`ed, docs/safety.md, Before the walk). Step 2 still confirms that
/// the mount point is that mount, so a table that changed since it was
/// read is `unsafe_path`, never followed.
pub fn resolve_in<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    path: &Path,
    want: Want,
) -> Result<Resolved<H::Fd>, ResolveError> {
    check_path(path)?;
    let mount = mount_for(mounts, path)
        .map_err(|why| unsafe_path(path, why))?
        .clone();

    let mount_fd = host
        .openat2(Base::Root, &mount.mount_point, mount_point_how())
        .map_err(|e| open_error(path, e))?;
    let at_mount = host
        .statx(Base::Dir(&mount_fd), Path::new(""))
        .map_err(ResolveError::Host)?;
    let found = require(at_mount.mnt_id, path, "the mount id (STATX_MNT_ID)")?;
    let mount_root = require(
        at_mount.mount_root,
        path,
        "the mount-root attribute (STATX_ATTR_MOUNT_ROOT)",
    )?;
    if !mount_root || found != mount.id {
        return Err(unsafe_path(
            path,
            Unsafe::MountMismatch {
                expected: mount.id,
                found,
                mount_root,
            },
        ));
    }

    // Rebuilt from components, so `//` and a trailing `/` disappear.
    let rest: PathBuf = path
        .strip_prefix(&mount.mount_point)
        .expect("mount_for returns a prefix of the path")
        .components()
        .collect();
    let (fd, stat) =
        open_below(host, &mount_fd, mount.id, &rest, Access::Path, want).map_err(|e| e.at(path))?;
    Ok(Resolved { fd, mount, stat })
}

/// A configured path must be absolute and hold no `.` or `..` segment.
fn check_path(path: &Path) -> Result<(), ResolveError> {
    if !path.is_absolute() {
        return Err(unsafe_path(path, Unsafe::NotAbsolute));
    }
    if has_dot_segments(path) {
        return Err(unsafe_path(path, Unsafe::NotNormal));
    }
    Ok(())
}

/// Opens `rel` below the open directory `dir`, which is on mount `mount_id`,
/// without following symlinks or crossing mounts. An empty `rel` opens `dir`
/// itself (`.`). Returns the fd and its stat.
pub fn open_beneath<H: Host>(
    host: &H,
    dir: &H::Fd,
    mount_id: u64,
    rel: &Path,
    access: Access,
    want: Want,
) -> Result<(H::Fd, Stat), ResolveError> {
    if rel.is_absolute() || has_dot_segments(rel) {
        return Err(unsafe_path(rel, Unsafe::NotNormal));
    }
    open_below(host, dir, mount_id, rel, access, want)
}

/// Whether a `/`-separated segment of `path` is `.` or `..`. Checked on the
/// text: `Path::components` silently drops interior `.` segments.
fn has_dot_segments(path: &Path) -> bool {
    path.as_os_str()
        .as_bytes()
        .split(|b| *b == b'/')
        .any(|segment| segment == b"." || segment == b"..")
}

fn open_below<H: Host>(
    host: &H,
    dir: &H::Fd,
    mount_id: u64,
    rel: &Path,
    access: Access,
    want: Want,
) -> Result<(H::Fd, Stat), ResolveError> {
    let target = if rel.as_os_str().is_empty() {
        Path::new(".")
    } else {
        rel
    };
    let fd = host
        .openat2(Base::Dir(dir), target, beneath_how(access, want))
        .map_err(|e| open_error(rel, e))?;
    let stat = host
        .statx(Base::Dir(&fd), Path::new(""))
        .map_err(ResolveError::Host)?;
    let found = require(stat.mnt_id, rel, "the mount id (STATX_MNT_ID)")?;
    if found != mount_id {
        return Err(unsafe_path(
            rel,
            Unsafe::LeftMount {
                expected: mount_id,
                found,
            },
        ));
    }
    if stat.file_type == Some(FileType::Symlink) {
        // Unreachable without O_NOFOLLOW; kept so a change of flags cannot
        // silently hand out an fd to a symlink.
        return Err(unsafe_path(rel, Unsafe::Symlink));
    }
    Ok((fd, stat))
}

impl ResolveError {
    /// Reports the error against `path` (the full configured path) instead
    /// of the relative remainder it was found on.
    fn at(self, path: &Path) -> Self {
        let path = path.to_owned();
        match self {
            Self::UnsafePath { why, .. } => Self::UnsafePath { path, why },
            Self::UnsupportedFs { missing, .. } => Self::UnsupportedFs { path, missing },
            Self::Missing { errno, .. } => Self::Missing { path, errno },
            other @ Self::Host(_) => other,
        }
    }
}

#[cfg(test)]
mod tests {
    //! The decision logic against a scripted host. What the kernel does with
    //! the flags (refusing symlinks and mount crossings for real) is proven
    //! by the container tests (diskgc-e2e, `host_resolve_*`).

    use super::*;
    use crate::host::{
        Dev, Environ, LinkTarget, LockMode, MapEntry, MonoTime, Pid, ProcError, ProcFd, ProcLink,
        Space, TryLock, WaitLock,
    };
    use std::cell::RefCell;
    use std::time::Duration;

    /// A host with a fixed mount table whose `openat2` answers from a list
    /// of (base, path) → result, and whose `statx` of an fd returns the stat
    /// scripted for that fd. Records every `openat2`.
    struct Scripted {
        mounts: Vec<MountEntry>,
        /// (path as opened, result): fds are the index into `stats`.
        opens: Vec<(&'static str, Result<usize, Errno>)>,
        stats: Vec<Stat>,
        calls: RefCell<Vec<(String, OpenHow)>>,
    }

    fn stat(mnt_id: Option<u64>, mount_root: Option<bool>, file_type: FileType) -> Stat {
        Stat {
            dev: Dev {
                major: 0,
                minor: 42,
            },
            file_type: Some(file_type),
            perm: Some(0o755),
            nlink: Some(1),
            uid: Some(1000),
            gid: Some(1000),
            ino: Some(7),
            size: Some(0),
            blocks: Some(0),
            mtime: None,
            ctime: None,
            btime: None,
            mnt_id,
            mount_root,
        }
    }

    fn mount(id: u64, parent_id: u64, at: &str) -> MountEntry {
        MountEntry {
            id,
            parent_id,
            dev: Dev {
                major: 0,
                minor: 42,
            },
            fs_root: "/".into(),
            mount_point: at.into(),
            fs_type: "tmpfs".into(),
            source: "tmpfs".into(),
            read_only: false,
        }
    }

    impl Host for Scripted {
        type Fd = usize;
        type ProcIdentity = ();

        fn mountinfo(&self) -> Result<Vec<MountEntry>, HostError> {
            Ok(self.mounts.clone())
        }
        fn openat2(
            &self,
            base: Base<'_, usize>,
            path: &Path,
            how: OpenHow,
        ) -> Result<usize, HostError> {
            let shown = match base {
                Base::Root => path.display().to_string(),
                Base::Dir(fd) => format!("fd{fd}/{}", path.display()),
            };
            self.calls.borrow_mut().push((shown.clone(), how));
            let (_, result) = self
                .opens
                .iter()
                .find(|(p, _)| *p == shown)
                .unwrap_or_else(|| panic!("unscripted openat2 {shown}"));
            result.map_err(|errno| HostError::errno("openat2", shown, errno))
        }
        fn statx(&self, base: Base<'_, usize>, path: &Path) -> Result<Stat, HostError> {
            match base {
                Base::Dir(fd) if path.as_os_str().is_empty() => Ok(self.stats[*fd].clone()),
                _ => panic!("resolve only statxes fds"),
            }
        }
        fn read_link(
            &self,
            _: &usize,
            _: &std::ffi::OsStr,
        ) -> Result<std::path::PathBuf, HostError> {
            unreachable!()
        }
        fn read_dir(&self, _: &usize, _: usize) -> Result<crate::host::DirListing, HostError> {
            unreachable!()
        }
        fn read_head(&self, _: &usize, _: usize) -> Result<Vec<u8>, HostError> {
            unreachable!()
        }
        fn reopen_regular(&self, _: &usize) -> Result<usize, HostError> {
            unreachable!()
        }
        fn statvfs(&self, _: &usize) -> Result<Space, HostError> {
            unreachable!()
        }
        fn flock_try(&self, _: usize, _: LockMode) -> Result<TryLock<usize>, HostError> {
            unreachable!()
        }
        fn flock_wait(
            &self,
            _: usize,
            _: LockMode,
            _: Option<MonoTime>,
        ) -> Result<WaitLock<usize>, HostError> {
            unreachable!()
        }
        fn now(&self) -> MonoTime {
            unreachable!()
        }
        fn interrupted(&self) -> Option<crate::report::Signal> {
            None
        }
        fn wall_clock(&self) -> crate::host::Timestamp {
            unreachable!()
        }
        fn sleep(&self, _: Duration) {
            unreachable!()
        }
        fn pressure(&self) -> Result<crate::host::Pressure, HostError> {
            unreachable!()
        }
        fn lower_priority(&self) -> Result<(), HostError> {
            unreachable!()
        }
        fn kernel_release(&self) -> Result<String, HostError> {
            unreachable!()
        }
        fn hostname(&self) -> Result<String, HostError> {
            unreachable!()
        }
        fn pids(&self) -> Result<Vec<Pid>, HostError> {
            unreachable!()
        }
        fn proc_identity(&self, _: Pid) -> Result<Self::ProcIdentity, ProcError> {
            unreachable!()
        }
        fn proc_identity_released(&self, _: &Self::ProcIdentity) -> Result<bool, ProcError> {
            unreachable!()
        }
        fn proc_fds(&self, _: Pid) -> Result<Vec<ProcFd>, ProcError> {
            unreachable!()
        }
        fn proc_links(&self, _: Pid, _: ProcLink) -> Result<Vec<LinkTarget>, ProcError> {
            unreachable!()
        }
        fn proc_maps(&self, _: Pid) -> Result<Vec<MapEntry>, ProcError> {
            unreachable!()
        }
        fn proc_environ(&self, _: Pid) -> Result<Environ, ProcError> {
            unreachable!()
        }
        fn own_pid(&self) -> Pid {
            unreachable!()
        }
        fn effective_uid(&self) -> u32 {
            unreachable!()
        }
        fn account_uid(&self, _: &str) -> Result<Option<u32>, HostError> {
            unreachable!()
        }
        fn inherit_lock(&self, _: &crate::host::Locked<Self::Fd>) -> Result<(), HostError> {
            unreachable!()
        }
        fn proc_status(&self, _: Pid) -> Result<crate::host::ProcStatus, ProcError> {
            unreachable!()
        }
        fn bound_unix_sockets(&self) -> Result<Vec<crate::host::BoundSocket>, HostError> {
            unreachable!()
        }

        fn pid_namespace(&self) -> Result<crate::host::PidNamespace, HostError> {
            unreachable!()
        }

        fn proc_nspid(&self, _: Pid) -> Result<Vec<u32>, ProcError> {
            unreachable!()
        }

        fn pid_alive(&self, _: Pid) -> Result<bool, HostError> {
            unreachable!()
        }

        fn fs_magic(&self, _: &Self::Fd) -> Result<u64, HostError> {
            unreachable!()
        }

        fn proc_uids(&self, _: Pid) -> Result<crate::host::Ids, ProcError> {
            unreachable!()
        }

        fn proc_cmdline(&self, _: Pid) -> Result<Vec<std::ffi::OsString>, ProcError> {
            unreachable!()
        }

        fn proc_root_lookup(
            &self,
            _: Pid,
            _: &Path,
            _: bool,
        ) -> Result<Vec<crate::host::DevIno>, ProcError> {
            unreachable!()
        }
    }

    /// `/` (id 1) and a tmpfs at `/tmp` (id 30): `/tmp` opens as fd 0, and
    /// `/tmp/a/b` below it as fd 1, both answering `stats`.
    fn tmp_host(mount_stat: Stat, target: Result<Stat, Errno>) -> Scripted {
        let (open_target, stats) = match target {
            Ok(t) => (Ok(1), vec![mount_stat, t]),
            Err(errno) => (Err(errno), vec![mount_stat]),
        };
        Scripted {
            mounts: vec![mount(1, 0, "/"), mount(30, 1, "/tmp")],
            opens: vec![("/tmp", Ok(0)), ("fd0/a/b", open_target), ("fd0/.", Ok(0))],
            stats,
            calls: RefCell::new(Vec::new()),
        }
    }

    fn good_mount() -> Stat {
        stat(Some(30), Some(true), FileType::Directory)
    }

    fn good_target() -> Stat {
        stat(Some(30), Some(false), FileType::Directory)
    }

    #[test]
    fn resolves_with_the_required_flags_and_confirms_the_mount() {
        let host = tmp_host(good_mount(), Ok(good_target()));
        let resolved = resolve(&host, Path::new("/tmp/a/b"), Want::Directory).expect("resolves");
        assert_eq!(resolved.fd, 1);
        assert_eq!(resolved.mount.id, 30);
        let calls = host.calls.borrow();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0], ("/tmp".to_owned(), mount_point_how()));
        assert!(calls[0].1.resolve.no_symlinks && calls[0].1.nofollow);
        // The remainder: BENEATH | NO_SYMLINKS | NO_XDEV, every time.
        let (path, how) = &calls[1];
        assert_eq!(path, "fd0/a/b");
        assert_eq!(
            how.resolve,
            ResolveFlags {
                beneath: true,
                no_symlinks: true,
                no_xdev: true
            }
        );
        assert!(how.directory && how.access == Access::Path);
    }

    #[test]
    fn the_mount_point_itself_resolves_as_dot() {
        let host = tmp_host(good_mount(), Ok(good_target()));
        let resolved = resolve(&host, Path::new("/tmp"), Want::Directory).expect("resolves");
        assert_eq!(resolved.fd, 0);
        assert_eq!(host.calls.borrow()[1].0, "fd0/.");
    }

    #[test]
    fn kernel_refusals_become_skip_reasons() {
        for (errno, reason) in [
            (Errno::ELOOP, Some(Reason::UnsafePath)),
            (Errno::EXDEV, Some(Reason::UnsafePath)),
            (Errno::ENOENT, Some(Reason::Missing)),
            (Errno::ENOTDIR, Some(Reason::Missing)),
            (Errno::EACCES, None),
        ] {
            let host = tmp_host(good_mount(), Err(errno));
            let err = resolve(&host, Path::new("/tmp/a/b"), Want::Any).unwrap_err();
            assert_eq!(err.skip_reason(), reason, "{errno}: {err}");
            if let ResolveError::UnsafePath { path, .. } = &err {
                assert_eq!(
                    path,
                    Path::new("/tmp/a/b"),
                    "reported against the full path"
                );
            }
        }
    }

    #[test]
    fn mount_id_or_mount_root_not_reported_is_unsupported_fs() {
        // Simulated statx masks: the kernel did not answer one check.
        for (mount_stat, what) in [
            (stat(None, Some(true), FileType::Directory), "STATX_MNT_ID"),
            (
                stat(Some(30), None, FileType::Directory),
                "STATX_ATTR_MOUNT_ROOT",
            ),
        ] {
            let host = tmp_host(mount_stat, Ok(good_target()));
            let err = resolve(&host, Path::new("/tmp/a/b"), Want::Any).unwrap_err();
            assert_eq!(err.skip_reason(), Some(Reason::UnsupportedFs), "{err}");
            assert!(err.to_string().contains(what), "{err}");
        }
        // The same for the object below the mount.
        let host = tmp_host(
            good_mount(),
            Ok(stat(None, Some(false), FileType::Directory)),
        );
        let err = resolve(&host, Path::new("/tmp/a/b"), Want::Any).unwrap_err();
        assert_eq!(err.skip_reason(), Some(Reason::UnsupportedFs), "{err}");
    }

    #[test]
    fn a_mount_point_that_is_not_that_mount_is_refused() {
        for mount_stat in [
            // Another mount sits there (shadowed, or the table changed).
            stat(Some(31), Some(true), FileType::Directory),
            // Same id but not the mount's root.
            stat(Some(30), Some(false), FileType::Directory),
        ] {
            let host = tmp_host(mount_stat, Ok(good_target()));
            let err = resolve(&host, Path::new("/tmp/a/b"), Want::Any).unwrap_err();
            assert!(
                matches!(
                    err,
                    ResolveError::UnsafePath {
                        why: Unsafe::MountMismatch { expected: 30, .. },
                        ..
                    }
                ),
                "{err}"
            );
        }
    }

    #[test]
    fn an_object_on_another_mount_is_refused_even_if_the_kernel_opened_it() {
        let host = tmp_host(
            good_mount(),
            Ok(stat(Some(99), Some(true), FileType::Directory)),
        );
        let err = resolve(&host, Path::new("/tmp/a/b"), Want::Any).unwrap_err();
        assert_eq!(
            err,
            ResolveError::UnsafePath {
                path: "/tmp/a/b".into(),
                why: Unsafe::LeftMount {
                    expected: 30,
                    found: 99
                }
            }
        );
    }

    #[test]
    fn relative_and_non_normal_paths_are_refused_before_touching_the_host() {
        let host = tmp_host(good_mount(), Ok(good_target()));
        for (path, why) in [
            ("tmp/a", Unsafe::NotAbsolute),
            ("/tmp/../etc", Unsafe::NotNormal),
            ("/tmp/./a", Unsafe::NotNormal),
            ("/tmp/a/.", Unsafe::NotNormal),
        ] {
            let err = resolve(&host, Path::new(path), Want::Any).unwrap_err();
            assert_eq!(
                err,
                ResolveError::UnsafePath {
                    path: path.into(),
                    why
                }
            );
        }
        for rel in ["/abs", "a/../b", "../x", "./a", "a/./b"] {
            let err =
                open_beneath(&host, &0, 30, Path::new(rel), Access::Path, Want::Any).unwrap_err();
            assert_eq!(err.skip_reason(), Some(Reason::UnsafePath), "{rel}");
        }
        assert!(host.calls.borrow().is_empty());
    }

    #[test]
    fn the_deepest_prefix_mount_wins_component_wise() {
        let mounts = vec![
            mount(1, 0, "/"),
            mount(2, 1, "/tmp"),
            mount(3, 2, "/tmp/a"),
            mount(4, 1, "/tmpx"),
        ];
        let at = |p: &str| mount_for(&mounts, Path::new(p)).map(|m| m.id);
        assert_eq!(at("/tmp/a/b"), Ok(3));
        assert_eq!(at("/tmp/a"), Ok(3));
        assert_eq!(at("/tmp/ab"), Ok(2), "component-wise, not textual");
        assert_eq!(at("/tmpx/y"), Ok(4));
        assert_eq!(at("/etc"), Ok(1));
        assert_eq!(
            mount_for(&mounts[1..], Path::new("/etc")),
            Err(Unsafe::NoMount)
        );
    }

    #[test]
    fn stacked_mounts_resolve_to_the_topmost_whatever_the_line_order() {
        // 5 is stacked on 3, which is stacked on 2, all at /tmp/s.
        let mut mounts = vec![
            mount(1, 0, "/"),
            mount(2, 1, "/tmp/s"),
            mount(5, 3, "/tmp/s"),
            mount(3, 2, "/tmp/s"),
        ];
        assert_eq!(
            mount_for(&mounts, Path::new("/tmp/s/x")).map(|m| m.id),
            Ok(5)
        );
        mounts.reverse();
        assert_eq!(
            mount_for(&mounts, Path::new("/tmp/s/x")).map(|m| m.id),
            Ok(5)
        );
        // Two mounts at one point, neither on the other: refuse to guess.
        let siblings = vec![
            mount(1, 0, "/"),
            mount(2, 1, "/tmp/s"),
            mount(3, 1, "/tmp/s"),
        ];
        assert_eq!(
            mount_for(&siblings, Path::new("/tmp/s")),
            Err(Unsafe::AmbiguousMounts(vec![2, 3]))
        );
    }

    #[test]
    fn full_mount_chain_refuses_unsafe_ancestors_but_allows_local_same_device_bind() {
        let base = mount(1, 0, "/");
        let mut network = mount(2, 1, "/net");
        network.fs_type = "nfs4".into();
        let local_below_network = mount(3, 2, "/net/local");
        let unsafe_chain = [base.clone(), network, local_below_network];
        assert!(matches!(
            screen_mount_chain(&unsafe_chain, Path::new("/net/local/build")),
            Err(MountScreenError::Networked { at, .. }) if at == Path::new("/net")
        ));

        let mut private = mount(4, 1, "/bound");
        private.fs_root = "/systemd-private-abc-cups.service-x1/tmp".into();
        let local_below_private = mount(5, 4, "/bound/local");
        assert!(matches!(
            screen_mount_chain(&[base.clone(), private, local_below_private], Path::new("/bound/local/build")),
            Err(MountScreenError::PrivateTmp { at, .. }) if at == Path::new("/bound")
        ));

        // The bind mount has a different mount id, but the same device.
        // Its local ancestry is safe and must remain usable.
        let same_device_bind = mount(6, 1, "/safe/bind");
        assert_eq!(same_device_bind.dev, base.dev);
        assert_eq!(
            screen_mount_chain(&[base, same_device_bind], Path::new("/safe/bind/build")),
            Ok(())
        );
    }

    /// TASK-180: an untriggered automount point (systemd's
    /// `x-systemd.automount` NFS, say) is `autofs` in mountinfo; a lookup
    /// through it would mount the share and can hang, so a root at or
    /// below it is refused from mountinfo alone. Once triggered, the real
    /// filesystem is stacked on it and decides: NFS is refused as NFS, a
    /// local one (an automounted /boot) is accepted.
    #[test]
    fn untriggered_automount_is_refused_and_a_triggered_one_is_its_filesystem() {
        let base = mount(1, 0, "/");
        let mut trigger = mount(2, 1, "/mnt/nas");
        trigger.fs_type = "autofs".into();
        let untriggered = [base.clone(), trigger.clone()];
        for root in ["/mnt/nas", "/mnt/nas/x/build"] {
            assert!(
                matches!(
                    screen_mount_chain(&untriggered, Path::new(root)),
                    Err(MountScreenError::Networked { at, mount_point, fs_type })
                        if at == Path::new(root)
                            && mount_point == Path::new("/mnt/nas")
                            && fs_type == "autofs"
                ),
                "{root}"
            );
        }
        assert_eq!(
            screen_mount_chain(&untriggered, Path::new("/mnt/other")),
            Ok(())
        );

        let mut share = mount(3, 2, "/mnt/nas");
        share.fs_type = "nfs4".into();
        assert!(matches!(
            screen_mount_chain(&[base.clone(), trigger.clone(), share], Path::new("/mnt/nas/x")),
            Err(MountScreenError::Networked { fs_type, .. }) if fs_type == "nfs4"
        ));

        let mut boot_trigger = trigger;
        boot_trigger.mount_point = "/boot".into();
        let mut boot = mount(4, 2, "/boot");
        boot.fs_type = "vfat".into();
        assert_eq!(
            screen_mount_chain(&[base, boot_trigger, boot], Path::new("/boot/x")),
            Ok(())
        );
    }
}
