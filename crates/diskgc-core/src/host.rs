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
    pub const EIO: Errno = Errno(5);
    pub const ENXIO: Errno = Errno(6);
    pub const EBADF: Errno = Errno(9);
    pub const ENOMEM: Errno = Errno(12);
    /// Also `EWOULDBLOCK`.
    pub const EAGAIN: Errno = Errno(11);
    pub const EACCES: Errno = Errno(13);
    pub const EBUSY: Errno = Errno(16);
    pub const EEXIST: Errno = Errno(17);
    pub const EXDEV: Errno = Errno(18);
    pub const ENOTDIR: Errno = Errno(20);
    pub const EISDIR: Errno = Errno(21);
    pub const EINVAL: Errno = Errno(22);
    pub const ENFILE: Errno = Errno(23);
    pub const EMFILE: Errno = Errno(24);
    pub const ENOSPC: Errno = Errno(28);
    pub const EROFS: Errno = Errno(30);
    pub const ENOSYS: Errno = Errno(38);
    pub const ENOTEMPTY: Errno = Errno(39);
    pub const ELOOP: Errno = Errno(40);

    /// Every errno value with a [`Errno::name`].
    pub const NAMED: &[Errno] = &[
        Self::EPERM,
        Self::ENOENT,
        Self::ESRCH,
        Self::EINTR,
        Self::EIO,
        Self::ENXIO,
        Self::EBADF,
        Self::ENOMEM,
        Self::EAGAIN,
        Self::EACCES,
        Self::EBUSY,
        Self::EEXIST,
        Self::EXDEV,
        Self::ENOTDIR,
        Self::EISDIR,
        Self::EINVAL,
        Self::ENFILE,
        Self::EMFILE,
        Self::ENOSPC,
        Self::EROFS,
        Self::ENOSYS,
        Self::ENOTEMPTY,
        Self::ELOOP,
    ];

    /// The errno called `name` (one of [`Errno::NAMED`]).
    pub fn from_name(name: &str) -> Option<Errno> {
        Self::NAMED.iter().copied().find(|e| e.name() == Some(name))
    }

    /// The symbolic name of the errno values diskgc interprets.
    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::EPERM => "EPERM",
            Self::ENOENT => "ENOENT",
            Self::ESRCH => "ESRCH",
            Self::EINTR => "EINTR",
            Self::EIO => "EIO",
            Self::ENXIO => "ENXIO",
            Self::EBADF => "EBADF",
            Self::ENOMEM => "ENOMEM",
            Self::EAGAIN => "EAGAIN",
            Self::EACCES => "EACCES",
            Self::EBUSY => "EBUSY",
            Self::EEXIST => "EEXIST",
            Self::EXDEV => "EXDEV",
            Self::ENOTDIR => "ENOTDIR",
            Self::EISDIR => "EISDIR",
            Self::EINVAL => "EINVAL",
            Self::ENFILE => "ENFILE",
            Self::EMFILE => "EMFILE",
            Self::ENOSPC => "ENOSPC",
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
                "refused by the pre-release sandbox gate (docs/safety.md): {why}; a development \
                 build deletes only inside a diskgc test container (release packages, from 0.1, \
                 are built without the gate)"
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

    /// Whether diskgc itself ran out of file descriptors (`EMFILE`: its own
    /// `RLIMIT_NOFILE`; `ENFILE`: the system's table).
    pub fn is_out_of_descriptors(&self) -> bool {
        self.is(Errno::EMFILE) || self.is(Errno::ENFILE)
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

/// As messages name it ("a {self}").
impl fmt::Display for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Regular => f.write_str("regular file"),
            Self::Directory => f.write_str("directory"),
            Self::Symlink => f.write_str("symlink"),
            Self::Socket => f.write_str("socket"),
            Self::Fifo => f.write_str("FIFO"),
            Self::CharDevice => f.write_str("character device"),
            Self::BlockDevice => f.write_str("block device"),
            Self::Unknown(bits) => write!(f, "file of unknown type {bits:#o}"),
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

/// System-wide pressure stall information (`/proc/pressure/io`, `memory`
/// and `cpu`): the `some avg10` value of each, the share of the last ten
/// seconds in which at least one task stalled on that resource, in
/// hundredths of a percent (`some avg10=12.34` is `1234`). What the
/// background tally's pressure brake reads (docs/architecture.md, The
/// tally).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pressure {
    pub io: u32,
    pub memory: u32,
    pub cpu: u32,
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
    /// A termination signal arrived while waiting; the fd was closed.
    Interrupted(crate::report::Signal),
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
    /// Every link, in the order of [`Host::proc_links_all`].
    pub const ALL: [Self; 3] = [Self::Cwd, Self::Root, Self::Exe];

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
/// `inode` columns (inode 0 for anonymous mappings). The path column is not
/// kept: matching is by (device, inode) only, and path text is wrong in
/// other mount namespaces (docs/safety.md, In use); not copying it saves an
/// allocation per mapping, of which a desktop has some 10^5 (TASK-115).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapEntry {
    pub dev: Dev,
    pub ino: u64,
}

/// A process's initial environment (`/proc/<pid>/environ`, the environment
/// block as of `exec`): its NUL-separated entries, kept as the one block
/// they were read as and split on demand. A desktop's scan reads some 10^5
/// variables; an owned string per name and value made allocation the
/// static build's bottleneck (TASK-115).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environ {
    /// The entries, each ended by a NUL except the last (one trailing NUL
    /// of the block is dropped; an empty block has no entries).
    body: Vec<u8>,
}

impl Environ {
    /// The environment of a block as `/proc/<pid>/environ` shows it:
    /// NUL-terminated entries; one trailing NUL is dropped.
    pub fn from_block(mut block: Vec<u8>) -> Self {
        if block.last() == Some(&0) {
            block.pop();
        }
        block.shrink_to_fit();
        Self { body: block }
    }

    /// The environment with these entries (`NAME=value`, or text without
    /// `=`), in order: the block the kernel shows for them, each entry
    /// ended by a NUL ([`Environ::from_block`]), so `["A=1", ""]` keeps its
    /// empty entry as the kernel's `A=1\0\0` does. An entry cannot hold a
    /// NUL: that would be two.
    pub fn from_entries<S: AsRef<OsStr>>(entries: impl IntoIterator<Item = S>) -> Self {
        let mut block = Vec::new();
        for entry in entries {
            let entry = entry.as_ref().as_encoded_bytes();
            assert!(
                !entry.contains(&0),
                "an environment entry holds a NUL: {entry:?}"
            );
            block.extend_from_slice(entry);
            block.push(0);
        }
        Self::from_block(block)
    }

    /// Every entry, in order.
    fn entries(&self) -> impl Iterator<Item = &[u8]> {
        let none = self.body.is_empty();
        self.body.split(|b| *b == 0).filter(move |_| !none)
    }

    /// `NAME=value` entries, split at the first `=`, in order.
    pub fn vars(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.entries().filter_map(|entry| {
            let eq = entry.iter().position(|b| *b == b'=')?;
            Some((os_str(&entry[..eq]), os_str(&entry[eq + 1..])))
        })
    }

    /// Entries without `=` (execve does not forbid them); kept, not dropped.
    pub fn without_equals(&self) -> impl Iterator<Item = &OsStr> {
        self.entries()
            .filter(|entry| !entry.contains(&b'='))
            .map(os_str)
    }

    /// Whether the block has no entries.
    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    /// The value of the first variable called `name`.
    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.vars().find(|(n, _)| *n == name).map(|(_, v)| v)
    }
}

fn os_str(bytes: &[u8]) -> &OsStr {
    use std::os::unix::ffi::OsStrExt;
    OsStr::from_bytes(bytes)
}

/// The four user ids, or the four group ids, of a process: the `Uid:` or
/// `Gid:` line of `/proc/<pid>/status` (real, effective, saved set,
/// filesystem).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ids {
    pub real: u32,
    pub effective: u32,
    pub saved: u32,
    pub fs: u32,
}

impl Ids {
    /// The same id four times.
    pub fn same(id: u32) -> Self {
        Self {
            real: id,
            effective: id,
            saved: id,
            fs: id,
        }
    }

    /// The four ids, in `status` order.
    pub fn all(self) -> [u32; 4] {
        [self.real, self.effective, self.saved, self.fs]
    }
}

impl fmt::Display for Ids {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {}",
            self.real, self.effective, self.saved, self.fs
        )
    }
}

/// What every visible process shows about itself, also when its other
/// `/proc` entries cannot be read (a non-dumpable process, another user's):
/// `/proc/<pid>/status` is world-readable, and so are the owners of the
/// entries below `/proc/<pid>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStatus {
    /// `Name:`: the command name (at most 15 bytes, as the kernel keeps it).
    pub name: OsString,
    /// `Uid:`.
    pub uid: Ids,
    /// `Gid:`.
    pub gid: Ids,
    /// `CapPrm:`: the permitted capability set (bit n is capability n).
    pub cap_permitted: u64,
    /// `CapEff:`: the effective capability set. The kernel shows a
    /// process's fds and maps to a reader of the same user namespace only
    /// if the reader's effective set includes the process's permitted set
    /// (`cap_ptrace_access_check` with `PTRACE_MODE_READ_FSCREDS`), even
    /// when both have the same uid (`systemd --user` holds
    /// `CAP_WAKE_ALARM`).
    pub cap_effective: u64,
    /// `/proc/<pid>/uid_map` (readable for every process), shown relative
    /// to the reader's user namespace. The text is no identity:
    /// two sibling namespaces can show the same mapping. Only the full
    /// identity map (`0 0 4294967295`) is telling, read by a reader whose
    /// own map is that too: the initial user namespace, or a child of it
    /// that only real root can create.
    pub uid_map: UidMap,
    /// The owner of `/proc/<pid>/task/<tid>/fd` for a **live** task of the
    /// process (its `stat`, read before and after, shows it neither ended
    /// nor exiting): the effective uid while the process is dumpable, root
    /// of its memory map's user namespace (0, or the overflow uid 65534
    /// where diskgc's namespace does not map it) while it is not
    /// (fs/proc/base.c `task_dump_owner`). The leader's own entry is not
    /// enough: a leader that called `pthread_exit` has no memory map and
    /// shows root on some kernels while its live threads are dumpable.
    /// `None` when no live task could be found or the owner not read.
    pub fd_dir_owner: Option<u32>,
}

/// What reading `/proc/<pid>/uid_map` gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UidMap {
    /// Its text.
    Map(String),
    /// It does not exist (`ENOENT`: a kernel without user namespaces).
    Absent,
    /// It could not be read for another reason (kept for the notes).
    Unreadable(String),
}

impl UidMap {
    /// The text, if it was read.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Map(text) => Some(text),
            Self::Absent | Self::Unreadable(_) => None,
        }
    }
}

/// An AF_UNIX socket bound to a filesystem name, as the kernel's socket
/// diagnostics report it (`NETLINK_SOCK_DIAG`, `UNIX_DIAG_VFS`): the device
/// and inode of the socket **file** (not of the sockfs inode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BoundSocket {
    pub dev: Dev,
    /// The low 32 bits of the inode number: the kernel reports it as a
    /// `__u32` (`struct unix_diag_vfs`), so callers compare the low 32 bits
    /// of their own (a coincidence of those bits makes a socket file look
    /// bound: the conservative side).
    pub ino: u32,
}

impl BoundSocket {
    /// Whether this may be the socket file `id`: by the low 32 bits of its
    /// inode number alone, whatever the device. The kernel reports the
    /// superblock's device, which `statx` does not show on btrfs
    /// subvolumes (an anonymous device per subvolume) or on an overlay of
    /// several filesystems, so devices are not comparable; ignoring them
    /// only adds matches (a stale socket kept as live), the safe side.
    pub fn is(&self, id: DevIno) -> bool {
        u64::from(self.ino) == id.ino & u64::from(u32::MAX)
    }
}

/// What the held `/proc` says about the PID namespace diskgc runs in,
/// for Owner PID's preconditions (docs/safety.md, Producer liveness).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidNamespace {
    /// `self/ns/pid`, read through the held `/proc`, is on nsfs and has the
    /// initial PID namespace's fixed inode ([`PROC_PID_INIT_INO`]).
    pub initial: bool,
    /// The held `/proc` is mounted with a `hidepid` other than `off`
    /// (`noaccess`, `invisible`, `ptraceable`, or their numbers): it may
    /// hide processes of diskgc's own user.
    pub hidepid: bool,
}

/// The inode of the initial PID namespace (`PROC_PID_INIT_INO` in the
/// kernel's `include/linux/proc_ns.h`; 4026531836). Fixed, unlike every
/// other namespace's inode.
pub const PROC_PID_INIT_INO: u64 = 0xEFFF_FFFC;

/// Everything the in-use check reads of one process ([`read_proc_view`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcView {
    pub fds: Vec<ProcFd>,
    pub maps: Vec<MapEntry>,
    pub cwd: Vec<LinkTarget>,
    pub root: Vec<LinkTarget>,
    pub exe: Vec<LinkTarget>,
    /// The environment, or why it could not be read (kept: the in-use
    /// check decides what a failed environment means).
    pub environ: Result<Environ, ProcError>,
    /// The pytest contract's facts, when the read asked for them
    /// ([`read_proc_view`]).
    pub facts: Option<ViewFacts>,
}

/// What the pytest contract reads of a process within the scan's own reads
/// (docs/safety.md, pytest sessions: every read only adds), so that a
/// child the process forks before it ends is listed and read as well: its
/// uids, its command line, and every value pytest would resolve looked up
/// in its own root ([`Host::proc_root_lookup`]; the values are
/// [`crate::liveness::pytest::wanted_lookups`] of its environment and
/// command line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewFacts {
    pub uids: Result<Ids, ProcError>,
    pub cmdline: Result<Vec<OsString>, ProcError>,
    /// Each lookup: the normalised path, whether it was climbed, and the
    /// answer (never `Gone`: the whole read is `Gone` then).
    pub lookups: Vec<(PathBuf, bool, Result<Vec<DevIno>, ProcError>)>,
}

/// Reads one process for the in-use check through the readers of `host`:
/// fds, memory map, cwd, root, exe, then environment, and with `facts` the
/// pytest contract's [`ViewFacts`]. The first reader that fails, other
/// than the environment's and the facts', is the result: the others are
/// not asked (a process of another user costs one refused read). A reader
/// that finds the process gone makes the whole read `Gone`.
pub fn read_proc_view<H: Host + ?Sized>(
    host: &H,
    pid: Pid,
    facts: bool,
) -> Result<ProcView, ProcError> {
    let fds = host.proc_fds(pid)?;
    let maps = host.proc_maps(pid)?;
    let [cwd, root, exe] = host.proc_links_all(pid)?;
    let environ = host.proc_environ(pid);
    if is_gone(&environ) {
        return Err(ProcError::Gone);
    }
    let facts = if facts {
        let uids = host.proc_uids(pid);
        let cmdline = host.proc_cmdline(pid);
        if is_gone(&uids) || is_gone(&cmdline) {
            return Err(ProcError::Gone);
        }
        let wanted =
            crate::liveness::pytest::wanted_lookups(environ.as_ref().ok(), cmdline.as_deref().ok());
        let mut lookups = Vec::with_capacity(wanted.len());
        for (path, climb) in wanted {
            let answer = host.proc_root_lookup(pid, &path, climb);
            if is_gone(&answer) {
                return Err(ProcError::Gone);
            }
            lookups.push((path, climb, answer));
        }
        Some(ViewFacts {
            uids,
            cmdline,
            lookups,
        })
    } else {
        None
    };
    Ok(ProcView {
        fds,
        maps,
        cwd,
        root,
        exe,
        environ,
        facts,
    })
}

/// Whether a reader found the process gone.
fn is_gone<T>(answer: &Result<T, ProcError>) -> bool {
    matches!(answer, Err(ProcError::Gone))
}

/// Why a `/proc/<pid>` reader returned nothing (docs/safety.md, In use:
/// unreadable or vanishing processes). Callers treat `Denied` and `Failed`
/// as "may be using anything", and `Exhausted` as an error of the run.
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
    /// diskgc itself ran out of file descriptors (`EMFILE`: its own
    /// `RLIMIT_NOFILE`; `ENFILE`: the system's table). Says nothing about
    /// the process being read and would hit the next one too: an error of
    /// the run, never an unreadable process (TASK-166, TASK-185). Its own
    /// variant, so every reader's caller must decide what to do with it
    /// rather than fold it into `Failed` by accident.
    Exhausted(HostError),
}

impl ProcError {
    /// Classifies a failed `/proc/<pid>` access: `Exhausted`, `Denied` or
    /// `Failed`, never `Gone`. A vanished entry (`ENOENT`, `ESRCH`) is
    /// `Failed` here: alone it proves nothing (a lookup can race an exec,
    /// `hidepid` hides live processes), so the readers handle it before it
    /// gets this far.
    pub fn from_host(e: HostError) -> Self {
        match e.kernel_errno() {
            Some(Errno::EMFILE | Errno::ENFILE) => Self::Exhausted(e),
            Some(Errno::EACCES | Errno::EPERM) => Self::Denied(e),
            _ => Self::Failed(e),
        }
    }

    /// `Failed`, unless `e` says diskgc ran out of file descriptors
    /// (`Exhausted`): for a failure whose `EACCES` or `EPERM` is not a
    /// refusal to show the process (a seccomp filter on `pidfd_open`).
    pub fn failed(e: HostError) -> Self {
        if e.is_out_of_descriptors() {
            Self::Exhausted(e)
        } else {
            Self::Failed(e)
        }
    }
}

impl fmt::Display for ProcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gone => f.write_str("process gone"),
            Self::Denied(e) => write!(f, "denied: {e}"),
            Self::Failed(e) | Self::Exhausted(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProcError {}

/// The machine, as diskgc sees it. See the module docs for the rules.
pub trait Host {
    /// An open file descriptor; dropping it closes it (and so releases any
    /// lock taken on it).
    type Fd: fmt::Debug;

    /// A scan-lifetime reference to one process incarnation. The host must
    /// keep the identity pinned until this value is dropped, even if the
    /// numeric pid is subsequently reused.
    type ProcIdentity: fmt::Debug;

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

    /// Read-only environmental preflight of one absolute command path.
    /// Follows executable symlinks as the kernel will at exec, and answers
    /// false for a missing, non-regular or non-executable object. The
    /// command runner receives only a path accepted by this check.
    fn executable_file(&self, path: &Path) -> Result<bool, HostError> {
        Err(HostError::invalid(
            "preflight executable",
            path.display(),
            "this host does not support command executable preflight",
        ))
    }

    /// `PATH` used for command-action preflight, captured from this host's
    /// execution environment. The model supplies a deterministic value.
    fn command_path(&self) -> OsString {
        OsString::new()
    }

    /// Docker client redirects captured from this host's process
    /// environment. The binder rejects any value it cannot prove targets
    /// the local system daemon and default builder.
    fn docker_environment(&self) -> DockerEnvironment {
        DockerEnvironment::default()
    }

    /// Run one constant-argv Docker query and return bounded UTF-8 output.
    /// A query cannot prune and does not ask the pre-release deletion gate.
    fn docker_query(&self, _query: DockerQuery) -> Result<String, HostError> {
        Err(HostError::invalid(
            "query Docker",
            "backend",
            "this host does not provide Docker queries",
        ))
    }

    /// The directory relative PATH entries and relative program paths refer
    /// to before the command child switches to its checked working directory.
    fn command_origin(&self) -> Result<PathBuf, HostError> {
        Err(HostError::invalid(
            "preflight executable",
            "working directory",
            "this host does not support command executable preflight",
        ))
    }
    /// Text of a symlink named by one entry below an already reached
    /// directory. Does not follow the link. Callers compare the entry's
    /// identity around this read when replacement matters.
    fn read_link(&self, dir: &Self::Fd, name: &OsStr) -> Result<PathBuf, HostError>;

    /// The names in the open directory `dir` (`getdents64` until the end, on
    /// a fresh `O_RDONLY | O_DIRECTORY` open of `.` below `dir`, so `dir` may
    /// be an `O_PATH` fd and every call is a listing of its own), without
    /// `.` and `..`, in the order the filesystem returns them; or
    /// [`DirListing::TooMany`] as soon as more than `limit` names were read
    /// (the listing stops there).
    ///
    /// Names only, deliberately: `d_type` can be `DT_UNKNOWN` and is stale
    /// by the time it is used, and every caller must `statx` each name
    /// anyway (identity, owner, type) before acting on it. Entries created
    /// or removed while the listing runs may or may not appear. The limit
    /// bounds memory and time against huge directories and against a writer
    /// that creates entries faster than they are read.
    fn read_dir(&self, dir: &Self::Fd, limit: usize) -> Result<DirListing, HostError>;

    /// At most `limit` bytes from the start of the regular file open as
    /// `file` (fewer at its end); a read that fails part-way is that error.
    /// `file` may be an `O_PATH` fd: `LinuxHost` reopens the object it
    /// refers to read-only and non-blocking (through `/proc/self/fd`), so
    /// what is read is the object the caller checked with
    /// [`Host::statx`]`(Base::Dir(file), "")`, never a name looked up again;
    /// anything but a regular file is refused (`Cause::Invalid`) by the
    /// fd's own type, before anything is opened (a FIFO or a device is
    /// never opened). For small
    /// marker files (a `CACHEDIR.TAG`'s signature), never user data.
    fn read_head(&self, file: &Self::Fd, limit: usize) -> Result<Vec<u8>, HostError>;

    /// The regular file open as `file` (typically an `O_PATH` fd whose
    /// identity the caller checked with [`Host::statx`]`(Base::Dir(file),
    /// "")`), opened again read-only and non-blocking as a new open file
    /// description, so that it can be `flock`ed (`flock` refuses an
    /// `O_PATH` fd). As for [`Host::read_head`], `LinuxHost` reopens the
    /// object the fd refers to (through `/proc/self/fd`), never a name
    /// looked up again, and refuses anything but a regular file
    /// (`Cause::Invalid`) by the fd's own type before anything is opened,
    /// so a FIFO or a device that took a lock file's name is never opened.
    /// For Cargo's lock files (docs/safety.md, `cargo-target`: Reclaim
    /// protocol).
    fn reopen_regular(&self, file: &Self::Fd) -> Result<Self::Fd, HostError>;

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

    /// The first SIGINT or SIGTERM delivered to this run, if any. This is
    /// observed between work units and during the host-lock wait.
    fn interrupted(&self) -> Option<crate::report::Signal>;

    /// The monotonic clock including suspend time.
    fn now(&self) -> MonoTime;

    /// The wall clock (`CLOCK_REALTIME`), on the scale `statx` reports
    /// times on: only for comparing with file times (the idle filters,
    /// docs/reference.md, Predicates). Never for deadlines or freshness,
    /// which use [`Host::now`]: the wall clock can be stepped.
    fn wall_clock(&self) -> Timestamp;

    /// Blocks for `duration`.
    fn sleep(&self, duration: Duration);

    /// The system-wide pressure stall information, read through the held
    /// `/proc` (`pressure/io`, `pressure/memory`, `pressure/cpu`; read-only,
    /// ungated). An error where the kernel has none: built without
    /// `CONFIG_PSI`, booted with `psi=0`, or the files unreadable.
    fn pressure(&self) -> Result<Pressure, HostError>;

    /// Lowers the calling thread's priority to what the background tally
    /// runs at (docs/architecture.md, The tally): `nice 19`
    /// (`setpriority`) and the idle I/O class (`ioprio_set`). For the
    /// calling thread only (Linux applies both per task), so a `watch`
    /// that runs its tally on a thread of its own keeps its reclaim at
    /// normal priority; threads it starts later inherit it. Raising it
    /// again is not expressible.
    fn lower_priority(&self) -> Result<(), HostError>;

    /// The kernel release string (`uname -r`).
    fn kernel_release(&self) -> Result<String, HostError>;

    /// `uname -n`, in the same UTS namespace where the process check runs.
    fn hostname(&self) -> Result<String, HostError>;

    /// The visible processes (thread-group ids; the readers below cover
    /// every task of each).
    fn pids(&self) -> Result<Vec<Pid>, HostError>;

    /// Pin the process currently named by `pid`. `Gone` means there was no
    /// live process at this instant. Other errors fail the scan closed.
    fn proc_identity(&self, pid: Pid) -> Result<Self::ProcIdentity, ProcError>;

    /// Whether that exact incarnation has been released. A zombie still
    /// occupying its pid is not released; a replacement cannot make this
    /// return false. Unknown answers fail the scan closed.
    fn proc_identity_released(&self, identity: &Self::ProcIdentity) -> Result<bool, ProcError>;

    /// The open fds of every task of the process (tasks may have fd tables
    /// of their own), each statted through its link, without duplicates.
    /// Fds closed while they are listed are left out.
    fn proc_fds(&self, pid: Pid) -> Result<Vec<ProcFd>, ProcError>;

    /// The `cwd`, `root` or `exe` of every live task of the process (tasks
    /// may have their own `cwd` and `root`), statted through the link,
    /// without duplicates.
    fn proc_links(&self, pid: Pid, which: ProcLink) -> Result<Vec<LinkTarget>, ProcError>;

    /// [`Host::proc_links`] of `cwd`, `root` and `exe`, in that order (the
    /// order of [`ProcLink::ALL`]). The provided method makes the three
    /// calls; `LinuxHost` reads the three links of every task in the same
    /// rounds, which lists the tasks once instead of three times.
    fn proc_links_all(&self, pid: Pid) -> Result<[Vec<LinkTarget>; 3], ProcError> {
        let [cwd, root, exe] = ProcLink::ALL;
        Ok([
            self.proc_links(pid, cwd)?,
            self.proc_links(pid, root)?,
            self.proc_links(pid, exe)?,
        ])
    }

    /// The process's memory mappings, read through a live task.
    fn proc_maps(&self, pid: Pid) -> Result<Vec<MapEntry>, ProcError>;

    /// The process's initial environment, read through a live task.
    fn proc_environ(&self, pid: Pid) -> Result<Environ, ProcError>;

    /// [`read_proc_view`] of every pid in `pids` (with the pytest facts
    /// when `facts`), results in the same order. The provided method reads
    /// them one after another through the readers above (so a model or a
    /// wrapper sees every reader call); `LinuxHost` reads several processes
    /// at once. Each process is read on its own, so the order and overlap
    /// of reads of different processes changes nothing about any one
    /// result.
    fn proc_views(&self, pids: &[Pid], facts: bool) -> Vec<Result<ProcView, ProcError>> {
        pids.iter()
            .map(|pid| read_proc_view(self, *pid, facts))
            .collect()
    }

    /// What the `/proc` readers have to report since the last call, once:
    /// conditions that cost time but change no answer (a refused fd-table
    /// comparison, TASK-115). [`crate::inuse::scan`] takes them into its
    /// [`crate::inuse::Scan::notes`]. The provided method has none.
    fn take_proc_read_notes(&self) -> Vec<String> {
        Vec::new()
    }

    /// diskgc's own process id (`getpid`), as [`Host::pids`] lists it.
    fn own_pid(&self) -> Pid;

    /// diskgc's effective user id (`geteuid`): whose files it may delete
    /// (docs/safety.md, Privileges), and what `{uid}` stands for in paths.
    fn effective_uid(&self) -> u32;

    /// The uid of the account called `name` in this host's account
    /// database (`getpwnam_r` through the name service), `None` when it
    /// has no such account. Read only for `owners:` names under
    /// `--allow-root`; what uids are acceptable is the caller's policy.
    fn account_uid(&self, name: &str) -> Result<Option<u32>, HostError>;

    /// Lets every child diskgc starts from now on inherit the held lock
    /// `lock` (clears close-on-exec on that one fd, `fcntl(F_SETFD)`), so
    /// the lock stays held until the last holder of a copy closes it (the
    /// host lock, docs/safety.md, Host lock). Nothing is locked or unlocked:
    /// the lock is still released only by closing every copy.
    fn inherit_lock(&self, lock: &Locked<Self::Fd>) -> Result<(), HostError>;

    /// What the in-use check needs to judge a process whose other entries
    /// cannot be read (docs/safety.md, In use), all readable for every
    /// process: one read of `/proc/<pid>/status` (`Name:`, `Uid:`, `Gid:`,
    /// `CapPrm:`, `CapEff:`), `/proc/<pid>/uid_map` (optional), and the
    /// owner of the `fd` directory of a live task (`statx`, not followed;
    /// see [`ProcStatus::fd_dir_owner`]).
    fn proc_status(&self, pid: Pid) -> Result<ProcStatus, ProcError>;

    /// Every AF_UNIX socket bound to a filesystem name in diskgc's network
    /// namespace: the union of two complete `NETLINK_SOCK_DIAG` dumps of
    /// `AF_UNIX` sockets in every state with `UDIAG_SHOW_VFS` (a read-only
    /// query). One dump is not a snapshot: the kernel resumes it by
    /// position within a hash bucket, so a socket closed meanwhile can make
    /// it skip a stable one, without marking the dump interrupted; missing
    /// a socket in both takes such a close during each. Sockets without a
    /// filesystem name (abstract, unnamed) are left out; sockets of other
    /// network namespaces are invisible. A dump that fails, is cut short or
    /// holds a truncated datagram is an error, never a shorter list.
    fn bound_unix_sockets(&self) -> Result<Vec<BoundSocket>, HostError>;

    /// Owner PID's namespace preconditions, read through the held `/proc`
    /// (docs/safety.md, Producer liveness, Owner PID steps 3 and 4).
    fn pid_namespace(&self) -> Result<PidNamespace, HostError>;

    /// The `NSpid:` line of `/proc/<pid>/status`: the process's pid in
    /// every PID namespace from diskgc's down to its own, the first being
    /// `pid`. Readable for every user's process, zombies included, unless
    /// `hidepid` hides it. `Gone` only on the kernel's word (as the other
    /// readers): the process was released.
    fn proc_nspid(&self, pid: Pid) -> Result<Vec<u32>, ProcError>;

    /// Whether a process (live or zombie) holds `pid` in diskgc's PID
    /// namespace now: `pidfd_open(pid)` succeeds (true) or fails with
    /// `ESRCH` (false). Any other answer is an error: only `ESRCH` is the
    /// kernel's word that no process carries the number.
    fn pid_alive(&self, pid: Pid) -> Result<bool, HostError>;

    /// The filesystem type (`fstatfs` `f_type`) of the filesystem holding
    /// `fd`.
    fn fs_magic(&self, fd: &Self::Fd) -> Result<u64, HostError>;

    /// The `Uid:` line of `/proc/<pid>/status` (real, effective, saved,
    /// filesystem), read as [`Host::proc_nspid`] reads that file: readable
    /// for every user's process unless `hidepid` hides it; `Gone` only on
    /// the kernel's word. Who a liveness contract's process set holds
    /// (docs/safety.md, Producer liveness, A process's paths).
    fn proc_uids(&self, pid: Pid) -> Result<Ids, ProcError>;

    /// The process's command line (`/proc/<pid>/cmdline`, its argument
    /// vector split at NUL bytes), read through a live task as the
    /// environment is. World-readable on Linux (no ptrace check), so
    /// `Denied` comes only with `hidepid`. Empty for a process without a
    /// memory map (a zombie, a kernel thread). For the pytest contract's
    /// "a pytest is running" and naming tests (docs/safety.md, pytest
    /// sessions).
    fn proc_cmdline(&self, pid: Pid) -> Result<Vec<OsString>, ProcError>;

    /// Resolves the absolute `path` in the process's own root, as the
    /// kernel resolves it for that process: one `openat2` (`O_PATH`) from
    /// `/proc/<pid>/root` with `RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS`
    /// (`..` clamps at that root; symlinks are followed inside it), `EAGAIN`
    /// retried at most [`ROOT_LOOKUP_EAGAIN_RETRIES`] times, then a `statx`
    /// of the result with `AT_NO_AUTOMOUNT | AT_STATX_DONT_SYNC`. The answer
    /// starts with the object's (device, inode); with `climb`, it is
    /// followed by those of the directories reached by `..` from it, by fd
    /// (in diskgc's view, which only adds ancestors), up to the fixpoint
    /// where `..` returns the same object, at most [`CLIMB_LEVELS_MAX`]
    /// levels (more is an error). A non-directory is not climbed. Only
    /// the temp-directory and `--basetemp` values pytest itself resolves
    /// are ever looked up this way (docs/safety.md, A process's paths).
    /// `Gone`: the process ended (the kernel's word, as for the other
    /// readers); any other error: the value is unresolvable. Only an error
    /// of the path walk itself carries an errno (`ENOENT`, `ENOTDIR`: the
    /// path is absent in the process's view; `ESRCH` only below a root on
    /// procfs); a root that cannot be opened, and any failure after the
    /// object was opened, carry none, so no caller reads them as absence.
    fn proc_root_lookup(
        &self,
        pid: Pid,
        path: &Path,
        climb: bool,
    ) -> Result<Vec<DevIno>, ProcError>;

    // ---- diskgc's own state (docs/architecture.md, The Host port) ----

    /// The UUID of the filesystem on the block device `dev`, where the
    /// system names one (`/dev/disk/by-uuid`): what the snapshot's
    /// [`FsKey`] prefers, since device numbers can change between boots.
    /// `None`: no UUID is known (tmpfs, btrfs, whose files' `st_dev` is
    /// not its block device, or no udev links). Read-only.
    fn fs_uuid(&self, dev: Dev) -> Result<Option<String>, HostError>;

    /// The kernel's boot id (`/proc/sys/kernel/random/boot_id`): a
    /// snapshot of a filesystem without a UUID is valid only in the boot
    /// that wrote it. Read-only.
    fn boot_id(&self) -> Result<String, HostError>;

    /// Opens the snapshot's single-writer lock file `index-<key>.lock` in
    /// diskgc's state directory, creating the directory (mode 0700) and
    /// the file (mode 0600, read-only open, never written, never renamed)
    /// if absent, for [`Host::flock_try`] or [`Host::flock_wait`] only.
    /// The state directory is the implementation's own, never a name from
    /// the caller; it must be a directory owned by diskgc's user and
    /// writable by no one else, and the file a regular file of that user,
    /// reached without following a symlink. Ungated: diskgc's own cache,
    /// not user data. One of the two operations that create anything
    /// outside [`Delete`] (docs/safety.md, Pre-release sandbox gate).
    fn state_lock(&self, key: &FsKey) -> Result<StateLock<Self::Fd>, HostError>;

    /// The snapshot file `index-<key>` in the state directory, without
    /// following a symlink, and its contents if it is a regular file of
    /// at most `limit` bytes (a larger one is never read). `None`: there
    /// is no state directory or no such file. Its owner and mode are the
    /// caller's to judge. Read-only.
    fn state_read(&self, key: &FsKey, limit: u64) -> Result<Option<StateFile>, HostError>;

    /// Replaces the snapshot `index-<key>` with `bytes`, atomically: a new
    /// file (never `O_TRUNC`, mode 0600) is filled completely before it
    /// is linked in and renamed onto `index-<key>`, the only name it ever
    /// renames onto, so a reader sees the old file or the new one and a
    /// failed write leaves the old one. `lock` must be this key's
    /// single-writer lock from [`Host::state_lock`], held exclusively
    /// (`ModelHost` fails the test otherwise). No `fsync`. Ungated, as
    /// [`Host::state_lock`].
    fn state_write(
        &self,
        lock: &Locked<Self::Fd>,
        key: &FsKey,
        bytes: &[u8],
    ) -> Result<(), HostError>;
}

/// Which filesystem a snapshot is for: it names `index-<key>` and
/// `index-<key>.lock` in diskgc's state directory (docs/architecture.md,
/// The tally: Snapshot). A typed key, never a name from the caller: its
/// text is `uuid-` and the filesystem's UUID in lowercase (hex digits and
/// dashes), or `dev-<major>-<minor>`, so it can hold no `/`, `.` or `..`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FsKey(String);

impl FsKey {
    /// The key of the filesystem with this UUID; `None` unless it is 1 to
    /// 64 hex digits and dashes.
    pub fn uuid(uuid: &str) -> Option<Self> {
        let ok = (1..=64).contains(&uuid.len())
            && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
        ok.then(|| Self(format!("uuid-{}", uuid.to_ascii_lowercase())))
    }

    /// The key of a filesystem without a known UUID: its device number.
    pub fn dev(dev: Dev) -> Self {
        Self(format!("dev-{}-{}", dev.major, dev.minor))
    }

    /// A key as [`Self::as_str`] wrote it; `None` for any other text.
    pub fn parse(text: &str) -> Option<Self> {
        if let Some(uuid) = text.strip_prefix("uuid-") {
            return Self::uuid(uuid).filter(|k| k.0 == text);
        }
        let (major, minor) = text.strip_prefix("dev-")?.split_once('-')?;
        let key = Self::dev(Dev {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        });
        // Only the canonical spelling (no sign, no leading zero).
        (key.0 == text).then_some(key)
    }

    /// Whether the key is a UUID (otherwise a device number, valid only
    /// within one boot for a filesystem such as tmpfs).
    pub fn has_uuid(&self) -> bool {
        self.0.starts_with("uuid-")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The snapshot's file name.
    pub fn snapshot_name(&self) -> String {
        format!("index-{}", self.0)
    }

    /// The single-writer lock's file name.
    pub fn lock_name(&self) -> String {
        format!("index-{}.lock", self.0)
    }

    /// The name a complete new snapshot is linked in under before it is
    /// renamed onto [`Self::snapshot_name`].
    pub fn new_name(&self) -> String {
        format!("index-{}.new", self.0)
    }
}

impl fmt::Display for FsKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The single-writer lock file [`Host::state_lock`] opened (not locked
/// yet), and the device of the state directory's filesystem: a writer
/// on the filesystem it reclaims guards the goal (docs/architecture.md,
/// Snapshot: Never at the cost of the goal; [`Host::statvfs`] on `fd`
/// samples that filesystem).
#[derive(Debug)]
pub struct StateLock<F> {
    pub fd: F,
    pub dir_dev: Dev,
}

/// What [`Host::state_read`] found at `index-<key>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateFile {
    pub file_type: FileType,
    pub uid: u32,
    /// The permission bits (`st_mode & 0o7777`).
    pub mode: u32,
    pub size: u64,
    /// The contents: `None` unless a regular file of at most the limit.
    pub bytes: Option<Vec<u8>>,
}

/// How often [`Host::proc_root_lookup`] retries an `openat2` that failed
/// with `EAGAIN` (a rename raced the `RESOLVE_IN_ROOT` lookup) before the
/// value is unresolvable (docs/safety.md, A process's paths).
pub const ROOT_LOOKUP_EAGAIN_RETRIES: usize = 3;

/// The defensive bound of a `..` climb without a fixpoint (docs/safety.md,
/// A process's paths): a real tree is far shallower than this (`PATH_MAX`
/// of 4096 bytes allows at most 2048 levels of one-byte names).
pub const CLIMB_LEVELS_MAX: usize = 4096;

/// The result of [`Host::read_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirListing {
    /// Every name (at most the limit).
    Names(Vec<OsString>),
    /// More names than the limit: the listing stopped.
    TooMany,
}

/// What [`Delete::unlinkat`] removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unlink {
    /// Anything but a directory (flags 0).
    NonDir,
    /// An empty directory (`AT_REMOVEDIR`).
    Dir,
}

/// Output prefixes are limited so a forwarded output segment has a fixed
/// maximum size independent of profile configuration.
pub const MAX_COMMAND_OUTPUT_PREFIX_BYTES: usize = 256;

/// One already-resolved command action. The caller resolves `executable`
/// during environmental preflight, substitutes `{path}` in `arguments` and
/// `environment`, and checks the candidate before handing its open directory
/// fd to [`Delete::run_command`]. The Linux implementation never invokes a
/// shell implicitly. `deadline` is the tier's monotonic deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRun {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
    /// For Docker prune, pin the validated local socket inode and recheck
    /// this daemon identity through that same inode before starting prune.
    pub docker_info: Option<crate::docker_backend::DockerInfo>,
    /// Human label supplied by the executor (tier and profile). Every
    /// child-output line is written to stderr with this prefix; child bytes
    /// never reach diskgc's result-only stdout.
    pub output_prefix: String,
    pub deadline: MonoTime,
}

/// Closed set of read-only Docker client queries. LinuxHost alone maps each
/// variant to constant argv; profiles and config cannot supply shell words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DockerQuery {
    Version,
    ClientVersion,
    ContextShow,
    ContextEndpoint,
    Info,
}

/// Client variables that can redirect a Docker command away from its active
/// context. The backend binder checks these
/// before querying or pruning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockerEnvironment {
    pub host: Option<OsString>,
    pub context: Option<OsString>,
}

/// A completed command action. The caller reports a non-zero exit, signal,
/// or timeout as an action error; none of those is a successful reclaim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandExit {
    Exited(i32),
    Signaled(i32),
    TimedOut,
}

/// Whether a deleting command actually started. Once `spawn` succeeds,
/// errors from output forwarding or waiting still describe an attempted
/// action: the caller must sample space and include the action in its report.
#[derive(Debug)]
pub enum CommandInvocation {
    NotStarted(HostError),
    DeadlineBeforeStart,
    Started(Result<CommandExit, HostError>),
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
/// with a [`Cause::SandboxGate`] error in development builds; release
/// packages, from 0.1 onwards, are built without the gate (docs/safety.md,
/// Pre-release sandbox gate).
pub trait Delete: Host {
    /// Whether the deleting operations below would act at all: `LinuxHost`
    /// answers with the pre-release sandbox gate's decision (a
    /// [`Cause::SandboxGate`] error outside a diskgc test container in
    /// development builds), without deleting or changing anything. A
    /// reclaim asks it first, so a closed gate stops it before it measures,
    /// locks or plans (docs/safety.md, Pre-release sandbox gate); it is on
    /// `Delete`, so a dry run, which is given only a [`Host`], cannot even
    /// ask.
    fn may_delete(&self) -> Result<(), HostError>;

    /// `unlinkat(dir, name, flags)`: removes the entry `name` of the open
    /// directory `dir`. `name` is a single component ([`is_single_name`]:
    /// no `/`, not `.` or `..`). Linux has no unlink-by-inode, so callers check the entry's
    /// identity with [`Host::statx`]`(Base::Dir(dir), name)` right before,
    /// as the previous port call (docs/safety.md, Deletion); `ModelHost`
    /// fails a test that does not.
    fn unlinkat(&self, dir: &Self::Fd, name: &OsStr, what: Unlink) -> Result<(), HostError>;

    /// Add owner write to the open **directory** `dir`, asking the kernel to
    /// retain its other current mode bits. The kernel may clear setgid under
    /// its normal permission rules. The fd must be open for reading. Read its current mode,
    /// check `deadline` immediately before `fchmod`, and return `false`
    /// without changing it if time expired. Linux cannot atomically OR a
    /// permission bit, so an unrelated concurrent chmod remains a race.
    /// This modification is gated like deletion. The engine's
    /// `make_writable` is its only production caller.
    fn fchmod(&self, dir: &Self::Fd, deadline: Option<MonoTime>) -> Result<bool, HostError>;

    /// Runs a deleting command with `cwd` set by `fchdir` to the checked
    /// directory fd, in its own process group. `executable` must be an
    /// absolute path obtained by preflight; the caller performs the
    /// candidate's walk and last in-use check before this call. LinuxHost
    /// asks the sandbox gate before starting a child, terminates its process
    /// group at `deadline`, and waits for the leader. The caller must have
    /// called [`Host::inherit_lock`] on the host lock before this call: the
    /// child inherits that fd across exec and holds the lock if diskgc dies.
    fn run_command(&self, cwd: &Self::Fd, run: &CommandRun) -> CommandInvocation;
}

#[cfg(test)]
mod tests {
    use super::*;

    use proptest::prelude::*;
    use std::os::unix::ffi::OsStrExt;

    /// Environment entries without NUL, from a small alphabet so that `=`
    /// and empty entries are common.
    fn entries() -> impl Strategy<Value = Vec<Vec<u8>>> {
        prop::collection::vec(
            prop::collection::vec(prop::sample::select(vec![b'A', b'=', b'/', 0xff]), 0..4),
            0..6,
        )
    }

    /// The kernel's block for `entries`: each ended by a NUL.
    fn kernel_block(entries: &[Vec<u8>]) -> Vec<u8> {
        entries
            .iter()
            .flat_map(|e| e.iter().copied().chain([0]))
            .collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1024))]

        /// TASK-115: the byte-block [`Environ`] gives back exactly the
        /// entries of the kernel's block (one trailing NUL dropped: so a
        /// block of one empty entry has none, as the old parser read it),
        /// [`Environ::from_entries`] builds that very block, and every
        /// entry is in exactly one of `vars` and `without_equals`, in order.
        #[test]
        fn environ_round_trips_the_kernels_block(entries in entries()) {
            let block = kernel_block(&entries);
            let env = Environ::from_block(block.clone());
            let expected: Vec<&[u8]> = if entries == [Vec::<u8>::new()] {
                Vec::new()
            } else {
                entries.iter().map(Vec::as_slice).collect()
            };
            prop_assert_eq!(env.entries().collect::<Vec<_>>(), expected.clone());
            let built = Environ::from_entries(
                entries.iter().map(|e| OsStr::from_bytes(e)),
            );
            prop_assert_eq!(&built, &env);
            // Split at the first `=`: a name never holds one.
            prop_assert!(env.vars().all(|(n, _)| !n.as_bytes().contains(&b'=')));
            let vars: Vec<Vec<u8>> = env
                .vars()
                .map(|(n, v)| [n.as_bytes(), b"=", v.as_bytes()].concat())
                .collect();
            let plain: Vec<&[u8]> = env.without_equals().map(OsStr::as_bytes).collect();
            let want_vars: Vec<Vec<u8>> =
                expected.iter().filter(|e| e.contains(&b'=')).map(|e| e.to_vec()).collect();
            let want_plain: Vec<&[u8]> =
                expected.iter().copied().filter(|e| !e.contains(&b'=')).collect();
            prop_assert_eq!(vars, want_vars);
            prop_assert_eq!(plain, want_plain);
        }
    }

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
        // diskgc out of descriptors is never a fact about the process.
        for exhausted in [Errno::EMFILE, Errno::ENFILE] {
            assert!(matches!(
                ProcError::from_host(e(exhausted)),
                ProcError::Exhausted(_)
            ));
            assert!(matches!(
                ProcError::failed(e(exhausted)),
                ProcError::Exhausted(_)
            ));
        }
        // `failed` keeps a refusal that is not the process's as `Failed`.
        assert!(matches!(
            ProcError::failed(e(Errno::EPERM)),
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
             missing; a development build deletes only inside a diskgc test container (release \
             packages, from 0.1, are built without the gate)"
        );
    }

    #[test]
    fn every_named_errno_reads_back_from_its_name() {
        for errno in Errno::NAMED {
            let name = errno.name().expect("named");
            assert_eq!(Errno::from_name(name), Some(*errno));
        }
        assert_eq!(Errno::from_name("EWHATEVER"), None);
        // Every value name() knows is listed.
        for n in 0..200 {
            assert_eq!(
                Errno(n).name().is_some(),
                Errno::NAMED.contains(&Errno(n)),
                "{n}"
            );
        }
    }

    /// The kernel reports a bound socket file's inode as 32 bits and its
    /// superblock's device: the match compares the low 32 bits of the
    /// file's inode, on any device.
    #[test]
    fn a_bound_socket_matches_by_the_low_32_bits_on_any_device() {
        let dev = Dev {
            major: 0,
            minor: 42,
        };
        let socket = BoundSocket { dev, ino: 7 };
        assert!(socket.is(DevIno { dev, ino: 7 }));
        assert!(socket.is(DevIno {
            dev,
            ino: (1 << 32) | 7
        }));
        assert!(!socket.is(DevIno { dev, ino: 8 }));
        // statx on a btrfs subvolume or a mixed overlay shows another
        // device than the superblock's the kernel reports: still bound.
        assert!(socket.is(DevIno {
            dev: Dev {
                major: 0,
                minor: 43
            },
            ino: 7
        }));
    }

    #[test]
    fn host_errors_name_the_errno() {
        let e = HostError::errno("openat2", "/tmp/x", Errno::ELOOP);
        assert_eq!(e.to_string(), "openat2 /tmp/x: ELOOP (40)");
        assert_eq!(Errno(4095).to_string(), "errno 4095");
    }
}
