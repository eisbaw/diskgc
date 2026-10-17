//! The `Host` port (docs/architecture.md, The Host port).
//!
//! Everything diskgc learns about or does to the machine goes through
//! [`Host`]. File and lock operations are **syscall-shaped** (`openat2`,
//! `statx`, `getdents`, `flock`, `statvfs`, mountinfo, `/proc` reads), so the safety
//! logic that chooses flags, orders calls and re-checks results lives in
//! `diskgc-core`, where `ModelHost` can test it. Time and every blocking
//! operation are port operations too, so a model can run them on a virtual
//! clock.
//!
//! Rules every implementation follows (the safety argument relies on them):
//!
//! - **Locks are released only by closing.** Locking consumes the fd and
//!   returns a [`Locked`] that lives as long as the lock; dropping it closes
//!   the fd. There is no unlock and no second lock call on a held lock, so
//!   `flock(LOCK_UN)` (which would release the lock for every inherited copy
//!   of the descriptor) and a non-atomic conversion cannot be expressed
//!   (docs/safety.md, Host lock).
//! - **`/proc` readers cover every task** of a thread group: a thread can
//!   hold files after the main thread exited, and can have its own fd table
//!   or working directory (`CLONE_FILES`, `CLONE_FS`).
//! - **statx fields are optional.** A field is `Some` only if the kernel set
//!   its bit in `stx_mask` (or, for attributes, in `stx_attributes_mask`).
//!   Callers decide what an unanswerable check means; for path traversal it
//!   is `unsupported_fs` (docs/safety.md, Path traversal).
//! - **No final symlink is followed** by [`Host::statx`], and every fd is
//!   opened close-on-exec.
//! - **Deleting operations live in a trait of their own**, [`Delete`], so
//!   code that holds only a [`Host`] (survey, tally, planner) cannot delete,
//!   and the pre-release sandbox gate (docs/safety.md) guards exactly those
//!   methods. `LinuxHost` implements it behind the gate (a refusal is a
//!   [`Cause::SandboxGate`] error); `ModelHost`, test-only and deleting
//!   nothing real, implements it without one.
//!
//! The trait is tentative (PRD, Irreversibility map); the path-resolution and
//! lock semantics it serves are not.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A Linux errno value as the host reported it. The numbers are Linux's
/// (x86_64 and every other architecture diskgc might run on share these).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Errno(pub i32);

impl Errno {
    pub const EPERM: Errno = Errno(1);
    pub const ENOENT: Errno = Errno(2);
    pub const ESRCH: Errno = Errno(3);
    pub const EINTR: Errno = Errno(4);
    pub const ENXIO: Errno = Errno(6);
    pub const EBADF: Errno = Errno(9);
    /// Also `EWOULDBLOCK`.
    pub const EAGAIN: Errno = Errno(11);
    pub const EACCES: Errno = Errno(13);
    pub const EBUSY: Errno = Errno(16);
    pub const EEXIST: Errno = Errno(17);
    pub const EXDEV: Errno = Errno(18);
    pub const ENOTDIR: Errno = Errno(20);
    pub const EISDIR: Errno = Errno(21);
    pub const EINVAL: Errno = Errno(22);
    pub const EROFS: Errno = Errno(30);
    pub const ENOSYS: Errno = Errno(38);
    pub const ENOTEMPTY: Errno = Errno(39);
    pub const ELOOP: Errno = Errno(40);

    /// The symbolic name of the errno values diskgc interprets.
    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::EPERM => "EPERM",
            Self::ENOENT => "ENOENT",
            Self::ESRCH => "ESRCH",
            Self::EINTR => "EINTR",
            Self::ENXIO => "ENXIO",
            Self::EBADF => "EBADF",
            Self::EAGAIN => "EAGAIN",
            Self::EACCES => "EACCES",
            Self::EBUSY => "EBUSY",
            Self::EEXIST => "EEXIST",
            Self::EXDEV => "EXDEV",
            Self::ENOTDIR => "ENOTDIR",
            Self::EISDIR => "EISDIR",
            Self::EINVAL => "EINVAL",
            Self::EROFS => "EROFS",
            Self::ENOSYS => "ENOSYS",
            Self::ENOTEMPTY => "ENOTEMPTY",
            Self::ELOOP => "ELOOP",
            _ => return None,
        })
    }
}

impl fmt::Debug for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{name} ({})", self.0),
            None => write!(f, "errno {}", self.0),
        }
    }
}

/// Why a host operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// The kernel refused it with this errno.
    Errno(Errno),
    /// It produced something diskgc cannot use (a malformed `/proc` file, an
    /// overflowing value, a misuse of the port); [`HostError::detail`] says
    /// what.
    Invalid,
    /// The pre-release sandbox gate refused a deleting operation outside a
    /// diskgc test container (docs/safety.md, Pre-release sandbox gate);
    /// [`HostError::detail`] names the marker that is missing or wrong. Never
    /// a kernel answer, so it can never be mistaken for one (or retried as
    /// one): nothing was attempted.
    SandboxGate,
}

/// A failed host operation: which operation, on what, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    /// The operation, e.g. `openat2` or `read mountinfo`.
    pub op: &'static str,
    /// What it was applied to (usually a path), for the message.
    pub subject: String,
    pub cause: Cause,
    /// Further explanation (always set unless `cause` is [`Cause::Errno`]).
    pub detail: String,
}

impl HostError {
    /// The kernel refused `op` on `subject` with `errno`.
    pub fn errno(op: &'static str, subject: impl fmt::Display, errno: Errno) -> Self {
        Self {
            op,
            subject: subject.to_string(),
            cause: Cause::Errno(errno),
            detail: String::new(),
        }
    }

    /// `op` on `subject` produced something diskgc cannot use (a malformed
    /// `/proc` file, an overflowing value, a misuse of the port).
    pub fn invalid(
        op: &'static str,
        subject: impl fmt::Display,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            op,
            subject: subject.to_string(),
            cause: Cause::Invalid,
            detail: detail.into(),
        }
    }

    /// The pre-release sandbox gate refused `op` on `subject`; `why` says
    /// which sandbox marker is missing or wrong.
    pub fn sandbox_gate(
        op: &'static str,
        subject: impl fmt::Display,
        why: impl fmt::Display,
    ) -> Self {
        Self {
            op,
            subject: subject.to_string(),
            cause: Cause::SandboxGate,
            detail: format!(
                "refused by the pre-release sandbox gate (docs/safety.md): {why}; until v1, \
                 diskgc deletes only inside a diskgc test container"
            ),
        }
    }

    /// The errno, if the kernel returned one.
    pub fn kernel_errno(&self) -> Option<Errno> {
        match self.cause {
            Cause::Errno(errno) => Some(errno),
            Cause::Invalid | Cause::SandboxGate => None,
        }
    }

    /// Whether the kernel returned exactly `errno`.
    pub fn is(&self, errno: Errno) -> bool {
        self.cause == Cause::Errno(errno)
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: ", self.op, self.subject)?;
        match (self.cause, self.detail.is_empty()) {
            (Cause::Errno(errno), true) => write!(f, "{errno}"),
            (Cause::Errno(errno), false) => write!(f, "{errno}: {}", self.detail),
            (Cause::Invalid | Cause::SandboxGate, _) => f.write_str(&self.detail),
        }
    }
}

impl std::error::Error for HostError {}

/// A device number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Dev {
    pub major: u32,
    pub minor: u32,
}

impl fmt::Display for Dev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.major, self.minor)
    }
}

/// The (device, inode) pair that identifies an object for the in-use checks
/// (docs/safety.md, In use).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DevIno {
    pub dev: Dev,
    pub ino: u64,
}

impl fmt::Display for DevIno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.dev, self.ino)
    }
}

/// File type from `stx_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    Socket,
    Fifo,
    CharDevice,
    BlockDevice,
    /// A type bit pattern Linux does not define.
    Unknown(u32),
}

impl FileType {
    /// From the `S_IFMT` bits of a mode.
    pub fn from_mode(mode: u32) -> Self {
        match mode & 0o170_000 {
            0o100_000 => Self::Regular,
            0o040_000 => Self::Directory,
            0o120_000 => Self::Symlink,
            0o140_000 => Self::Socket,
            0o010_000 => Self::Fifo,
            0o020_000 => Self::CharDevice,
            0o060_000 => Self::BlockDevice,
            other => Self::Unknown(other),
        }
    }
}

/// A timestamp as statx reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp {
    pub secs: i64,
    pub nanos: u32,
}

/// The result of `statx`. Every field except `dev` is `None` unless the
/// kernel marked it as answered (its bit in `stx_mask`, or for
/// `mount_root` the `STATX_ATTR_MOUNT_ROOT` bit in `stx_attributes_mask`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    /// The device of the filesystem holding the object (always reported).
    pub dev: Dev,
    pub file_type: Option<FileType>,
    /// Permission bits (`mode & 0o7777`).
    pub perm: Option<u32>,
    pub nlink: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub ino: Option<u64>,
    pub size: Option<u64>,
    /// Allocated 512-byte blocks.
    pub blocks: Option<u64>,
    pub mtime: Option<Timestamp>,
    pub ctime: Option<Timestamp>,
    pub btime: Option<Timestamp>,
    /// `stx_mnt_id`: the id mountinfo uses for the mount (Linux 5.8+).
    pub mnt_id: Option<u64>,
    /// `STATX_ATTR_MOUNT_ROOT`: the object is the root of its mount.
    pub mount_root: Option<bool>,
}

impl Stat {
    /// (device, inode), if the inode number was answered.
    pub fn dev_ino(&self) -> Option<DevIno> {
        self.ino.map(|ino| DevIno { dev: self.dev, ino })
    }
}

/// What an open fd may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// `O_PATH`: a position in the tree only (statx, further `openat2`).
    Path,
    /// `O_RDONLY`: also reading, listing and `flock`.
    Read,
}

/// `RESOLVE_*` flags of `openat2` (only those diskgc uses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResolveFlags {
    /// `RESOLVE_BENEATH`: never leave the starting directory.
    pub beneath: bool,
    /// `RESOLVE_NO_SYMLINKS`: refuse every symlink (`ELOOP`).
    pub no_symlinks: bool,
    /// `RESOLVE_NO_XDEV`: refuse every mount crossing (`EXDEV`), including
    /// bind mounts of the same filesystem.
    pub no_xdev: bool,
}

/// The arguments of `openat2` diskgc chooses. `O_CLOEXEC` is always set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenHow {
    pub access: Access,
    /// `O_DIRECTORY`.
    pub directory: bool,
    /// `O_NOFOLLOW`.
    pub nofollow: bool,
    pub resolve: ResolveFlags,
}

/// Where a path given to the port starts.
#[derive(Debug)]
pub enum Base<'a, F> {
    /// The process's root directory; the path must be absolute.
    Root,
    /// An open directory (or, with an empty path, the fd itself).
    Dir(&'a F),
}

impl<F> Clone for Base<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<F> Copy for Base<'_, F> {}

/// One line of `/proc/self/mountinfo` (proc(5)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// The mount id (the value `statx` reports as `stx_mnt_id`).
    pub id: u64,
    pub parent_id: u64,
    pub dev: Dev,
    /// The directory of the filesystem that forms this mount's root (`/`
    /// unless it is a bind mount of a subdirectory).
    pub fs_root: PathBuf,
    /// Where the mount is, relative to the process's root, unescaped.
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub source: String,
    /// `ro` among the per-mount or superblock options.
    pub read_only: bool,
}

/// Space on a filesystem, from `statvfs` (docs/architecture.md, Measuring
/// space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    /// `f_bavail × f_frsize`: what an unprivileged writer can use (never the
    /// root reserve).
    pub avail_bytes: u64,
    /// `f_blocks × f_frsize`.
    pub size_bytes: u64,
}

/// `flock` modes. There is deliberately no unlock: see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

/// An fd with a `flock` held on it. The lock is released when this is
/// dropped (the fd is closed), and never otherwise.
///
/// The lock calls take the fd **by value** and hand it back only inside a
/// `Locked`, which lends it out by reference alone. So a held lock can never
/// be passed to `flock` again: a shared-to-exclusive conversion (which
/// `flock(2)` does not do atomically: a failed conversion drops the shared
/// lock) and any re-lock or unlock of a held lock cannot be written.
///
/// ```compile_fail,E0308
/// use diskgc_core::host::{Host, LockMode, TryLock};
/// fn convert<H: Host>(host: &H, fd: H::Fd) {
///     if let Ok(TryLock::Acquired(shared)) = host.flock_try(fd, LockMode::Shared) {
///         // A second flock on the held lock does not compile.
///         let _ = host.flock_try(shared.fd(), LockMode::Exclusive);
///     }
/// }
/// ```
///
/// The same code without the second call compiles (the positive control
/// for the test above):
///
/// ```
/// use diskgc_core::host::{Host, LockMode, TryLock};
/// fn lock<H: Host>(host: &H, fd: H::Fd) {
///     if let Ok(TryLock::Acquired(shared)) = host.flock_try(fd, LockMode::Shared) {
///         let _held: &H::Fd = shared.fd();
///     }
/// }
/// ```
#[derive(Debug)]
pub struct Locked<F> {
    fd: F,
    mode: LockMode,
}

impl<F> Locked<F> {
    /// For [`Host`] implementations only: wraps `fd` right after `flock`
    /// granted `mode` on it.
    pub fn granted(fd: F, mode: LockMode) -> Self {
        Self { fd, mode }
    }

    /// The locked fd, for use below it (statx, openat2); never for `flock`.
    pub fn fd(&self) -> &F {
        &self.fd
    }

    pub fn mode(&self) -> LockMode {
        self.mode
    }
}

/// Result of a non-blocking lock attempt.
#[derive(Debug)]
pub enum TryLock<F> {
    Acquired(Locked<F>),
    /// Someone else holds a conflicting lock; the fd comes back unlocked.
    Busy(F),
}

/// Result of waiting for a lock.
#[derive(Debug)]
pub enum WaitLock<F> {
    Acquired(Locked<F>),
    /// The deadline passed with the lock still held by someone else; the fd
    /// comes back unlocked.
    DeadlinePassed(F),
}

/// A point on the host's monotonic clock that includes time spent suspended
/// (`CLOCK_BOOTTIME` on Linux), measured from an unspecified origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonoTime(pub Duration);

impl MonoTime {
    pub fn checked_add(self, d: Duration) -> Option<Self> {
        self.0.checked_add(d).map(Self)
    }

    /// Time from `earlier` to `self`, zero if `earlier` is later.
    pub fn saturating_since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

/// A process id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pid(pub u32);

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The per-process magic links diskgc reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcLink {
    Cwd,
    Root,
    Exe,
}

impl ProcLink {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cwd => "cwd",
            Self::Root => "root",
            Self::Exe => "exe",
        }
    }
}

/// What a `/proc/<pid>` magic link (an fd, `cwd`, `root`, `exe`) refers to.
/// `stat` comes from statting **through** the link, so it identifies the
/// object even in another mount namespace or after it was deleted; `text`
/// is the link's path text, for messages only (it is wrong for other mount
/// namespaces and carries ` (deleted)` for deleted files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTarget {
    pub text: OsString,
    pub stat: Stat,
}

/// One open file descriptor of a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcFd {
    pub fd: u32,
    pub target: LinkTarget,
}

/// One line of `/proc/<pid>/maps`, reduced to its object: the `dev` and
/// `inode` columns (inode 0 for anonymous mappings) and the path column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapEntry {
    pub dev: Dev,
    pub ino: u64,
    /// The path column (may end in ` (deleted)`; `None` if empty).
    pub path: Option<OsString>,
}

/// A process's initial environment (`/proc/<pid>/environ`, the environment
/// block as of `exec`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environ {
    /// `NAME=value` entries, split at the first `=`, in order.
    pub vars: Vec<(OsString, OsString)>,
    /// Entries without `=` (execve does not forbid them); kept, not dropped.
    pub without_equals: Vec<OsString>,
}

impl Environ {
    /// The value of the first variable called `name`.
    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_os_str())
    }
}

/// Why a `/proc/<pid>` reader returned nothing (docs/safety.md, In use:
/// unreadable or vanishing processes). Callers treat everything except
/// `Ok` and `Gone` as "may be using anything".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcError {
    /// The kernel said, through the process's pidfd, that the process has
    /// no live task: released, or a zombie alone in its group. Never
    /// inferred from a missing `/proc` entry (`ENOENT`) or a task listing.
    Gone,
    /// Not readable by this user (`EACCES`, `EPERM`): another user's or a
    /// non-dumpable process.
    Denied(HostError),
    /// Anything else, including a live task that could not be inspected.
    Failed(HostError),
}

impl ProcError {
    /// Classifies a failed `/proc/<pid>` access: `Denied` or `Failed`, never
    /// `Gone`. A vanished entry (`ENOENT`, `ESRCH`) is `Failed` here: alone
    /// it proves nothing (a lookup can race an exec, `hidepid` hides live
    /// processes), so the readers handle it before it gets this far.
    pub fn from_host(e: HostError) -> Self {
        match e.kernel_errno() {
            Some(Errno::EACCES | Errno::EPERM) => Self::Denied(e),
            _ => Self::Failed(e),
        }
    }
}

impl fmt::Display for ProcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gone => f.write_str("process gone"),
            Self::Denied(e) => write!(f, "denied: {e}"),
            Self::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProcError {}

/// The machine, as diskgc sees it. See the module docs for the rules.
pub trait Host {
    /// An open file descriptor; dropping it closes it (and so releases any
    /// lock taken on it).
    type Fd: fmt::Debug;

    /// The process's mount table (`/proc/self/mountinfo`), unescaped.
    fn mountinfo(&self) -> Result<Vec<MountEntry>, HostError>;

    /// `openat2(base, path, how)`. With [`Base::Root`], `path` is absolute.
    fn openat2(
        &self,
        base: Base<'_, Self::Fd>,
        path: &Path,
        how: OpenHow,
    ) -> Result<Self::Fd, HostError>;

    /// `statx(base, path, AT_SYMLINK_NOFOLLOW)` asking for the basic stats,
    /// birth time and mount id. An empty `path` with [`Base::Dir`] stats the
    /// fd itself (`AT_EMPTY_PATH`). Only the last component is not followed:
    /// like any `*at` call, symlinks in earlier components are, so safe
    /// callers pass a single name or an empty path below an fd reached by
    /// [`crate::resolve`].
    fn statx(&self, base: Base<'_, Self::Fd>, path: &Path) -> Result<Stat, HostError>;

    /// The names in the open directory `dir` (`getdents64` until the end, on
    /// a fresh `O_RDONLY | O_DIRECTORY` open of `.` below `dir`, so `dir` may
    /// be an `O_PATH` fd and every call is a complete listing of its own),
    /// without `.` and `..`, in the order the filesystem returns them.
    ///
    /// Names only, deliberately: `d_type` can be `DT_UNKNOWN` and is stale
    /// by the time it is used, and every caller must `statx` each name
    /// anyway (identity, owner, type) before acting on it. Entries created
    /// or removed while the listing runs may or may not appear.
    fn read_dir(&self, dir: &Self::Fd) -> Result<Vec<OsString>, HostError>;

    /// `fstatvfs(fd)`.
    fn statvfs(&self, fd: &Self::Fd) -> Result<Space, HostError>;

    /// `flock(fd, mode | LOCK_NB)` on an fd that holds no lock (it is
    /// consumed; see [`Locked`]). On error the fd is closed.
    fn flock_try(&self, fd: Self::Fd, mode: LockMode) -> Result<TryLock<Self::Fd>, HostError>;

    /// Waits for `flock(fd, mode)` until `deadline` (`None`: no deadline).
    fn flock_wait(
        &self,
        fd: Self::Fd,
        mode: LockMode,
        deadline: Option<MonoTime>,
    ) -> Result<WaitLock<Self::Fd>, HostError>;

    /// The monotonic clock including suspend time.
    fn now(&self) -> MonoTime;

    /// Blocks for `duration`.
    fn sleep(&self, duration: Duration);

    /// The kernel release string (`uname -r`).
    fn kernel_release(&self) -> Result<String, HostError>;

    /// The visible processes (thread-group ids; the readers below cover
    /// every task of each).
    fn pids(&self) -> Result<Vec<Pid>, HostError>;

    /// The open fds of every task of the process (tasks may have fd tables
    /// of their own), each statted through its link, without duplicates.
    /// Fds closed while they are listed are left out.
    fn proc_fds(&self, pid: Pid) -> Result<Vec<ProcFd>, ProcError>;

    /// The `cwd`, `root` or `exe` of every live task of the process (tasks
    /// may have their own `cwd` and `root`), statted through the link,
    /// without duplicates.
    fn proc_links(&self, pid: Pid, which: ProcLink) -> Result<Vec<LinkTarget>, ProcError>;

    /// The process's memory mappings, read through a live task.
    fn proc_maps(&self, pid: Pid) -> Result<Vec<MapEntry>, ProcError>;

    /// The process's initial environment, read through a live task.
    fn proc_environ(&self, pid: Pid) -> Result<Environ, ProcError>;
}

/// What [`Delete::unlinkat`] removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unlink {
    /// Anything but a directory (flags 0).
    NonDir,
    /// An empty directory (`AT_REMOVEDIR`).
    Dir,
}

/// Whether `name` is a single path component: not empty, no `/`, not `.`
/// or `..`. Only such a name can be unlinked safely: `unlinkat` follows
/// symlinks in every earlier component of a longer path.
pub fn is_single_name(name: &OsStr) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(n)), None) if n == name
    )
}

/// The deleting operations of the port (see the module docs: they are kept
/// apart from [`Host`] so that only code given a `Delete` can delete).
/// Outside a diskgc test container, `LinuxHost` refuses every one of them
/// with a [`Cause::SandboxGate`] error until v1 (docs/safety.md,
/// Pre-release sandbox gate).
pub trait Delete: Host {
    /// `unlinkat(dir, name, flags)`: removes the entry `name` of the open
    /// directory `dir`. `name` is a single component ([`is_single_name`]:
    /// no `/`, not `.` or `..`). Linux has no unlink-by-inode, so callers check the entry's
    /// identity with [`Host::statx`]`(Base::Dir(dir), name)` right before,
    /// as the previous port call (docs/safety.md, Deletion); `ModelHost`
    /// fails a test that does not.
    fn unlinkat(&self, dir: &Self::Fd, name: &OsStr, what: Unlink) -> Result<(), HostError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_types_from_mode_bits() {
        assert_eq!(FileType::from_mode(0o100_644), FileType::Regular);
        assert_eq!(FileType::from_mode(0o040_755), FileType::Directory);
        assert_eq!(FileType::from_mode(0o120_777), FileType::Symlink);
        assert_eq!(FileType::from_mode(0o140_755), FileType::Socket);
        assert_eq!(FileType::from_mode(0o010_644), FileType::Fifo);
        assert_eq!(FileType::from_mode(0o000_644), FileType::Unknown(0));
    }

    #[test]
    fn proc_errors_are_classified_by_errno() {
        let e = |errno| HostError::errno("read", "/proc/1/maps", errno);
        // A vanished entry alone never means Gone.
        for vanished in [Errno::ENOENT, Errno::ESRCH] {
            assert!(matches!(
                ProcError::from_host(e(vanished)),
                ProcError::Failed(_)
            ));
        }
        assert!(matches!(
            ProcError::from_host(e(Errno::EACCES)),
            ProcError::Denied(_)
        ));
        assert!(matches!(
            ProcError::from_host(e(Errno::EINVAL)),
            ProcError::Failed(_)
        ));
    }

    #[test]
    fn single_names_exclude_paths_and_dot_entries() {
        for ok in ["f", "a b", ".hidden", "..x", "x.."] {
            assert!(is_single_name(OsStr::new(ok)), "{ok:?}");
        }
        // "a/" and "./a" normalise to one component that differs from the
        // name, so they are refused too.
        for bad in ["", ".", "..", "a/b", "/a", "a/", "./a", "/"] {
            assert!(!is_single_name(OsStr::new(bad)), "{bad:?}");
        }
    }

    #[test]
    fn a_gate_refusal_is_not_a_kernel_answer() {
        let gate = HostError::sandbox_gate("unlinkat", "/tmp/x", "marker missing");
        assert_eq!(gate.cause, Cause::SandboxGate);
        assert_eq!(gate.kernel_errno(), None);
        assert!(!gate.is(Errno::EPERM) && !gate.is(Errno::EACCES));
        assert_ne!(
            gate.cause,
            HostError::invalid("unlinkat", "/tmp/x", "x").cause
        );
        assert_eq!(
            gate.to_string(),
            "unlinkat /tmp/x: refused by the pre-release sandbox gate (docs/safety.md): marker \
             missing; until v1, diskgc deletes only inside a diskgc test container"
        );
    }

    #[test]
    fn host_errors_name_the_errno() {
        let e = HostError::errno("openat2", "/tmp/x", Errno::ELOOP);
        assert_eq!(e.to_string(), "openat2 /tmp/x: ELOOP (40)");
        assert_eq!(Errno(4095).to_string(), "errno 4095");
    }
}
