//! `LinuxHost`: the [`Host`] port on the running kernel.
//!
//! `openat2`, `statx`, `fstatvfs`, `flock`, `unlinkat`, `fchmod`, `uname` and
//! `clock_gettime` go through rustix's `linux_raw` backend, i.e. raw
//! syscalls without a libc wrapper, so the static musl build behaves exactly
//! like the gnu build (and needs no libc support for `openat2` or `statx`).
//!
//! An fd is an [`OwnedFd`] and nothing else: dropping a [`LinuxFd`] closes
//! it, which is the only way a lock taken on it is ever released. No code
//! here calls `flock(LOCK_UN)` (docs/safety.md, Host lock).
//!
//! `/proc` is opened once, by [`LinuxHost::new`], which checks it is the
//! procfs of this process's own pid namespace; every later `/proc` access
//! (pids, task listings, `status`, `stat`, fd directories, magic links,
//! `maps`, `environ`, `mountinfo`) is relative to that held fd
//! ([`ProcRoot`]), so all of them reach the mount that was checked, for the
//! life of the host, whatever is mounted on `/proc` later. A source-scanning
//! test keeps any other `/proc` path out of this crate.
//!
//! The deleting operations ([`Delete`]) run the pre-release sandbox gate
//! (`sandbox::permit`) before anything else; see `impl Delete for
//! LinuxHost`.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use diskgc_core::host::{
    Access, Base, BoundSocket, CommandExit, CommandInvocation, CommandRun, Delete, Dev, DevIno,
    DirListing, DockerEnvironment, DockerQuery, Environ, Errno, FileType, FsKey, Host, HostError,
    LinkTarget, LockMode, Locked, MAX_COMMAND_OUTPUT_PREFIX_BYTES, MakeUserLock, MapEntry,
    MonoTime, MountEntry, OpenHow, PROC_PID_INIT_INO, Pid, PidNamespace, Pressure, ProcError,
    ProcFd, ProcLink, ProcStatus, ProcView, Space, Stat, StateFile, StateLock, Timestamp, TryLock,
    UidMap, Unlink, UserLock, UserLockError, WaitLock, is_single_name, read_proc_view,
};
use diskgc_core::report::Signal;
use rustix::event::PollFlags;
use rustix::fs::{AtFlags, CWD, FlockOperation, Mode, OFlags, PROC_SUPER_MAGIC, ResolveFlags};

use crate::mountinfo::{hidepid_of, parse_mountinfo};
use crate::procfs::{
    StatusFields, list_in_one_call, parse_maps, parse_nspid, parse_nspid_levels, parse_status,
    parse_status_ids, read_confirmed, read_in_one_call, read_two_passes,
};
use crate::sandbox::{self, Permit};
use crate::sockdiag;
use crate::stat::{STATX_MASK, stat_from_statx};
use crate::tasks::{
    Listed, Listing, PidfdPoll, Polled, Process, StatusRead, TaskStat, Tasks, Want, collect,
    is_vanished, never_shared, owner_through_live_task, parse_task_stat,
};

/// Where procfs is expected. Named only where [`LinuxHost::new`] opens it
/// and in messages ([`ProcRoot::shown`]); every access goes through the
/// held fd (checked by the test `procfs_is_reached_only_through_the_held_fd`).
const PROC: &str = "/proc";

/// First and longest pause between attempts while waiting for a lock.
const LOCK_POLL_FIRST: Duration = Duration::from_millis(10);
const LOCK_POLL_MAX: Duration = Duration::from_millis(100);

/// A signal handler may only record the signal. The executor does all I/O,
/// reporting and cleanup after it observes this lock-free flag.
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);

extern "C" fn note_interrupt(signal: i32) {
    let _ = INTERRUPTED.compare_exchange(0, signal, Ordering::Relaxed, Ordering::Relaxed);
}

/// The sole ungated signal-disposition wrapper. It changes only this
/// process's SIGINT/SIGTERM handlers, never a filesystem or child process.
/// The source scan counts every raw libc token here for explicit review.
fn install_interrupt_handlers_ungated() -> Result<(), HostError> {
    // SAFETY: sigaction writes only our process's signal dispositions;
    // the handler itself performs one lock-free atomic operation.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = note_interrupt as *const () as usize;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(HostError::invalid(
                "sigemptyset",
                "signal handler mask",
                std::io::Error::last_os_error().to_string(),
            ));
        }
        if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) != 0 {
            return Err(HostError::invalid(
                "sigaction",
                "SIGINT",
                std::io::Error::last_os_error().to_string(),
            ));
        }
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0 {
            return Err(HostError::invalid(
                "sigaction",
                "SIGTERM",
                std::io::Error::last_os_error().to_string(),
            ));
        }
    }
    Ok(())
}

/// `watch`'s SIGHUP handler (docs/reference.md, Watch signals): the same
/// flag-setting handler as SIGINT and SIGTERM, unless SIGHUP was ignored
/// when the process started (`nohup`), when it stays ignored. Returns
/// whether it was installed. Ungated like the handlers above.
fn install_hangup_handler_ungated() -> Result<bool, HostError> {
    let failed = |op: &'static str| {
        HostError::invalid(op, "SIGHUP", std::io::Error::last_os_error().to_string())
    };
    // SAFETY: sigaction reads and writes only our process's disposition
    // of SIGHUP; the handler performs one lock-free atomic operation.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut old) != 0 {
            return Err(failed("sigaction"));
        }
        if old.sa_sigaction == libc::SIG_IGN {
            return Ok(false);
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = note_interrupt as *const () as usize;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(failed("sigemptyset"));
        }
        if libc::sigaction(libc::SIGHUP, &action, std::ptr::null_mut()) != 0 {
            return Err(failed("sigaction"));
        }
    }
    Ok(true)
}

/// An open file descriptor (close-on-exec). Dropping it closes it.
///
/// `shown` is the path it was opened by (its base's path joined with the
/// relative path), and its [`Display`](fmt::Display) is that path. It exists
/// for messages only, so that an error names what a user knows rather than
/// an fd number (TASK-140): the entry may have been renamed or replaced
/// since, so nothing may look it up again or judge by it; the fd is the
/// identity.
#[derive(Debug)]
pub struct LinuxFd {
    fd: OwnedFd,
    shown: PathBuf,
}

impl AsFd for LinuxFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl fmt::Display for LinuxFd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.shown.display().fmt(f)
    }
}

/// The real host. Made only by [`LinuxHost::new`], which checks what the
/// `/proc` readers stand on.
#[derive(Debug)]
pub struct LinuxHost {
    /// The procfs mount checked at construction (`O_PATH` directory).
    proc: OwnedFd,
    /// `kcmp` refusals since the last [`Host::take_proc_read_notes`], and
    /// the errno of the latest: each one cost a full fd-table read that a
    /// comparison would have spared (TASK-115). Reported, never an error.
    kcmp_refused: std::sync::atomic::AtomicU64,
    kcmp_refused_errno: AtomicI32,
    #[cfg(not(feature = "release-without-sandbox-gate"))]
    clock_offset_seconds: u64,
    /// diskgc's state directory ([`LinuxHost::with_state_dir`]); `None`:
    /// the state operations fail, and no snapshot is read or written.
    state_dir: Option<PathBuf>,
    /// `$HOME`, for the per-user lock only when the account database has
    /// no home for diskgc's user ([`LinuxHost::with_home_fallback`]).
    home_fallback: Option<PathBuf>,
    /// diskgc's user's home from the account database, looked up once.
    account_home: std::sync::OnceLock<Option<PathBuf>>,
}

impl LinuxHost {
    /// Install the cooperative handlers before an `ensure` starts. The
    /// binary runs one command then exits, so the first signal is retained.
    pub fn install_interrupt_handlers() -> Result<(), HostError> {
        install_interrupt_handlers_ungated()
    }

    /// `watch`'s handlers: SIGINT and SIGTERM as for `ensure`, and SIGHUP,
    /// which stops `watch` the same way, unless it was ignored at start
    /// (`nohup`: then it stays ignored, and this returns `false`).
    pub fn install_watch_handlers() -> Result<bool, HostError> {
        install_interrupt_handlers_ungated()?;
        install_hangup_handler_ungated()
    }

    /// Checks, failing closed, that the `/proc` readers can work here:
    ///
    /// - the held `/proc` directory is a procfs mount (`fstatfs`), rather
    ///   than a directory imitating the process files;
    /// - `/proc` is the one of this process's own pid namespace: the
    ///   `NSpid:` line of `/proc/self/status` has exactly one field, equal
    ///   to `getpid()` (see [`check_proc_namespace`]). Under another
    ///   namespace's `/proc` (`unshare -p` without remounting it, a
    ///   container sharing the host's `/proc`), the pids from [`Host::pids`]
    ///   are numbers of that namespace, while `pidfd_open` resolves numbers
    ///   in this one: it would open another process, or none (`ESRCH`, so a
    ///   live process would be `Gone`). Comparing `/proc/self` with
    ///   `getpid()` is not enough: the two numbers can be equal by chance
    ///   (or by `clone3` `set_tid`). `/proc/self/status` not existing
    ///   (`ENOENT`: this process is not visible in that `/proc`, or none is
    ///   mounted) counts as a mismatch.
    /// - `pidfd_open` works (on this process): it is the only source of
    ///   `Gone`. Without it (`ENOSYS` or `EPERM` from a seccomp filter or a
    ///   sandbox) every process would be `Failed` and nothing reclaimed, so
    ///   it is refused here instead of degrading quietly.
    ///
    /// `/proc` is opened once, here, and the fd kept: the check and every
    /// later `/proc` access go through it ([`ProcRoot`]), so they all reach
    /// the mount that was checked, for the life of the host. (A mount made
    /// on `/proc` later, say a privileged process unmounting it and so
    /// exposing an ancestor namespace's procfs, is never seen; while the fd
    /// is held, the checked mount cannot be unmounted except lazily, and a
    /// lazily detached mount stays reachable through the fd.)
    pub fn new() -> Result<Self, HostError> {
        Self::with_proc(open_procfs()?)
    }

    /// [`LinuxHost::new`] on an open procfs directory.
    fn with_proc(proc: OwnedFd) -> Result<Self, HostError> {
        let own = rustix::process::getpid();
        let status = ProcRoot(proc.as_fd()).read(SELF_STATUS);
        check_proc_namespace(own.as_raw_nonzero().get().cast_unsigned(), status)?;
        check_pidfd_open(
            rustix::process::pidfd_open(own, rustix::process::PidfdFlags::empty())
                .map(drop)
                .map_err(errno_of),
        )?;
        #[cfg(not(feature = "release-without-sandbox-gate"))]
        let clock_offset_seconds = sandbox::clock_offset().map_err(|why| {
            HostError::invalid("read clock offset", sandbox::CLOCK_OFFSET_PATH, why)
        })?;
        Ok(Self {
            proc,
            kcmp_refused: std::sync::atomic::AtomicU64::new(0),
            kcmp_refused_errno: AtomicI32::new(0),
            #[cfg(not(feature = "release-without-sandbox-gate"))]
            clock_offset_seconds,
            state_dir: None,
            home_fallback: None,
            account_home: std::sync::OnceLock::new(),
        })
    }

    /// The host with diskgc's state directory at the absolute `dir`
    /// (`$XDG_STATE_HOME/diskgc`, chosen by the binary:
    /// `diskgc_core::snapshot::state_dir`), where the state operations keep
    /// the index snapshot (docs/architecture.md, The Host port: diskgc's
    /// own state). They never name anything else.
    pub fn with_state_dir(mut self, dir: PathBuf) -> Self {
        assert!(dir.is_absolute(), "a state directory must be absolute");
        self.state_dir = Some(dir);
        self
    }

    /// The host with `$HOME` (absolute) as the home of the per-user lock
    /// for when the account database has none for diskgc's user (a static
    /// musl build without nscd cannot resolve NSS-only accounts):
    /// docs/safety.md, Host lock. Nothing else reads it.
    pub fn with_home_fallback(mut self, home: PathBuf) -> Self {
        assert!(home.is_absolute(), "a home must be absolute");
        self.home_fallback = Some(home);
        self
    }

    /// The home of diskgc's user for the per-user lock: the account
    /// database's (looked up once), otherwise the `$HOME` fallback.
    fn lock_home(&self) -> Option<PathBuf> {
        let uid = rustix::process::geteuid().as_raw();
        self.account_home
            .get_or_init(|| account_home(uid).ok().flatten())
            .clone()
            .or_else(|| self.home_fallback.clone())
    }

    fn state_dir(&self) -> Result<&Path, HostError> {
        self.state_dir.as_deref().ok_or_else(|| {
            HostError::invalid(
                "state",
                "$XDG_STATE_HOME/diskgc",
                "no state directory: neither $XDG_STATE_HOME nor $HOME is an absolute path",
            )
        })
    }

    /// The contents of diskgc's configuration file at the absolute `path`
    /// (docs/reference.md, Configuration file), at most `limit` bytes; a
    /// larger file is an error, never a truncated read.
    ///
    /// Not a [`Host`] method: configuration is read once, by the binary,
    /// before anything touches candidates, and parsing it is pure
    /// (`diskgc_core::config`). Unlike candidate paths, symlinks **are**
    /// followed: packaged configurations are commonly symlinks (NixOS
    /// `/etc` entries, home-manager files); what the file says is
    /// validated, not where it lives. It is opened `O_PATH` first and its
    /// type checked on that fd before it is opened for reading
    /// (see [`read_config_file`]), so a FIFO or a device at the path is
    /// refused without being opened. Missing: `ENOENT` (or `ENOTDIR`), which
    /// the lookup treats as "not there".
    pub fn read_config_file(&self, path: &Path, limit: usize) -> Result<Vec<u8>, HostError> {
        read_config_file(path, limit)
    }
}

/// The one place `/proc` is opened by path: an `O_PATH` directory fd,
/// checked to be procfs (`fstatfs`), through which every later `/proc`
/// access goes ([`ProcRoot`]).
fn open_procfs() -> Result<OwnedFd, HostError> {
    let proc = rustix::fs::open(
        PROC,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| HostError::errno("open", PROC, errno_of(e)))?;
    let fs =
        rustix::fs::fstatfs(&proc).map_err(|e| HostError::errno("fstatfs", PROC, errno_of(e)))?;
    if fs.f_type != PROC_SUPER_MAGIC {
        return Err(HostError::invalid(
            "fstatfs",
            PROC,
            format!(
                "filesystem type {:#x} is not procfs ({PROC_SUPER_MAGIC:#x}); \
                 /proc must be a procfs mount",
                fs.f_type
            ),
        ));
    }
    Ok(proc)
}

/// Read a configuration without constructing `LinuxHost`. `diskgc check`
/// uses this in Nix build sandboxes where process inspection and a minimum
/// kernel are irrelevant. The O_PATH/fstat/reopen sequence keeps FIFO and
/// device files from being opened while still following packaged symlinks.
pub fn read_config_file(path: &Path, limit: usize) -> Result<Vec<u8>, HostError> {
    read_config_file_policy(path, limit, false)
}

/// Read a policy for privileged use. The original lookup path and every
/// symlink target must be protected by root-owned, non-writable ancestors.
/// The file is held by fd while it is checked and read.
pub fn read_root_config_file(path: &Path, limit: usize) -> Result<Vec<u8>, HostError> {
    read_config_file_policy(path, limit, true)
}

fn read_config_file_policy(
    path: &Path,
    limit: usize,
    strict_modes: bool,
) -> Result<Vec<u8>, HostError> {
    let subject = path.display().to_string();
    if !path.is_absolute() {
        return Err(HostError::invalid(
            "open",
            &subject,
            "a configuration file path must be absolute",
        ));
    }
    let fd = rustix::fs::open(path, OFlags::PATH | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| HostError::errno("open", &subject, errno_of(e)))?;
    let kind =
        rustix::fs::fstat(&fd).map_err(|e| HostError::errno("fstat", &subject, errno_of(e)))?;
    if FileType::from_mode(kind.st_mode) != FileType::Regular {
        return Err(HostError::invalid(
            "reopen",
            &subject,
            "not a regular file (only a regular file is read as a configuration file)",
        ));
    }
    if strict_modes {
        check_root_config_modes(path, kind.st_dev, kind.st_ino)?;
    }
    let fd_path = format!("{PROC}/self/fd/{}", fd.as_raw_fd());
    let file = rustix::fs::open(
        &*fd_path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| HostError::errno("open", &subject, errno_of(e)))?;
    let opened =
        rustix::fs::fstat(&file).map_err(|e| HostError::errno("fstat", &subject, errno_of(e)))?;
    if opened.st_dev != kind.st_dev || opened.st_ino != kind.st_ino {
        return Err(HostError::invalid(
            "reopen",
            &subject,
            "the configuration file changed between checking its type and reading it",
        ));
    }
    let mut content = Vec::new();
    std::fs::File::from(file)
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut content)
        .map_err(|e| io_error("read", &subject, &e))?;
    if content.len() > limit {
        return Err(HostError::invalid(
            "read",
            &subject,
            format!("larger than {limit} bytes: not read as a configuration file"),
        ));
    }
    Ok(content)
}

/// Check both the name used for lookup and its canonical target. Checking
/// every original prefix catches a link through an untrusted directory;
/// checking each prefix's target catches symlinks into untrusted trees.
fn check_root_config_modes(path: &Path, dev: u64, ino: u64) -> Result<(), HostError> {
    let subject = path.display().to_string();
    for prefix in path.ancestors() {
        let target =
            std::fs::canonicalize(prefix).map_err(|e| io_error("canonicalize", &subject, &e))?;
        for component in target.ancestors() {
            let metadata =
                std::fs::metadata(component).map_err(|e| io_error("stat", &subject, &e))?;
            if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                return Err(HostError::invalid(
                    "StrictModes",
                    &subject,
                    format!(
                        "{} must be root-owned and not writable by group or others",
                        component.display()
                    ),
                ));
            }
        }
    }
    let metadata = std::fs::metadata(path).map_err(|e| io_error("stat", &subject, &e))?;
    if metadata.dev() != dev || metadata.ino() != ino {
        return Err(HostError::invalid(
            "StrictModes",
            &subject,
            "configuration changed between opening and validating its path",
        ));
    }
    Ok(())
}

/// The uid of the account `name` in the system name service
/// ([`Host::account_uid`]); `None` when there is none. Static musl builds
/// inherit musl's local-account resolver limits (TASK91). The policy (no
/// system uids, no duplicates) is `diskgc_core::invocation`'s.
fn lookup_account(name: &str) -> Result<Option<u32>, HostError> {
    let cname = std::ffi::CString::new(name).map_err(|_| {
        HostError::invalid("getpwnam_r", format!("{name:?}"), "contains a NUL byte")
    })?;
    let mut storage = vec![0u8; 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: cname and storage remain live across the call; libc writes
        // the passwd entry and result pointer only within their allocations.
        let code = unsafe {
            libc::getpwnam_r(
                cname.as_ptr(),
                entry.as_mut_ptr(),
                storage.as_mut_ptr().cast(),
                storage.len(),
                &mut result,
            )
        };
        // The buffer doubles up to 1 MiB: a passwd entry larger than that
        // is an error, not an unbounded allocation.
        if code == libc::ERANGE && storage.len() < (1 << 20) {
            storage.resize(storage.len() * 2, 0);
            continue;
        }
        if code != 0 {
            return Err(HostError::errno(
                "getpwnam_r",
                format!("{name:?}"),
                Errno(code),
            ));
        }
        if result.is_null() {
            return Ok(None);
        }
        // SAFETY: getpwnam_r returned success and a non-null pointer to entry.
        return Ok(Some(unsafe { entry.assume_init().pw_uid }));
    }
}

/// Whether `/run/user/<uid>` is the user's runtime directory as logind
/// makes it: a directory (not a symlink) of `uid` with mode 0700. Only then
/// is the per-user lock there (docs/safety.md, Host lock); a session's
/// `$XDG_RUNTIME_DIR` is never read.
fn runtime_dir_is_users(uid: u32) -> bool {
    is_users_runtime_dir(Path::new(&format!("/run/user/{uid}")), uid)
}

/// Whether `dir` is a directory (not a symlink) of `uid` with mode 0700.
fn is_users_runtime_dir(dir: &Path, uid: u32) -> bool {
    rustix::fs::statat(CWD, dir, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|st| {
        FileType::from_mode(st.st_mode) == FileType::Directory
            && st.st_uid == uid
            && st.st_mode & 0o7777 == 0o700
    })
}

/// The home directory of `uid` in the account database (`getpwuid_r`):
/// `None` when it has no entry, or a home that is not absolute.
fn account_home(uid: u32) -> Result<Option<PathBuf>, HostError> {
    let mut storage = vec![0u8; 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: storage remains live across the call; libc writes the
        // passwd entry and result pointer only within their allocations.
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                storage.as_mut_ptr().cast(),
                storage.len(),
                &mut result,
            )
        };
        // As lookup_account: the buffer doubles up to 1 MiB.
        if code == libc::ERANGE && storage.len() < (1 << 20) {
            storage.resize(storage.len() * 2, 0);
            continue;
        }
        if code != 0 {
            return Err(HostError::errno("getpwuid_r", uid, Errno(code)));
        }
        if result.is_null() {
            return Ok(None);
        }
        // SAFETY: getpwuid_r returned success and a non-null pointer to
        // entry, whose pw_dir points into storage (NUL-terminated) or is null.
        let dir = unsafe { entry.assume_init().pw_dir };
        if dir.is_null() {
            return Ok(None);
        }
        // SAFETY: as above.
        let bytes = unsafe { std::ffi::CStr::from_ptr(dir) }.to_bytes();
        let home = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        return Ok(home.is_absolute().then_some(home));
    }
}

impl LinuxHost {
    /// The procfs mount every reader goes through.
    fn root(&self) -> ProcRoot<'_> {
        ProcRoot(self.proc.as_fd())
    }

    /// The regular file `file` (at `subject`) refers to, opened again
    /// read-only and non-blocking ([`Host::read_head`],
    /// [`Host::reopen_regular`]). The type first, on the fd itself (an
    /// `O_PATH` fd is fstat-able), so a FIFO or a device is refused (`why`
    /// says what may be opened) without ever being opened: opening one has
    /// effects (a FIFO's writer goes on, a device driver runs). Then the
    /// object the fd refers to, through its magic link: never a name looked
    /// up again; non-blocking all the same.
    fn open_regular(&self, file: &LinuxFd, subject: &str, why: &str) -> Result<OwnedFd, HostError> {
        let kind =
            rustix::fs::fstat(file).map_err(|e| HostError::errno("fstat", subject, errno_of(e)))?;
        if FileType::from_mode(kind.st_mode) != FileType::Regular {
            return Err(HostError::invalid(
                "reopen",
                subject,
                format!("not a regular file ({why})"),
            ));
        }
        let rel = format!("self/fd/{}", file.as_fd().as_raw_fd());
        self.root()
            .open(&rel, OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY)
    }
}

/// This process's `status`, relative to the procfs root.
const SELF_STATUS: &str = "self/status";

/// The procfs mount checked by [`LinuxHost::new`], as a directory fd; every
/// path given to it is relative to that mount (`openat`, `statx`,
/// `readlinkat` on the fd). Plain `*at` calls, without `RESOLVE_*`
/// restrictions: the magic links under `/proc` (`fd/N`, `cwd`, `root`,
/// `exe`) must be followed by `statx` and read by `readlinkat`, which
/// `RESOLVE_NO_MAGICLINKS` or `RESOLVE_BENEATH` would refuse.
#[derive(Debug, Clone, Copy)]
struct ProcRoot<'a>(BorrowedFd<'a>);

impl ProcRoot<'_> {
    /// `rel` as shown in messages.
    fn shown(rel: &str) -> String {
        format!("{PROC}/{rel}")
    }

    /// Opens `rel` with `flags`, which are only ever read-only flags:
    /// production code names no write, create or truncate flag outside the
    /// allowlisted wrappers (`source_scan::GATED`, `GATED_ALLOWED`).
    fn open(self, rel: &str, flags: OFlags) -> Result<OwnedFd, HostError> {
        rustix::fs::openat(self.0, rel, flags | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| HostError::errno("open", Self::shown(rel), errno_of(e)))
    }

    /// The whole file, read to its end.
    fn read(self, rel: &str) -> Result<Vec<u8>, HostError> {
        let mut file = std::fs::File::from(self.open(rel, OFlags::RDONLY)?);
        let mut text = Vec::new();
        file.read_to_end(&mut text)
            .map_err(|e| io_error("read", Self::shown(rel), &e))?;
        Ok(text)
    }

    /// The names in directory `rel`, without `.` and `..`. A listing that
    /// fails part-way (`ENOENT` for a directory removed meanwhile, such as a
    /// task that exited) is that error, never a shorter listing.
    fn names(self, rel: &str) -> Result<Vec<Vec<u8>>, HostError> {
        let fd = self.open(rel, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let mut names = Vec::new();
        for_each_name(&fd, |name| {
            names.push(name.to_vec());
            true
        })
        .map_err(|e| HostError::errno("getdents", Self::shown(rel), errno_of(e)))?;
        Ok(names)
    }

    /// The owner of `rel` itself (`statx`, not followed); `None` if the
    /// kernel did not report it.
    fn owner(self, rel: &str) -> Result<Option<u32>, HostError> {
        rustix::fs::statx(
            self.0,
            rel,
            AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT,
            STATX_MASK,
        )
        .map(|x| stat_from_statx(&x).uid)
        .map_err(|e| HostError::errno("statx", Self::shown(rel), errno_of(e)))
    }

    /// A magic link: its text and the object it refers to (`statx`
    /// following it).
    fn link_target(self, rel: &str) -> Result<LinkTarget, HostError> {
        // A FUSE server may have stopped answering after a task entered its
        // mount. Use cached attributes for the magic-link target rather than
        // asking that server to refresh them during the in-use scan.
        let stat = rustix::fs::statx(
            self.0,
            rel,
            AtFlags::STATX_DONT_SYNC | AtFlags::NO_AUTOMOUNT,
            STATX_MASK,
        )
        .map(|x| stat_from_statx(&x))
        .map_err(|e| HostError::errno("statx", Self::shown(rel), errno_of(e)))?;
        // Into a stack buffer, then one exact allocation (TASK-115); a
        // text that fills the buffer may be cut, so it is read again
        // through the growing read.
        let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); LINK_TEXT_MAX];
        let (text, _) = rustix::fs::readlinkat_raw(self.0, rel, &mut buf)
            .map_err(|e| HostError::errno("readlink", Self::shown(rel), errno_of(e)))?;
        let text = if text.len() < LINK_TEXT_MAX {
            text.to_vec()
        } else {
            rustix::fs::readlinkat(self.0, rel, Vec::new())
                .map_err(|e| HostError::errno("readlink", Self::shown(rel), errno_of(e)))?
                .into_bytes()
        };
        Ok(LinkTarget {
            text: OsString::from_vec(text),
            stat,
        })
    }
}

/// The `getdents64` buffer of [`for_each_name`]: far larger than one entry
/// (a name is at most 255 bytes), so it never fails with `EINVAL`.
const DIRENT_BUFFER: usize = 32 * 1024;

/// Calls `each` with every name in the directory open (readable) as `fd`,
/// without `.` and `..`, from the fd's current position (a fresh fd: the
/// start) to the end, until `each` returns `false`. Every `getdents64`
/// error is returned, `ENOENT` (a directory removed after it was opened)
/// included: `rustix::fs::Dir` turns that one into the end of the listing,
/// which would read an unlistable directory as complete, so its raw form
/// is used. The caller opens the fd for this listing alone, so no other
/// listing shares its position.
fn for_each_name(
    fd: &OwnedFd,
    mut each: impl FnMut(&[u8]) -> bool,
) -> Result<(), rustix::io::Errno> {
    // The thread's scratch buffer (crate::scratch: why).
    crate::scratch::with_scratch(|buf| {
        buf.reserve(DIRENT_BUFFER);
        let mut entries =
            rustix::fs::RawDir::new(fd, &mut buf.spare_capacity_mut()[..DIRENT_BUFFER]);
        loop {
            match entries.next() {
                None => return Ok(()),
                Some(Err(rustix::io::Errno::INTR)) => continue,
                Some(Err(e)) => return Err(e),
                Some(Ok(entry)) => {
                    let name = entry.file_name().to_bytes();
                    if name != b"." && name != b".." && !each(name) {
                        return Ok(());
                    }
                }
            }
        }
    })
}

/// The pid namespace check of [`LinuxHost::new`], on what reading
/// [`SELF_STATUS`] gave.
///
/// `NSpid:` lists the task's pid in every pid namespace from the one of the
/// `/proc` mount down to the task's own (fs/proc/array.c `task_state`: `for
/// (g = ns->level; g <= pid->level; g++)`, with `ns` the mount's namespace
/// and `pid` the task's). One field means the mount's namespace is at the
/// task's own level, and since the task is visible in it, it is the task's
/// own namespace. More fields mean the `/proc` of an ancestor namespace.
/// The line exists on every kernel with `CONFIG_PID_NS` (Linux 4.1 and
/// later); without it nothing can be confirmed, so that is refused too.
fn check_proc_namespace(own: u32, status: Result<Vec<u8>, HostError>) -> Result<(), HostError> {
    let why = match status {
        Ok(text) => match parse_nspid_levels(&text) {
            Ok(levels) if levels == [own] => return Ok(()),
            Ok(levels) => format!(
                "its NSpid line is {:?} (pids from the namespace of the mounted /proc down \
                 to this process's own), but this process is pid {own} in its own \
                 namespace: /proc belongs to another pid namespace",
                levels
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Err(e) => format!("{e}, so its pid namespace cannot be confirmed"),
        },
        Err(e) if e.is(Errno::ENOENT) => {
            "does not exist: this process is not visible in the mounted /proc".to_owned()
        }
        Err(e) => return Err(e),
    };
    Err(HostError::invalid(
        "read",
        ProcRoot::shown(SELF_STATUS),
        format!(
            "{why}; /proc must be mounted for this process's own pid namespace, or no process \
             can be inspected (e.g. unshare --pid --fork --mount-proc)"
        ),
    ))
}

/// The pidfd check of [`LinuxHost::new`]: what `pidfd_open` on this process
/// gave.
fn check_pidfd_open(opened: Result<(), Errno>) -> Result<(), HostError> {
    opened.map_err(|e| {
        HostError::invalid(
            "pidfd_open",
            "this process",
            format!(
                "{e}: pidfd_open is unavailable (blocked by a seccomp filter or a sandbox); \
                 diskgc cannot tell whether a process has exited without it"
            ),
        )
    })
}

pub(crate) fn errno_of(e: rustix::io::Errno) -> Errno {
    Errno(e.raw_os_error())
}

pub(crate) fn io_error(
    op: &'static str,
    subject: impl std::fmt::Display,
    e: &std::io::Error,
) -> HostError {
    match e.raw_os_error() {
        Some(raw) => HostError::errno(op, subject, Errno(raw)),
        None => HostError::invalid(op, subject, e.to_string()),
    }
}

/// The path an open of `path` from `base` is shown as (see [`LinuxFd`]).
fn shown_path(base: &Base<'_, LinuxFd>, path: &Path) -> PathBuf {
    match base {
        Base::Root => path.to_owned(),
        Base::Dir(fd) if path.as_os_str().is_empty() || path == Path::new(".") => fd.shown.clone(),
        Base::Dir(fd) => fd.shown.join(path),
    }
}

fn shown(base: &Base<'_, LinuxFd>, path: &Path) -> String {
    shown_path(base, path).display().to_string()
}

fn oflags(how: OpenHow) -> OFlags {
    let mut flags = OFlags::CLOEXEC;
    flags |= match how.access {
        Access::Path => OFlags::PATH,
        // A readable open can reach a FIFO or a device before the caller
        // checks its type. Never wait for a FIFO writer or acquire a
        // controlling terminal while opening a candidate entry.
        Access::Read => OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY,
    };
    if how.directory {
        flags |= OFlags::DIRECTORY;
    }
    if how.nofollow {
        flags |= OFlags::NOFOLLOW;
    }
    flags
}

fn resolve_flags(how: OpenHow) -> ResolveFlags {
    let mut flags = ResolveFlags::empty();
    if how.resolve.beneath {
        flags |= ResolveFlags::BENEATH;
    }
    if how.resolve.no_symlinks {
        flags |= ResolveFlags::NO_SYMLINKS;
    }
    if how.resolve.no_xdev {
        flags |= ResolveFlags::NO_XDEV;
    }
    flags
}

fn flock_operation(mode: LockMode) -> FlockOperation {
    match mode {
        LockMode::Shared => FlockOperation::NonBlockingLockShared,
        LockMode::Exclusive => FlockOperation::NonBlockingLockExclusive,
    }
}

/// A path relative to the procfs root, formatted without allocating: the
/// in-use scan names some 10^5 of them per scan, and on the static musl
/// build every allocation takes the allocator's one global lock (TASK-115).
/// The longest, `<pid>/task/<tid>/fd/<fd>` with 10-digit numbers, is 41
/// bytes.
#[derive(Clone, Copy)]
struct ProcPath {
    len: usize,
    buf: [u8; PROC_PATH_MAX],
}

/// The stack buffer of one magic link's text ([`ProcRoot::link_target`]):
/// `PATH_MAX`, which holds nearly every text in one call.
const LINK_TEXT_MAX: usize = 4096;

/// The capacity of a [`ProcPath`].
const PROC_PATH_MAX: usize = 64;

impl ProcPath {
    /// `args` formatted; a path that does not fit is a bug (every caller
    /// formats numbers and fixed names) and panics.
    fn of(args: std::fmt::Arguments<'_>) -> Self {
        let mut path = Self {
            len: 0,
            buf: [0; PROC_PATH_MAX],
        };
        std::fmt::Write::write_fmt(&mut path, args)
            .unwrap_or_else(|_| panic!("a /proc path longer than {PROC_PATH_MAX} bytes: {args}"));
        path
    }
}

impl std::fmt::Write for ProcPath {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        let room = self.buf.get_mut(self.len..end).ok_or(std::fmt::Error)?;
        room.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

impl std::ops::Deref for ProcPath {
    type Target = str;
    fn deref(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len]).expect("formatted from str pieces")
    }
}

impl std::fmt::Display for ProcPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

impl std::fmt::Debug for ProcPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

/// `<pid>/task/<tid>/<rest>`, relative to the procfs root.
fn task_path(pid: Pid, tid: u32, rest: &str) -> ProcPath {
    ProcPath::of(format_args!("{pid}/task/{tid}/{rest}"))
}

/// `None` for an entry that vanished (`ENOENT`, `ESRCH`), which alone
/// proves nothing; any other error is classified by
/// [`ProcError::from_host`].
fn vanished_as_none<T>(result: Result<T, HostError>) -> Result<Option<T>, ProcError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if is_vanished(&e) => Ok(None),
        Err(e) => Err(ProcError::from_host(e)),
    }
}

/// The task ids of thread group `pid`; `None` when a lookup said `ENOENT`
/// ([`crate::tasks::collect`] decides whether the process is gone).
fn task_ids(proc: ProcRoot<'_>, pid: Pid) -> Result<Option<Vec<u32>>, ProcError> {
    let dir = ProcPath::of(format_args!("{pid}/task"));
    let listed = (|| {
        let fd = proc.open(&dir, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let mut tids = Vec::new();
        let mut unexpected = None;
        for_each_name(&fd, |name| {
            match std::str::from_utf8(name).ok().and_then(|s| s.parse().ok()) {
                Some(tid) => tids.push(tid),
                None => unexpected = Some(String::from_utf8_lossy(name).into_owned()),
            }
            unexpected.is_none()
        })
        .map_err(|e| HostError::errno("getdents", ProcRoot::shown(&dir), errno_of(e)))?;
        match unexpected {
            None => Ok(tids),
            Some(name) => Err(HostError::invalid(
                "getdents",
                ProcRoot::shown(&dir),
                format!("unexpected entry {name:?}"),
            )),
        }
    })();
    vanished_as_none(listed)
}

/// One read of `/proc/<pid>/status` (see [`StatusFields`]); `None` when it
/// said `ENOENT`.
fn group_status(proc: ProcRoot<'_>, pid: Pid) -> Result<Option<StatusFields>, ProcError> {
    let path = ProcPath::of(format_args!("{pid}/status"));
    let Some(text) = vanished_as_none(proc.read(&path))? else {
        return Ok(None);
    };
    parse_status(&text)
        .map(Some)
        .map_err(|e| ProcError::Failed(HostError::invalid("parse", ProcRoot::shown(&path), e)))
}

/// One [`Listing`]: list numeric tids, pin their task directories, read the
/// count, then list again and confirm that each pinned task still owns its
/// numeric tid. A vanished pin makes the round incomplete. `None` means a
/// lookup vanished before the count could be established.
fn task_listing<P>(
    mut list: impl FnMut() -> Result<Option<Vec<u32>>, ProcError>,
    count: impl FnOnce() -> Result<Option<usize>, ProcError>,
    mut pin: impl FnMut(u32) -> Result<Option<P>, ProcError>,
    mut same: impl FnMut(u32, &P) -> Result<bool, ProcError>,
) -> Result<Option<Listing>, ProcError> {
    let Some(tids) = list()? else {
        return Ok(None);
    };
    let mut pinned = Vec::with_capacity(tids.len());
    for &tid in &tids {
        let Some(identity) = pin(tid)? else {
            return Ok(None);
        };
        pinned.push((tid, identity));
    }
    let Some(threads) = count()? else {
        return Ok(None);
    };
    let Some(again) = list()? else {
        return Ok(None);
    };
    let mut listing = Listing::new(tids, threads, &again);
    for (tid, identity) in pinned {
        if !same(tid, &identity)? {
            listing.invalidate_identity();
        }
    }
    Ok(Some(listing))
}

/// The numeric listing can look unchanged when a tid exits and its number
/// is recycled between the two `getdents` calls. Pin every first-list task
/// before reading `Threads:` and compare its procfs inode with a fresh open
/// after the second list. A mismatch invalidates the round. Procfs assigns
/// each instantiated task directory a fresh inode (`get_next_ino` in
/// fs/proc/base.c) and keeps its `struct pid` in that inode.
fn anchored_task_listing(proc: ProcRoot<'_>, pid: Pid) -> Result<Option<Listing>, ProcError> {
    anchored_task_listing_with_count(proc, pid, || {
        Ok(group_status(proc, pid)?.map(|status| status.threads))
    })
}

/// `count` sits between pinning the first list and reading the second.
/// Supplying it separately lets the fake-procfs test replace a task at that
/// precise point while production reads the kernel's `Threads:` field.
fn anchored_task_listing_with_count(
    proc: ProcRoot<'_>,
    pid: Pid,
    count: impl FnOnce() -> Result<Option<usize>, ProcError>,
) -> Result<Option<Listing>, ProcError> {
    task_listing(
        || task_ids(proc, pid),
        count,
        |tid| {
            let path = ProcPath::of(format_args!("{pid}/task/{tid}"));
            match proc.open(&path, OFlags::PATH | OFlags::DIRECTORY) {
                Ok(fd) => Ok(Some(fd)),
                Err(e) if is_vanished(&e) => Ok(None),
                Err(e) => Err(ProcError::from_host(e)),
            }
        },
        |tid, old| {
            let path = ProcPath::of(format_args!("{pid}/task/{tid}"));
            let fresh = match proc.open(&path, OFlags::PATH | OFlags::DIRECTORY) {
                Ok(fd) => fd,
                Err(e) if is_vanished(&e) => return Ok(false),
                Err(e) => return Err(ProcError::from_host(e)),
            };
            // `fstat` on an old directory inode can still succeed after its
            // task was released. Require a *current task* through the pin
            // as well, so even an inode-number wrap cannot validate reuse.
            match rustix::fs::openat(old, "stat", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()) {
                Ok(_) => {}
                Err(rustix::io::Errno::NOENT | rustix::io::Errno::SRCH) => {
                    return Ok(false);
                }
                Err(e) => {
                    return Err(ProcError::from_host(HostError::errno(
                        "open",
                        task_path(pid, tid, "stat"),
                        errno_of(e),
                    )));
                }
            }
            let old_stat = rustix::fs::fstat(old).map_err(|e| {
                ProcError::from_host(HostError::errno(
                    "fstat",
                    ProcRoot::shown(&path),
                    errno_of(e),
                ))
            })?;
            let fresh_stat = rustix::fs::fstat(&fresh).map_err(|e| {
                ProcError::from_host(HostError::errno(
                    "fstat",
                    ProcRoot::shown(&path),
                    errno_of(e),
                ))
            })?;
            Ok((old_stat.st_dev, old_stat.st_ino) == (fresh_stat.st_dev, fresh_stat.st_ino))
        },
    )
}

/// The process ids in a fresh listing of `/proc` (thread-group ids), sorted.
fn proc_pids(proc: ProcRoot<'_>) -> Result<Vec<Pid>, HostError> {
    let mut pids = Vec::new();
    for name in proc.names(".")? {
        if !name.is_empty() && name.iter().all(u8::is_ascii_digit) {
            let pid = std::str::from_utf8(&name)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| {
                    HostError::invalid("getdents", ProcRoot::shown("."), "pid out of range")
                })?;
            pids.push(Pid(pid));
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

/// A pidfd for process `pid`; `pidfd_open` saying `ESRCH` is `Gone`: the
/// number names no pid in this namespace (`find_get_pid`, kernel/pid.c), or
/// its pid has no task left (`pidfd_prepare`, kernel/fork.c:
/// `!pid_has_task(pid, PIDTYPE_PID)`), i.e. the process was released. The
/// pidfd pins the process's `struct pid`, so a reused pid cannot answer
/// for it, and no `/proc` visibility rule (`hidepid`) touches it.
fn open_pidfd(pid: Pid) -> Result<OwnedFd, ProcError> {
    let raw = rustix::process::Pid::from_raw(pid.0.cast_signed()).ok_or_else(|| {
        ProcError::Failed(HostError::invalid("pidfd_open", pid, "not a valid pid"))
    })?;
    match rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::empty()) {
        Ok(fd) => Ok(fd),
        Err(rustix::io::Errno::SRCH) => Err(ProcError::Gone),
        // `EPERM` here is a seccomp filter, not a refusal to show the
        // process: `failed`, which keeps only `EMFILE`/`ENFILE` apart.
        Err(e) => Err(ProcError::failed(HostError::errno(
            "pidfd_open",
            pid,
            errno_of(e),
        ))),
    }
}

/// What `poll` on a pidfd reported (`revents`), as [`PidfdPoll`]:
/// `POLLHUP` (with `POLLIN`) when the pid has no task left, `POLLIN` alone
/// when the task found had exited (fs/pidfs.c `pidfd_poll`). Anything else
/// (`POLLNVAL`, `POLLERR`) is a bug here or an unknown kernel: `Failed`.
fn pidfd_poll_answer(pid: Pid, revents: PollFlags) -> Result<PidfdPoll, ProcError> {
    if revents.is_empty() {
        Ok(PidfdPoll::Running)
    } else if revents.contains(PollFlags::HUP) {
        Ok(PidfdPoll::Hangup)
    } else if revents == PollFlags::IN | PollFlags::RDNORM || revents == PollFlags::IN {
        Ok(PidfdPoll::Readable)
    } else {
        Err(ProcError::Failed(HostError::invalid(
            "poll",
            pid,
            format!("unexpected pidfd poll events {revents:?}"),
        )))
    }
}

/// A zero signal only checks the process pinned by the pidfd. On kernels
/// before 6.9, poll cannot distinguish a released process from a zombie.
fn pidfd_released(pid: Pid, pidfd: &OwnedFd) -> Result<bool, ProcError> {
    // SAFETY: signal 0 sends nothing; the fd is held open and the remaining
    // syscall arguments are the documented null siginfo and zero flags.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            0,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == 0 {
        return Ok(false);
    }
    let error = rustix::io::Errno::from_io_error(&std::io::Error::last_os_error())
        .expect("a failed syscall sets errno");
    match error {
        rustix::io::Errno::SRCH => Ok(true),
        // A live process belonging to another user may deny the check.
        rustix::io::Errno::PERM => Ok(false),
        _ => Err(ProcError::failed(HostError::errno(
            "pidfd_send_signal",
            pid,
            errno_of(error),
        ))),
    }
}

/// Polls `pidfd`, waiting at most `timeout` for an exiting process.
fn poll_pidfd(pid: Pid, pidfd: &OwnedFd, timeout: Duration) -> Result<PidfdPoll, ProcError> {
    let mut fds = [rustix::event::PollFd::new(pidfd, PollFlags::IN)];
    let wait = rustix::event::Timespec {
        tv_sec: timeout.as_secs().cast_signed(),
        tv_nsec: timeout.subsec_nanos().into(),
    };
    rustix::event::poll(&mut fds, Some(&wait))
        .map_err(|e| ProcError::failed(HostError::errno("poll", pid, errno_of(e))))?;
    let answer = pidfd_poll_answer(pid, fds[0].revents())?;
    if answer == PidfdPoll::Readable && pidfd_released(pid, pidfd)? {
        Ok(PidfdPoll::Hangup)
    } else {
        Ok(answer)
    }
}

/// How the readers reach process `pid`: its task listings, its pidfd and
/// its `status` (for the rule, see [`crate::tasks::has_exited`]). The
/// listing is anchored ([`anchored_task_listing`]) for [`Want::EveryTask`];
/// [`Want::FirstLiveTask`] needs only the tids to try
/// ([`Listing::candidates`]), which saves pinning and confirming every task
/// of the process for each of the memory map and the environment.
fn process<'a>(
    proc: ProcRoot<'a>,
    pid: Pid,
    pidfd: &'a OwnedFd,
    want: Want,
) -> Process<
    impl FnMut() -> Listed + 'a,
    impl FnMut() -> Polled + 'a,
    impl FnMut() -> StatusRead + 'a,
> {
    let mut polls = 0;
    Process {
        pid: pid.0,
        list: move || match want {
            Want::EveryTask => anchored_task_listing(proc, pid),
            Want::FirstLiveTask => Ok(task_ids(proc, pid)?.map(Listing::candidates)),
        },
        poll: move || {
            let timeout = if polls == 0 {
                Duration::ZERO
            } else {
                Duration::from_millis(2)
            };
            polls += 1;
            let answer = poll_pidfd(pid, pidfd, timeout)?;
            if timeout > Duration::ZERO && answer == PidfdPoll::Readable {
                // A zombie makes poll return immediately. Give a task in
                // the middle of release the same bounded pause before
                // checking the held pidfd once more.
                thread::sleep(timeout);
                poll_pidfd(pid, pidfd, Duration::ZERO)
            } else {
                Ok(answer)
            }
        },
        status: move || group_status(proc, pid),
    }
}

/// Holds the `stat` file of each task whose `stat` is read, open, and reads
/// it again with one `pread` at offset 0 (which makes the kernel generate
/// it afresh). A procfs inode holds its task's `struct pid`, so a later
/// read of the held file cannot silently follow a reused numeric tid to
/// another `struct pid`: once that task is released, the read says
/// `ESRCH` (`proc_single_show`). Keep the handle through all rounds: a
/// vanished incarnation must make the read untrustworthy, even when the
/// replacement has the same start tick.
struct TaskStats<'a> {
    proc: ProcRoot<'a>,
    pid: Pid,
    pins: BTreeMap<u32, OwnedFd>,
}

/// The buffer of one `stat` read. The line is one `seq_file` record: 52
/// fields of at most 20 digits and a command name of at most 64 bytes, so
/// well under this; a read that fills it is refused, never parsed.
const TASK_STAT_MAX: usize = 4096;

impl<'a> TaskStats<'a> {
    fn new(proc: ProcRoot<'a>, pid: Pid) -> Self {
        Self {
            proc,
            pid,
            pins: BTreeMap::new(),
        }
    }

    fn read(&mut self, tid: u32) -> Result<TaskStat, HostError> {
        let path = || task_path(self.pid, tid, "stat");
        if !self.pins.contains_key(&tid) {
            let pin = self.proc.open(&path(), OFlags::RDONLY)?;
            self.pins.insert(tid, pin);
        }
        let pin = self.pins.get(&tid).expect("task stat was just pinned");
        let mut text = [0; TASK_STAT_MAX];
        let n = rustix::io::pread(pin, &mut text, 0)
            .map_err(|e| HostError::errno("read", ProcRoot::shown(&path()), errno_of(e)))?;
        if n == text.len() {
            return Err(HostError::invalid(
                "read",
                ProcRoot::shown(&path()),
                format!("longer than {TASK_STAT_MAX} bytes"),
            ));
        }
        parse_task_stat(&text[..n])
            .map_err(|e| HostError::invalid("parse", ProcRoot::shown(&path()), e))
    }
}

/// Bounds of the one-call reads: the first buffer, and the largest one
/// before giving up (`Failed`). 16 MiB of directory entries hold several
/// hundred thousand fds; 64 MiB exceeds any environment `execve` accepts.
const FIRST_BUFFER: usize = 64 * 1024;
const MAX_FD_LISTING: usize = 16 * 1024 * 1024;
const MAX_ENVIRON: usize = 64 * 1024 * 1024;
/// The bounds of the buffer of one `read` call of `maps`.
const MAPS_READ_MIN: usize = 16 * 1024;
const MAPS_READ_MAX: usize = 1024 * 1024;

/// The file operations of the `/proc` readers, so tests can script what the
/// kernel returns and pin the reading rules at their call sites.
trait ProcFiles {
    type File;
    fn open(&self, path: &str) -> Result<Self::File, HostError>;
    /// One `read` call.
    fn read(&self, file: &mut Self::File, buf: &mut [u8]) -> Result<usize, HostError>;
    /// Back to the start of the file (`lseek` to 0).
    fn rewind(&self, file: &mut Self::File) -> Result<(), HostError>;
    /// One `getdents` call on a fresh open of `dir` with a buffer of `size`
    /// bytes: the fd numbers it gave, and whether the next call gave nothing.
    fn list_once(&self, dir: &str, size: usize) -> Result<(Vec<u32>, bool), HostError>;
}

/// The real `/proc`, through the procfs mount of the host.
struct Kernel<'a>(ProcRoot<'a>);

impl ProcFiles for Kernel<'_> {
    /// The file and the `/proc` path it was opened by, for messages.
    type File = (std::fs::File, String);

    fn open(&self, path: &str) -> Result<Self::File, HostError> {
        let file = self.0.open(path, OFlags::RDONLY).map(std::fs::File::from)?;
        Ok((file, ProcRoot::shown(path)))
    }

    fn read(&self, (file, shown): &mut Self::File, buf: &mut [u8]) -> Result<usize, HostError> {
        file.read(buf).map_err(|e| io_error("read", &*shown, &e))
    }

    fn rewind(&self, (file, shown): &mut Self::File) -> Result<(), HostError> {
        file.seek(SeekFrom::Start(0))
            .map(|_| ())
            .map_err(|e| io_error("seek", &*shown, &e))
    }

    fn list_once(&self, dir: &str, size: usize) -> Result<(Vec<u32>, bool), HostError> {
        let fd = self.0.open(dir, OFlags::RDONLY | OFlags::DIRECTORY)?;
        // The thread's scratch buffer (crate::scratch: why).
        crate::scratch::with_scratch(|buf| {
            buf.reserve(size);
            list_fd_numbers(&fd, dir, &mut buf.spare_capacity_mut()[..size])
        })
    }
}

/// The fd numbers of the fd directory open as `fd` (at `dir`) from one
/// `getdents` call into `buf`, and whether the next call gave nothing
/// ([`ProcFiles::list_once`]).
fn list_fd_numbers(
    fd: &OwnedFd,
    dir: &str,
    buf: &mut [std::mem::MaybeUninit<u8>],
) -> Result<(Vec<u32>, bool), HostError> {
    let mut entries = rustix::fs::RawDir::new(fd, buf);
    let mut fds = Vec::new();
    let mut filled = false;
    loop {
        if filled && entries.is_buffer_empty() {
            // The next `next` makes a second call: it must find nothing.
            return match entries.next() {
                None => Ok((fds, true)),
                Some(Ok(_)) => Ok((fds, false)),
                Some(Err(e)) => Err(HostError::errno("getdents", dir, errno_of(e))),
            };
        }
        match entries.next() {
            None => return Ok((fds, true)),
            Some(Err(e)) => return Err(HostError::errno("getdents", dir, errno_of(e))),
            Some(Ok(entry)) => {
                filled = true;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                let fd = std::str::from_utf8(name)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| {
                        HostError::invalid("getdents", dir, format!("unexpected entry {name:?}"))
                    })?;
                fds.push(fd);
            }
        }
    }
}

/// The fd numbers of one task from a single `getdents` call (see
/// [`list_in_one_call`]): one call finds the task once, so the fds all come
/// from one task (each fd is looked up in that task's current table,
/// `fget_task_next`). A task closing its table at exit (`exit_files`) can
/// give a prefix and then nothing; `PF_EXITING` is set before that, so the
/// `stat` read after this listing classifies the task as exiting, and an
/// exiting task's read is never the whole answer.
fn fd_numbers(files: &impl ProcFiles, dir: &str) -> Result<Vec<u32>, HostError> {
    list_in_one_call(FIRST_BUFFER, MAX_FD_LISTING, |size| {
        files.list_once(dir, size)
    })?
    .ok_or_else(|| HostError::invalid("getdents", dir, "too many fds to list in one call"))
}

/// The open fds of one task, from one listing (see [`fd_numbers`]), each
/// statted through its link. An fd closed between listing and stat is left
/// out: it no longer holds anything. (A link bound to the dead old leader of
/// a non-leader exec also says `ENOENT`; the exec'ing thread's old tid then
/// shows that leader as ended, and [`crate::tasks::collect`] reads the
/// leader's tid again.)
fn task_fds(proc: ProcRoot<'_>, pid: Pid, tid: u32) -> Result<Vec<ProcFd>, HostError> {
    let dir = task_path(pid, tid, "fd");
    let numbers = fd_numbers(&Kernel(proc), &dir)?;
    let mut fds = Vec::with_capacity(numbers.len());
    for fd in numbers {
        match proc.link_target(&ProcPath::of(format_args!("{pid}/task/{tid}/fd/{fd}"))) {
            Ok(target) => fds.push(ProcFd { fd, target }),
            Err(e) if e.is(Errno::ENOENT) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(fds)
}

/// `kcmp(reference, tid, KCMP_FILES, 0, 0)`: whether the two tasks hold
/// the same fd table now (`Ok(true)`: 0), or not (`Ok(false)`: 1, 2 or 3,
/// the kernel's ordering of two different tables), or the kernel's refusal
/// (`ENOSYS` without `CONFIG_KCMP`; `EPERM` from a seccomp filter, such as
/// a container runtime's default profile without `CAP_SYS_PTRACE`, or from
/// the ptrace access check; `ESRCH`, a task gone, is `Ok(false)`, no
/// refusal). Anything but `Ok(true)` is "not known
/// to share": the caller then reads `tid`'s table in full, so nothing is
/// lost but time ([`crate::tasks::collect`]). Note that the kernel reads
/// the two tasks' table pointers without a lock that orders them (a
/// documented limit, docs/safety.md, In use: Threads).
fn same_fd_table(reference: u32, tid: u32) -> Result<bool, Errno> {
    /// `KCMP_FILES` of include/uapi/linux/kcmp.h.
    const KCMP_FILES: i32 = 2;
    let (Ok(reference), Ok(tid)) = (i32::try_from(reference), i32::try_from(tid)) else {
        return Ok(false);
    };
    // SAFETY: kcmp takes two pids, a comparison type and two unsigned
    // longs that KCMP_FILES does not use; it dereferences no pointer.
    let answer = unsafe { libc::syscall(libc::SYS_kcmp, reference, tid, KCMP_FILES, 0_u64, 0_u64) };
    if answer < 0 {
        let error = std::io::Error::last_os_error();
        let errno = Errno(error.raw_os_error().expect("a failed syscall sets errno"));
        // A task gone between its `stat` and the comparison is churn, not
        // a refusal: the full read then decides about it.
        return if errno == Errno::ESRCH {
            Ok(false)
        } else {
            Err(errno)
        };
    }
    Ok(answer == 0)
}

/// `ioprio_set(IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE)` for the calling
/// thread (`who` 0 is the caller; Linux keeps the I/O priority per task).
/// Only the idle class's own scheduler, BFQ, honours it; it changes no file
/// (docs/architecture.md, The tally).
fn lower_io_priority_ungated() -> Result<(), Errno> {
    /// include/uapi/linux/ioprio.h.
    const IOPRIO_WHO_PROCESS: i32 = 1;
    const IOPRIO_CLASS_IDLE: i32 = 3;
    const IOPRIO_CLASS_SHIFT: i32 = 13;
    // SAFETY: ioprio_set takes three integers and dereferences no pointer.
    let answer = unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0,
            IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT,
        )
    };
    if answer < 0 {
        let error = std::io::Error::last_os_error();
        return Err(Errno(
            error.raw_os_error().expect("a failed syscall sets errno"),
        ));
    }
    Ok(())
}

/// Whether a task's read of its three links lacks one ([`Host::proc_links_all`]).
/// A link always names something, so one missing from a live task makes
/// the read untrusted, as `ENOENT` from one [`Host::proc_links`] does; the
/// links an exiting task still shows are kept all the same.
fn links_incomplete(links: &[Option<LinkTarget>; 3]) -> bool {
    links.iter().any(Option::is_none)
}

/// Reads `environ` of one task in one call (see [`read_in_one_call`]).
fn read_environ(files: &impl ProcFiles, path: &str) -> Result<Vec<u8>, HostError> {
    read_in_one_call(
        FIRST_BUFFER,
        MAX_ENVIRON,
        || files.open(path),
        |file, buf| files.read(file, buf),
    )?
    .ok_or_else(|| HostError::invalid("read", path, "environment too large"))
}

/// Reads `cmdline` in one call, bounded as the environment is (both live
/// in the argument area `execve` limits).
fn read_cmdline(files: &impl ProcFiles, path: &str) -> Result<Vec<u8>, HostError> {
    read_in_one_call(
        FIRST_BUFFER,
        MAX_ENVIRON,
        || files.open(path),
        |file, buf| files.read(file, buf),
    )?
    .ok_or_else(|| HostError::invalid("read", path, "command line too large"))
}

/// Reads `maps` of one task in two passes on separate opens, each to the
/// end and confirmed (see [`read_confirmed`]), and takes their union (see
/// [`read_two_passes`]).
fn read_maps(files: &impl ProcFiles, path: &str) -> Result<Vec<u8>, HostError> {
    read_two_passes(|| {
        read_confirmed(
            || files.open(path),
            |file| {
                // Calls that grow with the content: a desktop process maps
                // up to some 10^3 objects (~10^5 bytes), and every call is
                // a `read` and a resume; the buffer each call zeroes stays
                // within the content read so far (TASK-115). Read into the
                // thread's scratch buffer and copied out at its length
                // (crate::scratch: why).
                crate::scratch::with_scratch(|content| {
                    loop {
                        let start = content.len();
                        content.resize(start + start.clamp(MAPS_READ_MIN, MAPS_READ_MAX), 0);
                        let n = files.read(file, &mut content[start..])?;
                        content.truncate(start + n);
                        if n == 0 {
                            return Ok(content.to_vec());
                        }
                    }
                })
            },
            |file| {
                files.rewind(file)?;
                files.read(file, &mut [0; 256])
            },
        )
    })
}

/// Reads `maps` or `environ` (`name`, which gives the `what` of messages)
/// of a live task of `pid` (all tasks share the memory map and the
/// environment block).
fn read_through_live_task(
    proc: ProcRoot<'_>,
    pid: Pid,
    (name, what): (&str, &'static str),
    read: impl Fn(&str) -> Result<Vec<u8>, HostError>,
) -> Result<(ProcPath, Vec<u8>), ProcError> {
    let pidfd = open_pidfd(pid)?;
    let mut stats = TaskStats::new(proc, pid);
    let mut got = collect(
        process(proc, pid, &pidfd, Want::FirstLiveTask),
        Tasks {
            what,
            read: |tid| {
                let path = task_path(pid, tid, name);
                read(&path).map(|bytes| (path, bytes))
            },
            is_empty: |(_, bytes): &(ProcPath, Vec<u8>)| bytes.is_empty(),
            stat: |tid| stats.read(tid),
            shares: never_shared,
        },
        Want::FirstLiveTask,
    )?;
    // Empty only when every live task is a kernel thread without the file:
    // no memory map, no environment.
    Ok(got
        .pop()
        .unwrap_or_else(|| (task_path(pid, pid.0, name), Vec::new())))
}

impl Host for LinuxHost {
    type Fd = LinuxFd;
    type ProcIdentity = (Pid, OwnedFd);

    fn mountinfo(&self) -> Result<Vec<MountEntry>, HostError> {
        let path = "self/mountinfo";
        let text = self.root().read(path)?;
        parse_mountinfo(&text).map_err(|e| HostError::invalid("parse", ProcRoot::shown(path), e))
    }

    fn openat2(
        &self,
        base: Base<'_, LinuxFd>,
        path: &Path,
        how: OpenHow,
    ) -> Result<LinuxFd, HostError> {
        let dir = match base {
            Base::Root if !path.is_absolute() => {
                return Err(HostError::invalid(
                    "openat2",
                    path.display(),
                    "a path from the root must be absolute",
                ));
            }
            Base::Root => CWD,
            Base::Dir(fd) => fd.as_fd(),
        };
        match rustix::fs::openat2(dir, path, oflags(how), Mode::empty(), resolve_flags(how)) {
            Ok(fd) => Ok(LinuxFd {
                fd,
                shown: shown_path(&base, path),
            }),
            Err(error) => {
                // O_DIRECTORY | O_NOFOLLOW reports a trailing symlink as
                // ENOTDIR. For a mount-point open, resolve maps that to
                // "missing". Inspect it without traversing any symlink so
                // a stale mountinfo entry naming a link fails unsafe_path.
                let errno = if matches!(base, Base::Root)
                    && error == rustix::io::Errno::NOTDIR
                    && how.directory
                    && how.nofollow
                    && how.resolve.no_symlinks
                    && matches!(
                        self.statx(Base::Root, path),
                        Ok(Stat {
                            file_type: Some(FileType::Symlink),
                            ..
                        }) | Err(HostError {
                            cause: diskgc_core::host::Cause::Errno(Errno::ELOOP),
                            ..
                        })
                    ) {
                    Errno::ELOOP
                } else {
                    errno_of(error)
                };
                Err(HostError::errno("openat2", shown(&base, path), errno))
            }
        }
    }

    fn statx(&self, base: Base<'_, LinuxFd>, path: &Path) -> Result<Stat, HostError> {
        let empty = path.as_os_str().is_empty();
        let (dir, flags) = match base {
            Base::Root if !path.is_absolute() => {
                return Err(HostError::invalid(
                    "statx",
                    path.display(),
                    "a path from the root must be absolute",
                ));
            }
            Base::Root => {
                // AT_SYMLINK_NOFOLLOW protects only the final component.
                // Open through the kernel's no-symlink resolver first, then
                // stat the held object; O_NOFOLLOW still lets us report a
                // trailing symlink's type to the caller.
                let fd = rustix::fs::openat2(
                    CWD,
                    path,
                    OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                    ResolveFlags::NO_SYMLINKS,
                )
                .map_err(|e| HostError::errno("statx", shown(&base, path), errno_of(e)))?;
                let stat = rustix::fs::statx(
                    fd.as_fd(),
                    "",
                    AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT,
                    STATX_MASK,
                )
                .map(|x| stat_from_statx(&x))
                .map_err(|e| HostError::errno("statx", shown(&base, path), errno_of(e)))?;
                return Ok(stat);
            }
            Base::Dir(fd) if empty => (fd.as_fd(), AtFlags::SYMLINK_NOFOLLOW | AtFlags::EMPTY_PATH),
            Base::Dir(fd) => (fd.as_fd(), AtFlags::SYMLINK_NOFOLLOW),
        };
        // statx on an autofs point must inspect the point, not trigger its
        // mount (which may lead to an unresponsive network filesystem).
        rustix::fs::statx(dir, path, flags | AtFlags::NO_AUTOMOUNT, STATX_MASK)
            .map(|x| stat_from_statx(&x))
            .map_err(|e| HostError::errno("statx", shown(&base, path), errno_of(e)))
    }

    fn executable_file(&self, path: &Path) -> Result<bool, HostError> {
        if !path.is_absolute() {
            return Err(HostError::invalid(
                "preflight executable",
                path.display(),
                "the executable path must be absolute",
            ));
        }
        match std::fs::metadata(path) {
            Ok(meta) => Ok(meta.is_file() && meta.permissions().mode() & 0o111 != 0),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(io_error(
                "preflight executable",
                path.display().to_string(),
                &error,
            )),
        }
    }

    fn command_path(&self) -> OsString {
        std::env::var_os("PATH").unwrap_or_default()
    }

    fn docker_environment(&self) -> DockerEnvironment {
        DockerEnvironment {
            host: std::env::var_os("DOCKER_HOST"),
            context: std::env::var_os("DOCKER_CONTEXT"),
        }
    }

    fn docker_query(&self, query: DockerQuery) -> Result<String, HostError> {
        run_docker_query_ungated(query, None, DOCKER_QUERY_TIMEOUT)
    }

    fn command_origin(&self) -> Result<std::path::PathBuf, HostError> {
        std::env::current_dir()
            .map_err(|error| io_error("preflight executable", "working directory", &error))
    }

    fn read_link(&self, dir: &LinuxFd, name: &OsStr) -> Result<PathBuf, HostError> {
        if !is_single_name(name) {
            return Err(HostError::invalid(
                "readlink",
                name.to_string_lossy(),
                "must name one entry below the held directory",
            ));
        }
        let text = rustix::fs::readlinkat(dir, name, Vec::new())
            .map_err(|e| HostError::errno("readlink", name.to_string_lossy(), errno_of(e)))?;
        Ok(PathBuf::from(OsString::from_vec(text.into_bytes())))
    }

    fn read_dir(&self, dir: &LinuxFd, limit: usize) -> Result<DirListing, HostError> {
        let subject = dir.to_string();
        let readable = reopen_readable(dir, &subject)?;
        let mut names = Vec::new();
        let mut too_many = false;
        for_each_name(&readable, |name| {
            if names.len() == limit {
                too_many = true;
                return false;
            }
            names.push(OsString::from_vec(name.to_vec()));
            true
        })
        .map_err(|e| HostError::errno("getdents", &subject, errno_of(e)))?;
        Ok(if too_many {
            DirListing::TooMany
        } else {
            DirListing::Names(names)
        })
    }

    fn listable(&self, dir: &LinuxFd) -> Result<(), HostError> {
        // `read_dir`'s own open, closed again unread.
        reopen_readable(dir, &dir.to_string()).map(drop)
    }

    fn read_head(&self, file: &LinuxFd, limit: usize) -> Result<Vec<u8>, HostError> {
        let subject = file.to_string();
        let reopened = self.open_regular(file, &subject, "only marker files are read")?;
        let mut head = Vec::new();
        std::fs::File::from(reopened)
            .take(u64::try_from(limit).unwrap_or(u64::MAX))
            .read_to_end(&mut head)
            .map_err(|e| io_error("read", &subject, &e))?;
        Ok(head)
    }

    fn reopen_regular(&self, file: &LinuxFd) -> Result<LinuxFd, HostError> {
        let subject = file.to_string();
        self.open_regular(file, &subject, "only lock files are reopened")
            .map(|fd| LinuxFd {
                fd,
                shown: file.shown.clone(),
            })
    }

    fn statvfs(&self, fd: &LinuxFd) -> Result<Space, HostError> {
        let subject = fd.to_string();
        let s = rustix::fs::fstatvfs(fd)
            .map_err(|e| HostError::errno("fstatvfs", &subject, errno_of(e)))?;
        let bytes = |blocks: u64, what: &str| {
            blocks.checked_mul(s.f_frsize).ok_or_else(|| {
                HostError::invalid(
                    "fstatvfs",
                    &subject,
                    format!("{what} {blocks} × f_frsize {} overflows", s.f_frsize),
                )
            })
        };
        Ok(Space {
            avail_bytes: bytes(s.f_bavail, "f_bavail")?,
            size_bytes: bytes(s.f_blocks, "f_blocks")?,
            block_bytes: s.f_frsize,
        })
    }

    fn flock_try(&self, fd: LinuxFd, mode: LockMode) -> Result<TryLock<LinuxFd>, HostError> {
        loop {
            match rustix::fs::flock(&fd, flock_operation(mode)) {
                Ok(()) => return Ok(TryLock::Acquired(Locked::granted(fd, mode))),
                Err(rustix::io::Errno::WOULDBLOCK) => return Ok(TryLock::Busy(fd)),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(HostError::errno("flock", &fd, errno_of(e))),
            }
        }
    }

    /// Polls with `LOCK_NB` (backing off from 10 ms to 100 ms) so that the
    /// deadline holds without signals; see docs/safety.md, Host lock, for
    /// what that costs. Each attempt is on the still unlocked fd.
    fn flock_wait(
        &self,
        fd: LinuxFd,
        mode: LockMode,
        deadline: Option<MonoTime>,
    ) -> Result<WaitLock<LinuxFd>, HostError> {
        let mut fd = fd;
        let mut pause = LOCK_POLL_FIRST;
        loop {
            if let Some(signal) = self.interrupted() {
                return Ok(WaitLock::Interrupted(signal));
            }
            fd = match self.flock_try(fd, mode)? {
                TryLock::Acquired(locked) => {
                    if let Some(signal) = self.interrupted() {
                        return Ok(WaitLock::Interrupted(signal));
                    }
                    return Ok(WaitLock::Acquired(locked));
                }
                TryLock::Busy(fd) => fd,
            };
            let now = self.now();
            let nap = match deadline {
                Some(deadline) if now >= deadline => return Ok(WaitLock::DeadlinePassed(fd)),
                Some(deadline) => pause.min(deadline.saturating_since(now)),
                None => pause,
            };
            self.sleep(nap);
            pause = (pause * 2).min(LOCK_POLL_MAX);
        }
    }

    fn interrupted(&self) -> Option<Signal> {
        match INTERRUPTED.load(Ordering::Relaxed) {
            signal if signal == rustix::process::Signal::INT.as_raw() => Some(Signal::Int),
            signal if signal == rustix::process::Signal::TERM.as_raw() => Some(Signal::Term),
            signal if signal == rustix::process::Signal::HUP.as_raw() => Some(Signal::Hup),
            _ => None,
        }
    }

    fn now(&self) -> MonoTime {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
        // CLOCK_BOOTTIME is never negative and tv_nsec is below 10^9.
        MonoTime(Duration::new(
            u64::try_from(t.tv_sec).expect("CLOCK_BOOTTIME seconds are non-negative"),
            u32::try_from(t.tv_nsec).expect("tv_nsec is below one second"),
        ))
    }

    fn wall_clock(&self) -> Timestamp {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Realtime);
        #[cfg(not(feature = "release-without-sandbox-gate"))]
        let secs = t
            .tv_sec
            .checked_add(i64::try_from(self.clock_offset_seconds).expect("bounded offset"))
            .expect("test clock offset exceeded timestamp range");
        #[cfg(feature = "release-without-sandbox-gate")]
        let secs = t.tv_sec;
        Timestamp {
            secs,
            // tv_nsec is below one second.
            nanos: u32::try_from(t.tv_nsec).expect("tv_nsec is below one second"),
        }
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn pressure(&self) -> Result<Pressure, HostError> {
        let read = |resource: &str| -> Result<u32, HostError> {
            let path = format!("pressure/{resource}");
            let text = self.root().read(&path)?;
            crate::pressure::some_avg10(&text)
                .map_err(|e| HostError::invalid("parse", ProcRoot::shown(&path), e))
        };
        Ok(Pressure {
            io: read("io")?,
            memory: read("memory")?,
            cpu: read("cpu")?,
        })
    }

    fn lower_priority(&self) -> Result<(), HostError> {
        /// The lowest scheduling priority (`nice 19`).
        const NICE_LOWEST: i32 = 19;
        // `None`: the calling thread (Linux's setpriority with who 0).
        rustix::process::setpriority_process(None, NICE_LOWEST)
            .map_err(|e| HostError::errno("setpriority", "nice 19", Errno(e.raw_os_error())))?;
        lower_io_priority_ungated()
            .map_err(|errno| HostError::errno("ioprio_set", "idle class", errno))
    }

    fn kernel_release(&self) -> Result<String, HostError> {
        let uname = rustix::system::uname();
        uname
            .release()
            .to_str()
            .map(str::to_owned)
            .map_err(|_| HostError::invalid("uname", "release", "not UTF-8"))
    }

    fn hostname(&self) -> Result<String, HostError> {
        rustix::system::uname()
            .nodename()
            .to_str()
            .map(str::to_owned)
            .map_err(|_| HostError::invalid("uname", "nodename", "not UTF-8"))
    }

    fn pids(&self) -> Result<Vec<Pid>, HostError> {
        proc_pids(self.root())
    }

    fn proc_identity(&self, pid: Pid) -> Result<Self::ProcIdentity, ProcError> {
        open_pidfd(pid).map(|fd| (pid, fd))
    }

    fn proc_identity_released(&self, identity: &Self::ProcIdentity) -> Result<bool, ProcError> {
        Ok(poll_pidfd(identity.0, &identity.1, Duration::ZERO)? == PidfdPoll::Hangup)
    }

    fn proc_fds(&self, pid: Pid) -> Result<Vec<ProcFd>, ProcError> {
        let proc = self.root();
        let pidfd = open_pidfd(pid)?;
        let mut stats = TaskStats::new(proc, pid);
        let per_task = collect(
            process(proc, pid, &pidfd, Want::EveryTask),
            Tasks {
                what: "fd table",
                read: |tid| task_fds(proc, pid, tid),
                is_empty: |fds: &Vec<ProcFd>| fds.is_empty(),
                stat: |tid| stats.read(tid),
                // Threads normally share one table (CLONE_FILES): read it
                // once, not once per thread (TASK-115).
                shares: |reference, tid| match same_fd_table(reference, tid) {
                    Ok(shared) => shared,
                    Err(errno) => {
                        self.kcmp_refused.fetch_add(1, Ordering::Relaxed);
                        self.kcmp_refused_errno.store(errno.0, Ordering::Relaxed);
                        false
                    }
                },
            },
            Want::EveryTask,
        )?;
        // Tasks sharing an fd table list the same fds: keep each once, by
        // sorting, not a set of copied link texts (an allocation per fd).
        let key = |f: &ProcFd| (f.fd, f.target.stat.dev_ino());
        let mut fds: Vec<ProcFd> = per_task.into_iter().flatten().collect();
        fds.sort_by(|a, b| {
            key(a)
                .cmp(&key(b))
                .then_with(|| a.target.text.cmp(&b.target.text))
        });
        fds.dedup_by(|a, b| key(a) == key(b) && a.target.text == b.target.text);
        Ok(fds)
    }

    fn proc_links(&self, pid: Pid, which: ProcLink) -> Result<Vec<LinkTarget>, ProcError> {
        let proc = self.root();
        let pidfd = open_pidfd(pid)?;
        let mut stats = TaskStats::new(proc, pid);
        let per_task = collect(
            process(proc, pid, &pidfd, Want::EveryTask),
            Tasks {
                what: which.name(),
                read: |tid| proc.link_target(&task_path(pid, tid, which.name())),
                // A link always names something; nothing is ENOENT.
                is_empty: |_: &LinkTarget| false,
                stat: |tid| stats.read(tid),
                shares: never_shared,
            },
            Want::EveryTask,
        )?;
        let mut seen = HashSet::new();
        Ok(per_task
            .into_iter()
            .filter(|t| seen.insert((t.stat.dev_ino(), t.text.clone())))
            .collect())
    }

    /// The three links of every task in the same rounds (one task listing
    /// and one `stat` bracket per task, not three; TASK-115). Each link is
    /// kept or found missing on its own: a task's links that are there are
    /// kept also when another of them vanished (an exiting task), as three
    /// [`Host::proc_links`] calls would keep them; one missing from a live
    /// task makes the round untrusted, as a missing link (`ENOENT`) does
    /// there.
    fn proc_links_all(&self, pid: Pid) -> Result<[Vec<LinkTarget>; 3], ProcError> {
        let proc = self.root();
        let pidfd = open_pidfd(pid)?;
        let mut stats = TaskStats::new(proc, pid);
        let per_task = collect(
            process(proc, pid, &pidfd, Want::EveryTask),
            Tasks {
                // A missing link reads as an incomplete (empty) read: see
                // `is_empty` below.
                what: "set of cwd, root and exe links",
                read: |tid| {
                    let mut links: [Option<LinkTarget>; 3] = [None, None, None];
                    for (link, which) in links.iter_mut().zip(ProcLink::ALL) {
                        *link = match proc.link_target(&task_path(pid, tid, which.name())) {
                            Ok(target) => Some(target),
                            Err(e) if is_vanished(&e) => None,
                            Err(e) => return Err(e),
                        };
                    }
                    Ok(links)
                },
                is_empty: links_incomplete,
                stat: |tid| stats.read(tid),
                shares: never_shared,
            },
            Want::EveryTask,
        )?;
        // Kept in first-seen order (the first `exe` names the command),
        // each once; a process's tasks show few distinct links, so a scan of
        // the kept ones costs less than a set of copied link texts.
        let mut all: [Vec<LinkTarget>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for links in per_task {
            for (kept, link) in all.iter_mut().zip(links) {
                if let Some(t) = link
                    && !kept
                        .iter()
                        .any(|k| k.stat.dev_ino() == t.stat.dev_ino() && k.text == t.text)
                {
                    kept.push(t);
                }
            }
        }
        Ok(all)
    }

    fn proc_maps(&self, pid: Pid) -> Result<Vec<MapEntry>, ProcError> {
        let proc = self.root();
        let (path, text) = read_through_live_task(proc, pid, ("maps", "memory map"), |path| {
            read_maps(&Kernel(proc), path)
        })?;
        parse_maps(&text)
            .map_err(|e| ProcError::Failed(HostError::invalid("parse", ProcRoot::shown(&path), e)))
    }

    fn proc_environ(&self, pid: Pid) -> Result<Environ, ProcError> {
        let proc = self.root();
        let (_, bytes) = read_through_live_task(proc, pid, ("environ", "environment"), |path| {
            read_environ(&Kernel(proc), path)
        })?;
        Ok(Environ::from_block(bytes))
    }

    /// Reads the processes on up to [`PROC_READERS`] threads (this one
    /// included), each taking the next unread pid: a scan of a desktop is
    /// dominated by multi-threaded processes whose every task's fd table is
    /// read (task-6 measured about 0.8 s for 480 processes on one thread).
    /// A helper thread that cannot be started leaves its share to the
    /// others; a reader that panics is a bug and panics here too.
    /// One note when `kcmp` was refused since the last call: the scan then
    /// read every thread's copy of shared fd tables, which on a busy host
    /// can make it older than the Freshness bound (TASK-115).
    fn take_proc_read_notes(&self) -> Vec<String> {
        let refused = self.kcmp_refused.swap(0, Ordering::Relaxed);
        if refused == 0 {
            return Vec::new();
        }
        let errno = Errno(self.kcmp_refused_errno.load(Ordering::Relaxed));
        vec![format!(
            "kcmp was refused {refused} times (last: {errno}), so threads' shared fd tables were \
             each read in full; the process scan is slower (a seccomp filter without \
             CAP_SYS_PTRACE, or a kernel without CONFIG_KCMP)"
        )]
    }

    fn proc_views(&self, pids: &[Pid], facts: bool) -> Vec<Result<ProcView, ProcError>> {
        let threads = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(PROC_READERS)
            .min(pids.len());
        let next = std::sync::atomic::AtomicUsize::new(0);
        let work = || {
            let mut done = Vec::new();
            loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(pid) = pids.get(i) else {
                    return done;
                };
                done.push((i, read_proc_view(self, *pid, facts)));
            }
        };
        let mut slots: Vec<Option<Result<ProcView, ProcError>>> = vec![None; pids.len()];
        std::thread::scope(|scope| {
            let helpers: Vec<_> = (1..threads)
                .filter_map(|_| {
                    std::thread::Builder::new()
                        .name("diskgc-proc".into())
                        .spawn_scoped(scope, work)
                        .ok()
                })
                .collect();
            let mut all = work();
            for helper in helpers {
                all.extend(helper.join().expect("a /proc reader thread panicked"));
            }
            for (i, view) in all {
                slots[i] = Some(view);
            }
        });
        slots
            .into_iter()
            .map(|slot| slot.expect("every pid was read by one thread"))
            .collect()
    }

    fn own_pid(&self) -> Pid {
        Pid(rustix::process::getpid()
            .as_raw_nonzero()
            .get()
            .cast_unsigned())
    }

    fn effective_uid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn account_uid(&self, name: &str) -> Result<Option<u32>, HostError> {
        lookup_account(name)
    }

    /// `fcntl(F_SETFD)` without `FD_CLOEXEC`, on that one fd: every other fd
    /// stays close-on-exec.
    fn inherit_lock(&self, lock: &Locked<LinuxFd>) -> Result<(), HostError> {
        let fd = lock.fd();
        let subject = fd.to_string();
        let flags = rustix::io::fcntl_getfd(fd)
            .map_err(|e| HostError::errno("fcntl F_GETFD", &subject, errno_of(e)))?;
        rustix::io::fcntl_setfd(fd, flags - rustix::io::FdFlags::CLOEXEC)
            .map_err(|e| HostError::errno("fcntl F_SETFD", &subject, errno_of(e)))
    }

    /// `status`, `uid_map` and the `fd` directory of a live task are read by
    /// number while the pidfd pins the process; a lookup that vanished
    /// (`ENOENT`) is `Gone` only when the pidfd then confirms release,
    /// otherwise `Failed`. The live task is chosen by
    /// [`crate::tasks::owner_through_live_task`].
    fn proc_status(&self, pid: Pid) -> Result<ProcStatus, ProcError> {
        let proc = self.root();
        let pidfd = open_pidfd(pid)?;
        let read = || -> Result<ProcStatus, ProcError> {
            let path = format!("{pid}/status");
            let text = proc.read(&path).map_err(ProcError::from_host)?;
            let ids = parse_status_ids(&text).map_err(|e| {
                ProcError::Failed(HostError::invalid("parse", ProcRoot::shown(&path), e))
            })?;
            // Optional: absent without user namespaces; then no excuse
            // that needs it.
            let uid_map = uid_map_of(proc.read(&format!("{pid}/uid_map")))?;
            let fd_dir_owner = match task_ids(proc, pid)? {
                None => None,
                Some(tids) => {
                    let mut stats = TaskStats::new(proc, pid);
                    owner_through_live_task(
                        &tids,
                        |tid| stats.read(tid),
                        |tid| proc.owner(&task_path(pid, tid, "fd")),
                    )
                    .map_err(ProcError::from_host)?
                }
            };
            Ok(ProcStatus {
                name: ids.name,
                uid: ids.uid,
                gid: ids.gid,
                cap_permitted: ids.cap_permitted,
                cap_effective: ids.cap_effective,
                uid_map,
                fd_dir_owner,
            })
        };
        match read() {
            Ok(status) => Ok(status),
            Err(ProcError::Failed(e)) if is_vanished(&e) => {
                match poll_pidfd(pid, &pidfd, Duration::ZERO)? {
                    PidfdPoll::Hangup => Err(ProcError::Gone),
                    PidfdPoll::Running | PidfdPoll::Readable => Err(ProcError::Failed(e)),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Two complete dumps, each over a fresh `NETLINK_SOCK_DIAG` socket,
    /// and their union (see [`crate::sockdiag`] for the request, the
    /// answer and what fails it, and `Host::bound_unix_sockets` for why
    /// one dump is not enough). Reads, never writes: the socket is not
    /// bound to any name. A receive timeout bounds a kernel that never
    /// answers.
    fn bound_unix_sockets(&self) -> Result<Vec<BoundSocket>, HostError> {
        sockdiag::union_of_two(sock_diag_dump)
    }

    /// `self/ns/pid` and `self/mountinfo`, both through the held `/proc`:
    /// the namespace file is opened (following the magic link to the nsfs
    /// inode) and `fstatfs`ed and `fstat`ed; the `hidepid` option is read
    /// from the held mount's own `mountinfo` line, found by the mount id
    /// `statx` reports for the held fd.
    fn pid_namespace(&self) -> Result<PidNamespace, HostError> {
        let proc = self.root();
        let ns = proc.open("self/ns/pid", OFlags::RDONLY)?;
        let shown = ProcRoot::shown("self/ns/pid");
        let fs = rustix::fs::fstatfs(&ns)
            .map_err(|e| HostError::errno("fstatfs", &shown, errno_of(e)))?;
        let st =
            rustix::fs::fstat(&ns).map_err(|e| HostError::errno("fstat", &shown, errno_of(e)))?;
        let initial = fs.f_type == NSFS_MAGIC && st.st_ino == PROC_PID_INIT_INO;
        let held = rustix::fs::statx(
            &self.proc,
            "",
            AtFlags::EMPTY_PATH | AtFlags::NO_AUTOMOUNT,
            rustix::fs::StatxFlags::MNT_ID,
        )
        .map_err(|e| HostError::errno("statx", ProcRoot::shown(""), errno_of(e)))?;
        if held.stx_mask & rustix::fs::StatxFlags::MNT_ID.bits() == 0 {
            return Err(HostError::invalid(
                "statx",
                ProcRoot::shown(""),
                "the kernel did not report the mount id",
            ));
        }
        let table = proc.read("self/mountinfo")?;
        let hidepid = hidepid_of(&table, held.stx_mnt_id)
            .map_err(|why| HostError::invalid("parse", ProcRoot::shown("self/mountinfo"), why))?;
        Ok(PidNamespace { initial, hidepid })
    }

    /// One read of `/proc/<pid>/status` under a pidfd taken first, as
    /// [`Host::proc_status`] does: a read that fails because the entry
    /// vanished is `Gone` only when the pidfd says the process has no task
    /// left.
    fn proc_nspid(&self, pid: Pid) -> Result<Vec<u32>, ProcError> {
        let proc = self.root();
        let pidfd = open_pidfd(pid)?;
        let path = format!("{pid}/status");
        let read = || -> Result<Vec<u32>, ProcError> {
            let text = proc.read(&path).map_err(ProcError::from_host)?;
            parse_nspid(&text, pid.0).map_err(|e| {
                ProcError::Failed(HostError::invalid("parse", ProcRoot::shown(&path), e))
            })
        };
        match read() {
            Ok(levels) => Ok(levels),
            Err(ProcError::Failed(e)) if is_vanished(&e) => {
                match poll_pidfd(pid, &pidfd, Duration::ZERO)? {
                    PidfdPoll::Hangup => Err(ProcError::Gone),
                    PidfdPoll::Running | PidfdPoll::Readable => Err(ProcError::Failed(e)),
                }
            }
            Err(e) => Err(e),
        }
    }

    fn pid_alive(&self, pid: Pid) -> Result<bool, HostError> {
        match open_pidfd(pid) {
            Ok(_) => Ok(true),
            Err(ProcError::Gone) => Ok(false),
            Err(ProcError::Denied(e) | ProcError::Failed(e) | ProcError::Exhausted(e)) => Err(e),
        }
    }

    fn fs_magic(&self, fd: &LinuxFd) -> Result<u64, HostError> {
        let fs =
            rustix::fs::fstatfs(fd).map_err(|e| HostError::errno("fstatfs", fd, errno_of(e)))?;
        fs_magic_of(i128::from(fs.f_type)).ok_or_else(|| {
            HostError::invalid(
                "fstatfs",
                fd,
                format!("f_type {:#x} is no 32-bit filesystem magic", fs.f_type),
            )
        })
    }

    /// One read of `/proc/<pid>/status` under a pidfd taken first, as
    /// [`Host::proc_nspid`] reads it.
    fn proc_uids(&self, pid: Pid) -> Result<diskgc_core::host::Ids, ProcError> {
        let proc = self.root();
        let pidfd = open_pidfd(pid)?;
        let path = format!("{pid}/status");
        let read = || -> Result<diskgc_core::host::Ids, ProcError> {
            let text = proc.read(&path).map_err(ProcError::from_host)?;
            parse_status_ids(&text).map(|ids| ids.uid).map_err(|e| {
                ProcError::Failed(HostError::invalid("parse", ProcRoot::shown(&path), e))
            })
        };
        match read() {
            Ok(uids) => Ok(uids),
            Err(ProcError::Failed(e)) if is_vanished(&e) => {
                match poll_pidfd(pid, &pidfd, Duration::ZERO)? {
                    PidfdPoll::Hangup => Err(ProcError::Gone),
                    PidfdPoll::Running | PidfdPoll::Readable => Err(ProcError::Failed(e)),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// The leader's `cmdline` first: every task shares the one memory map
    /// it is read from, so a non-empty read is the answer. A leader that
    /// called `pthread_exit` has no memory map and reads empty; then, and
    /// for any failed read, through a live task, as the environment is
    /// read (enumerating the tasks costs a `stat` per thread, which a
    /// browser's dozens of threads make worth avoiding).
    fn proc_cmdline(&self, pid: Pid) -> Result<Vec<OsString>, ProcError> {
        let proc = self.root();
        if let Ok(bytes) = read_cmdline(&Kernel(proc), &format!("{pid}/cmdline"))
            && !bytes.is_empty()
        {
            return Ok(parse_cmdline(&bytes));
        }
        let (_, bytes) = read_through_live_task(proc, pid, ("cmdline", "command line"), |path| {
            read_cmdline(&Kernel(proc), path)
        })?;
        Ok(parse_cmdline(&bytes))
    }

    fn proc_root_lookup(
        &self,
        pid: Pid,
        path: &Path,
        climb: bool,
    ) -> Result<Vec<DevIno>, ProcError> {
        if !path.is_absolute() {
            return Err(ProcError::Failed(HostError::invalid(
                "openat2",
                path.display(),
                "a temp-root value is looked up only when absolute",
            )));
        }
        let proc = self.root();
        let pidfd = open_pidfd(pid)?;
        let rel = format!("{pid}/root");
        // A root that cannot be opened (a leader that called pthread_exit
        // while other threads live has none, ENOENT) says nothing about
        // what exists inside it: the error carries no errno, so no caller
        // can read it as the absence of the path looked up.
        let unopened = |e: HostError| root_unopened(e, &rel);
        let root = match proc.open(&rel, OFlags::PATH | OFlags::DIRECTORY) {
            Ok(root) => root,
            Err(e) if is_vanished(&e) => {
                return match poll_pidfd(pid, &pidfd, Duration::ZERO)? {
                    PidfdPoll::Hangup => Err(ProcError::Gone),
                    PidfdPoll::Running | PidfdPoll::Readable => Err(unopened(e)),
                };
            }
            Err(e) => return Err(unopened(e)),
        };
        let shown = || format!("{} in {}", path.display(), ProcRoot::shown(&rel));
        let resolve = ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS;
        let mut attempts = 0;
        let object = loop {
            match rustix::fs::openat2(
                &root,
                path,
                OFlags::PATH | OFlags::CLOEXEC,
                Mode::empty(),
                resolve,
            ) {
                Ok(fd) => break fd,
                Err(rustix::io::Errno::AGAIN)
                    if attempts < diskgc_core::host::ROOT_LOOKUP_EAGAIN_RETRIES =>
                {
                    attempts += 1;
                }
                Err(e) => {
                    let on_proc =
                        || rustix::fs::fstatfs(&root).is_ok_and(|fs| fs.f_type == PROC_SUPER_MAGIC);
                    return Err(root_lookup_error(errno_of(e), on_proc, shown()));
                }
            }
        };
        let id_of = |fd: &OwnedFd| -> Result<(DevIno, bool), ProcError> {
            let stat = rustix::fs::statx(
                fd,
                "",
                AtFlags::EMPTY_PATH | AtFlags::NO_AUTOMOUNT | AtFlags::STATX_DONT_SYNC,
                STATX_MASK,
            )
            .map(|x| stat_from_statx(&x))
            // The object was opened: no failure from here on says that the
            // path is absent, so none carries an errno (unresolvable).
            .map_err(|e| {
                errno_erased(
                    errno_of(e),
                    "statx",
                    shown(),
                    "the opened object could not be statted",
                )
            })?;
            let id = stat.dev_ino().ok_or_else(|| {
                ProcError::Failed(HostError::invalid(
                    "statx",
                    shown(),
                    "the kernel reported no inode number",
                ))
            })?;
            Ok((id, stat.file_type == Some(FileType::Directory)))
        };
        let (first, is_dir) = id_of(&object)?;
        let mut chain = vec![first];
        if !(climb && is_dir) {
            return Ok(chain);
        }
        let mut here = object;
        for _ in 0..diskgc_core::host::CLIMB_LEVELS_MAX {
            let up = rustix::fs::openat(
                &here,
                "..",
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| {
                errno_erased(
                    errno_of(e),
                    "openat",
                    shown(),
                    "climbing from the opened object failed",
                )
            })?;
            let (id, _) = id_of(&up)?;
            if Some(&id) == chain.last() {
                return Ok(chain);
            }
            chain.push(id);
            here = up;
        }
        Err(ProcError::Failed(HostError::invalid(
            "climb",
            shown(),
            format!(
                "no fixpoint after {} levels of `..`",
                diskgc_core::host::CLIMB_LEVELS_MAX
            ),
        )))
    }

    fn fs_uuid(&self, dev: Dev) -> Result<Option<String>, HostError> {
        crate::state::fs_uuid(dev)
    }

    fn boot_id(&self) -> Result<String, HostError> {
        const BOOT_ID: &str = "sys/kernel/random/boot_id";
        let text = ProcRoot(self.proc.as_fd()).read(BOOT_ID)?;
        let id = String::from_utf8(text)
            .ok()
            .map(|t| t.trim_end().to_owned())
            .filter(|t| t.len() == 36 && t.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
            .ok_or_else(|| HostError::invalid("read", ProcRoot::shown(BOOT_ID), "not a boot id"))?;
        Ok(id)
    }

    fn state_lock(&self, key: &FsKey) -> Result<StateLock<LinuxFd>, HostError> {
        let (fd, shown, _) = crate::state::open_lock_file_ungated(self.state_dir()?, key)?;
        // The superblock's device, as mountinfo names it for the lock
        // file's mount (btrfs subvolumes share it; their st_dev differ).
        let mnt_id =
            rustix::fs::statx(&fd, "", AtFlags::EMPTY_PATH, rustix::fs::StatxFlags::MNT_ID)
                .map_err(|e| HostError::errno("statx", shown.display(), errno_of(e)))?
                .stx_mnt_id;
        let dir_dev = self
            .mountinfo()?
            .into_iter()
            .find(|m| m.id == mnt_id)
            .map(|m| m.dev)
            .ok_or_else(|| {
                HostError::invalid(
                    "mountinfo",
                    shown.display(),
                    format!("no mount {mnt_id} for the state directory"),
                )
            })?;
        Ok(StateLock {
            fd: LinuxFd { fd, shown },
            dir_dev,
        })
    }

    fn state_read(&self, key: &FsKey, limit: u64) -> Result<Option<StateFile>, HostError> {
        crate::state::read_snapshot(self.state_dir()?, key, limit, self.proc.as_fd())
    }

    fn user_lock_path(&self) -> Option<PathBuf> {
        let uid = rustix::process::geteuid().as_raw();
        if runtime_dir_is_users(uid) {
            return Some(diskgc_core::hostlock::runtime_user_lock(uid));
        }
        self.lock_home()
            .map(|home| diskgc_core::hostlock::home_user_lock(&home))
    }

    fn user_lock(&self, make: MakeUserLock) -> Result<Option<UserLock<LinuxFd>>, UserLockError> {
        let path = self.user_lock_path().ok_or(UserLockError::NoPlace)?;
        let dir = path.parent().expect("the lock is in a directory");
        let opened = crate::state::open_user_lock_ungated(dir, make, self.proc.as_fd())?;
        Ok(opened.map(|(fd, path)| UserLock {
            fd: LinuxFd {
                fd,
                shown: path.clone(),
            },
            path,
        }))
    }

    fn state_write(
        &self,
        lock: &Locked<LinuxFd>,
        key: &FsKey,
        bytes: &[u8],
    ) -> Result<(), HostError> {
        if lock.mode() != LockMode::Exclusive {
            return Err(HostError::invalid(
                "write",
                key.snapshot_name(),
                "the single-writer lock is not held exclusively",
            ));
        }
        crate::state::write_snapshot_ungated(
            self.state_dir()?,
            key,
            lock.fd().as_fd(),
            bytes,
            self.proc.as_fd(),
        )
    }
}

/// The error of a failed lookup in a process's root. `ESRCH` is kept (the
/// path's absence, as `ENOENT` is) only where its origin is known: the root
/// is a dead process's `/proc` directory (Chromium's sandbox chroots its
/// renderers into one), where nothing resolves for the process either.
/// Anywhere else it carries no errno: unresolvable, never absent.
/// `root_on_proc` is asked only for `ESRCH`.
fn root_lookup_error(
    errno: Errno,
    root_on_proc: impl FnOnce() -> bool,
    shown: String,
) -> ProcError {
    if errno == Errno::ESRCH && !root_on_proc() {
        return ProcError::Failed(HostError::invalid(
            "openat2",
            shown,
            "ESRCH outside a /proc root: unexplained, so unresolvable",
        ));
    }
    ProcError::failed(HostError::errno("openat2", shown, errno))
}

/// A process's root that could not be opened (a leader that called
/// `pthread_exit` while other threads live has none, `ENOENT`) says
/// nothing about what exists inside it: the error carries no errno, so no
/// caller can read it as the absence of the path looked up. diskgc out of
/// descriptors is the exception: kept as it is, an error of the run
/// (TASK-185), never an unresolvable lookup.
fn root_unopened(e: HostError, rel: &str) -> ProcError {
    if e.is_out_of_descriptors() {
        return ProcError::Exhausted(e);
    }
    ProcError::Failed(HostError::invalid(
        "open",
        ProcRoot::shown(rel),
        format!("the process's root cannot be opened: {e}"),
    ))
}

/// A failure after the looked-up object was opened: nothing from there on
/// says that the path is absent, so the error carries no errno
/// (unresolvable), except diskgc out of descriptors (`Exhausted`, an error
/// of the run, TASK-185).
fn errno_erased(errno: Errno, op: &'static str, shown: String, what: &str) -> ProcError {
    let e = HostError::errno(op, &shown, errno);
    if e.is_out_of_descriptors() {
        return ProcError::Exhausted(e);
    }
    ProcError::Failed(HostError::invalid(op, shown, format!("{what}: {errno}")))
}

/// What reading `/proc/<pid>/uid_map` gave: absent (`ENOENT`, a kernel
/// without user namespaces), or unreadable for another reason (kept for
/// the notes; only the capability excuse needs the map). diskgc out of
/// descriptors is no fact about the process: `Exhausted` (TASK-185).
fn uid_map_of(read: Result<Vec<u8>, HostError>) -> Result<UidMap, ProcError> {
    match read {
        Ok(bytes) => Ok(String::from_utf8(bytes)
            .map_or_else(|_| UidMap::Unreadable("not UTF-8".into()), UidMap::Map)),
        Err(e) if e.is(Errno::ENOENT) => Ok(UidMap::Absent),
        Err(e) if e.is_out_of_descriptors() => Err(ProcError::Exhausted(e)),
        Err(e) => Ok(UidMap::Unreadable(e.to_string())),
    }
}

/// `/proc/<pid>/cmdline`: the arguments, each ended by a NUL byte (the
/// last one's NUL may be missing when a process rewrote its argument
/// area). Nothing is dropped: an empty argument stays an empty string.
fn parse_cmdline(bytes: &[u8]) -> Vec<OsString> {
    let bytes = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .split(|b| *b == 0)
        .map(|arg| OsString::from_vec(arg.to_vec()))
        .collect()
}

/// A `statfs` `f_type` as a 32-bit magic, without folding: a value that
/// fits in 32 bits as it is, or a negative one that fits in an `i32` (a
/// 32-bit ABI's signed word holding a magic with the top bit set)
/// reinterpreted. Anything else is no magic (`None`), so the local list
/// can never be matched by truncation.
fn fs_magic_of(f_type: i128) -> Option<u64> {
    if let Ok(magic) = u32::try_from(f_type) {
        return Some(u64::from(magic));
    }
    i32::try_from(f_type)
        .ok()
        .map(|signed| u64::from(signed.cast_unsigned()))
}

/// `NSFS_MAGIC` (`include/uapi/linux/magic.h`): the filesystem of
/// `/proc/<pid>/ns/*` once followed.
const NSFS_MAGIC: rustix::fs::FsWord = 0x6e73_6673;

/// What the socket dumps say in messages.
const SOCK_DIAG_SUBJECT: &str = "AF_UNIX sockets of this network namespace";

/// One complete socket-diagnostics dump of the bound AF_UNIX sockets.
fn sock_diag_dump() -> Result<Vec<BoundSocket>, HostError> {
    use rustix::net::{
        AddressFamily, SendFlags, SocketFlags, SocketType, netlink, sockopt::Timeout,
    };
    let subject = SOCK_DIAG_SUBJECT;
    let failed = |op: &'static str| move |e| HostError::errno(op, subject, errno_of(e));
    let fd = rustix::net::socket_with(
        AddressFamily::NETLINK,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC,
        Some(netlink::SOCK_DIAG),
    )
    .map_err(failed("socket"))?;
    rustix::net::sockopt::set_socket_timeout(&fd, Timeout::Recv, Some(SOCK_DIAG_TIMEOUT))
        .map_err(failed("setsockopt"))?;
    let request = sockdiag::request(SOCK_DIAG_SEQ);
    let sent = loop {
        match rustix::net::send(&fd, &request, SendFlags::empty()) {
            Err(rustix::io::Errno::INTR) => continue,
            other => break other.map_err(failed("send"))?,
        }
    };
    if sent != request.len() {
        return Err(HostError::invalid(
            "send",
            subject,
            format!("sent {sent} of {} bytes", request.len()),
        ));
    }
    let mut sockets = Vec::new();
    let mut buf = vec![0u8; SOCK_DIAG_BUFFER];
    loop {
        let len = recv_datagram(&fd, &mut buf)?;
        if len == 0 {
            return Err(HostError::invalid(
                "recv",
                subject,
                "the dump ended without NLMSG_DONE",
            ));
        }
        let part = sockdiag::parse(&buf[..len], SOCK_DIAG_SEQ)
            .map_err(|e| HostError::invalid("sock_diag", subject, e))?;
        sockets.extend(part.sockets);
        if part.done {
            return Ok(sockets);
        }
    }
}

/// One datagram of a dump into `buf`; its length. `MSG_TRUNC` makes the
/// kernel return a datagram's real length even when it did not fit, so a
/// truncated datagram is an error rather than a shorter answer (the flag
/// truncates nothing on disk: it is a receive flag).
fn recv_datagram(fd: &OwnedFd, buf: &mut [u8]) -> Result<usize, HostError> {
    let (_, len) = loop {
        match rustix::net::recv(fd, &mut *buf, rustix::net::RecvFlags::TRUNC) {
            Err(rustix::io::Errno::INTR) => continue,
            other => {
                break other
                    .map_err(|e| HostError::errno("recv", SOCK_DIAG_SUBJECT, errno_of(e)))?;
            }
        }
    };
    sockdiag::whole_datagram(len, buf.len())
        .map_err(|e| HostError::invalid("recv", SOCK_DIAG_SUBJECT, e))
}

/// The most threads [`LinuxHost`]'s `proc_views` reads processes on.
const PROC_READERS: usize = 8;

/// The socket-diagnostics dump: its sequence number, its receive buffer
/// (larger than any datagram the kernel sends for a dump, which it caps at
/// 32 KiB; a larger one is detected, [`recv_datagram`]), and how long one receive
/// may wait.
const SOCK_DIAG_SEQ: u32 = 1;
const SOCK_DIAG_BUFFER: usize = 64 * 1024;
const SOCK_DIAG_TIMEOUT: Duration = Duration::from_secs(1);

/// Every method runs [`gate`] first and hands its [`Permit`] to its one
/// syscall wrapper ([`unlinkat_permitted`], [`fchmod_permitted`]), which
/// cannot be called without one. A source scan of every production crate
/// (`source_scan::tests::deleting_syscalls_only_behind_the_gate`) fails if a
/// deleting, content-destroying, child-starting or namespace-changing call
/// appears outside the allowlisted wrappers, so a future deleting method
/// cannot skip the gate.
impl Delete for LinuxHost {
    /// The gate's cached decision ([`sandbox::status`]); no [`Permit`] is
    /// made, nothing is attempted.
    fn may_delete(&self) -> Result<(), HostError> {
        sandbox::status().map_err(|why| HostError::sandbox_gate("delete", "anything", why))
    }

    fn unlinkat(&self, dir: &LinuxFd, name: &OsStr, what: Unlink) -> Result<(), HostError> {
        let subject = shown(&Base::Dir(dir), Path::new(name));
        let permit = gate("unlinkat", &subject)?;
        if self.interrupted().is_some() {
            return Err(HostError::invalid(
                "unlinkat",
                &subject,
                "interrupted before deletion",
            ));
        }
        if !is_single_name(name) {
            return Err(HostError::invalid(
                "unlinkat",
                &subject,
                "only a single name can be unlinked (not empty, no '/', not '.' or '..')",
            ));
        }
        let flags = match what {
            Unlink::NonDir => AtFlags::empty(),
            Unlink::Dir => AtFlags::REMOVEDIR,
        };
        unlinkat_permitted(&permit, dir, name, flags)
            .map_err(|e| HostError::errno("unlinkat", &subject, errno_of(e)))
    }

    fn fchmod(&self, dir: &LinuxFd, deadline: Option<MonoTime>) -> Result<bool, HostError> {
        let subject = dir.to_string();
        let permit = gate("fchmod", &subject)?;
        let stat = rustix::fs::fstat(dir)
            .map_err(|e| HostError::errno("fstat before fchmod", &subject, errno_of(e)))?;
        if FileType::from_mode(stat.st_mode) != FileType::Directory {
            return Err(HostError::errno("fchmod", &subject, Errno::ENOTDIR));
        }
        let mode = stat.st_mode & 0o7777;
        if mode & 0o200 != 0 {
            return Ok(true);
        }
        // fstat can block. The clock is the last operation before fchmod.
        if deadline.is_some_and(|at| self.now() >= at) {
            return Ok(false);
        }
        if self.interrupted().is_some() {
            return Err(HostError::invalid(
                "fchmod",
                &subject,
                "interrupted before permission change",
            ));
        }
        fchmod_permitted(&permit, dir, Mode::from_raw_mode(mode | 0o200))
            .map_err(|e| HostError::errno("fchmod", &subject, errno_of(e)))?;
        Ok(true)
    }

    fn run_command(&self, cwd: &LinuxFd, run: &CommandRun) -> CommandInvocation {
        let subject = run.executable.display().to_string();
        let permit = match gate("run command", &subject) {
            Ok(permit) => permit,
            Err(error) => return CommandInvocation::NotStarted(error),
        };
        if self.interrupted().is_some() {
            return CommandInvocation::NotStarted(HostError::invalid(
                "run command",
                &subject,
                "interrupted before child start",
            ));
        }
        if self.effective_uid() == 0 {
            return CommandInvocation::NotStarted(HostError::invalid(
                "run command",
                &subject,
                "command actions are refused when diskgc runs as root",
            ));
        }
        if !run.executable.is_absolute() {
            return CommandInvocation::NotStarted(HostError::invalid(
                "run command",
                &subject,
                "executable was not resolved to an absolute path during preflight",
            ));
        }
        if run.output_prefix.is_empty()
            || run.output_prefix.len() > MAX_COMMAND_OUTPUT_PREFIX_BYTES
            || run.output_prefix.contains('\n')
            || run.output_prefix.contains('\r')
        {
            return CommandInvocation::NotStarted(HostError::invalid(
                "run command",
                &subject,
                format!(
                    "output prefix must be non-empty, at most {MAX_COMMAND_OUTPUT_PREFIX_BYTES} bytes, and contain no line break"
                ),
            ));
        }
        for (key, value) in &run.environment {
            if key.is_empty()
                || key.as_bytes().contains(&b'=')
                || key.as_bytes().contains(&0)
                || value.as_bytes().contains(&0)
            {
                return CommandInvocation::NotStarted(HostError::invalid(
                    "run command",
                    &subject,
                    format!("invalid environment assignment for {key:?}"),
                ));
            }
        }
        if run.deadline <= self.now() {
            return CommandInvocation::DeadlineBeforeStart;
        }
        let stat = match rustix::fs::fstat(cwd) {
            Ok(stat) => stat,
            Err(error) => {
                return CommandInvocation::NotStarted(HostError::errno(
                    "fstat command cwd",
                    format!("{cwd:?}"),
                    errno_of(error),
                ));
            }
        };
        if FileType::from_mode(stat.st_mode) != FileType::Directory {
            return CommandInvocation::NotStarted(HostError::invalid(
                "run command",
                format!("{cwd:?}"),
                "checked working-directory fd is no longer a directory",
            ));
        }
        run_command_permitted(&permit, self, cwd, run)
    }
}

/// `dir` itself, opened anew `O_RDONLY | O_DIRECTORY | O_NOATIME`: `.` below `dir`
/// names `dir`, so nothing is resolved and no symlink or mount can be met,
/// and `dir` may be an `O_PATH` fd. A fresh open also gives a listing its own
/// position (the fd's would be shared with any dup of it). If the caller
/// does not own the directory, Linux rejects `O_NOATIME` with `EPERM`; retry
/// without it so a readable directory remains listable.
fn reopen_readable(dir: &LinuxFd, subject: &str) -> Result<OwnedFd, HostError> {
    let base = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    open_with_noatime(base, |flags| {
        rustix::fs::openat(dir, ".", flags, Mode::empty())
    })
    .map_err(|e| HostError::errno("open", subject, errno_of(e)))
}

/// Retry only the documented ownership/capability refusal. Other open
/// errors must reach the caller unchanged, especially a vanished directory.
fn open_with_noatime<T>(
    base: OFlags,
    mut open: impl FnMut(OFlags) -> rustix::io::Result<T>,
) -> rustix::io::Result<T> {
    match open(base | OFlags::NOATIME) {
        Err(rustix::io::Errno::PERM) => open(base),
        result => result,
    }
}

/// The pre-release sandbox gate for deleting operation `op` on `subject`
/// (docs/safety.md, Pre-release sandbox gate): a [`Permit`] inside a diskgc
/// test container, otherwise a [`diskgc_core::host::Cause::SandboxGate`]
/// error naming the marker that is missing or wrong. Nothing is attempted
/// after a refusal.
fn gate(op: &'static str, subject: &str) -> Result<Permit, HostError> {
    sandbox::permit().map_err(|why| HostError::sandbox_gate(op, subject, why))
}

/// `unlinkat(dir, name, flags)`: the only deleting syscall in this crate.
/// The [`Permit`] proves the gate allowed it.
fn unlinkat_permitted(
    _gate: &Permit,
    dir: &LinuxFd,
    name: &OsStr,
    flags: AtFlags,
) -> rustix::io::Result<()> {
    rustix::fs::unlinkat(dir, name, flags)
}

/// `fchmod(dir, mode)`: the only permission change in this crate. The
/// [`Permit`] proves the gate allowed it.
fn fchmod_permitted(_gate: &Permit, dir: &LinuxFd, mode: Mode) -> rustix::io::Result<()> {
    rustix::fs::fchmod(dir, mode)
}

/// The only child-starting site in diskgc-linux. The held `Permit` is the
/// sandbox proof. `pre_exec` uses only async-signal-safe syscalls: `fchdir`
/// on the already checked fd and the open-file limit's restore; `process_group(0)` makes the child a new group
/// leader before it can run any profile code.
// At most one such segment is buffered for each of the two pipes. A line
// without a newline is emitted as successive segments, rather than held
// until EOF. The read buffer for each drain has this size too.
const COMMAND_OUTPUT_CHUNK: usize = 4096;
const COMMAND_READS_PER_PUMP: usize = 16;
const COMMAND_OUTPUT_DRAIN_AFTER_EXIT: Duration = Duration::from_secs(1);
const COMMAND_STDERR_BACKPRESSURE: Duration = Duration::from_millis(50);
const COMMAND_TERMINATION_GRACE: Duration = Duration::from_secs(10);

const DOCKER_QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const DOCKER_QUERY_MAX_OUTPUT: usize = 1024 * 1024;

/// An `O_PATH` reference to the verified socket inode. A Docker client
/// connecting through `/proc/self/fd/N` reaches this inode even if another
/// process replaces `/var/run/docker.sock` after the backend recheck.
struct DockerSocketPin {
    fd: OwnedFd,
    host: String,
}

fn pin_docker_socket() -> Result<DockerSocketPin, HostError> {
    const SOCKET: &str = "/var/run/docker.sock";
    let fd = rustix::fs::open(
        SOCKET,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| HostError::errno("pin Docker socket", SOCKET, errno_of(error)))?;
    let stat = rustix::fs::fstat(&fd)
        .map_err(|error| HostError::errno("stat pinned Docker socket", SOCKET, errno_of(error)))?;
    if FileType::from_mode(stat.st_mode) != FileType::Socket {
        return Err(HostError::invalid(
            "pin Docker socket",
            SOCKET,
            "endpoint is not a Unix socket inode",
        ));
    }
    let host = format!("unix:///proc/self/fd/{}", fd.as_raw_fd());
    Ok(DockerSocketPin { fd, host })
}

/// The sole ungated child-starting site: fixed, read-only Docker queries.
/// The caller supplies an enum, never argv. Both pipes are drained within a
/// byte bound, and the whole process group is killed if the query hangs.
fn run_docker_query_ungated(
    query: DockerQuery,
    pinned: Option<(&Path, &DockerSocketPin)>,
    timeout: Duration,
) -> Result<String, HostError> {
    let args: &[&str] = match query {
        DockerQuery::Version => &["version", "--format", "{{json .Server}}"],
        DockerQuery::ClientVersion => &["version", "--format", "{{json .Client}}"],
        DockerQuery::ContextShow => &["context", "show"],
        DockerQuery::ContextEndpoint => &[
            "context",
            "inspect",
            "default",
            "--format",
            "{{json .Endpoints.docker.Host}}",
        ],
        DockerQuery::Info => &["info", "--format", "{{json .}}"],
    };
    let subject = format!("docker {args:?}");
    let mut command =
        process::Command::new(pinned.map_or(Path::new("docker"), |(program, _)| program));
    if let Some((_, pin)) = pinned {
        command
            .arg(format!("--host={}", pin.host))
            .env("DOCKER_HOST", &pin.host)
            .env("DOCKER_CONTEXT", "default");
        let raw_fd = pin.fd.as_raw_fd();
        // SAFETY: after fork the child changes only one inherited descriptor
        // flag by syscall. The parent keeps its descriptor close-on-exec.
        unsafe {
            command.pre_exec(move || {
                let fd = BorrowedFd::borrow_raw(raw_fd);
                rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())
                    .map_err(std::io::Error::from)
            });
        }
    }
    // SAFETY: async-signal-safe (atomic loads and one syscall); runs after
    // the closure above.
    unsafe {
        command.pre_exec(crate::nofile::restore_inherited_in_child);
    }
    let mut child = command
        .args(args)
        .process_group(0)
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped())
        .spawn()
        .map_err(|error| io_error("start Docker query", &subject, &error))?;
    collect_docker_query_output(&mut child, &subject, timeout)
}

/// Keep the group leader waitable until its entire group has been dealt
/// with. A descendant may inherit a pipe after the leader exits, and
/// reaping the leader early would free the process-group ID before KILL.
fn collect_docker_query_output(
    child: &mut process::Child,
    subject: &str,
    timeout: Duration,
) -> Result<String, HostError> {
    let pid = match rustix::process::Pid::from_raw(child.id().cast_signed()) {
        Some(pid) => pid,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(HostError::invalid(
                "start Docker query",
                subject,
                "child has invalid pid",
            ));
        }
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let setup = (|| -> Result<(), HostError> {
        for fd in [stdout.as_fd(), stderr.as_fd()] {
            let flags = rustix::fs::fcntl_getfl(fd).map_err(|error| {
                HostError::errno("fcntl Docker output", subject, errno_of(error))
            })?;
            rustix::fs::fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(|error| {
                HostError::errno("fcntl Docker output", subject, errno_of(error))
            })?;
        }
        Ok(())
    })();
    if let Err(error) = setup {
        let _ = signal_command_group(pid, rustix::process::Signal::KILL, subject);
        let _ = child.wait();
        return Err(error);
    }
    let deadline = Instant::now() + timeout.min(DOCKER_QUERY_TIMEOUT);
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let observed = loop {
        if Instant::now() >= deadline {
            break Err(HostError::invalid(
                "query Docker",
                subject,
                format!("read-only query exceeded {timeout:?}"),
            ));
        }
        if !stdout_eof {
            match read_docker_query_pipe(&mut stdout, &mut stdout_bytes, subject) {
                Ok(eof) => stdout_eof = eof,
                Err(error) => break Err(error),
            }
        }
        if !stderr_eof {
            match read_docker_query_pipe(&mut stderr, &mut stderr_bytes, subject) {
                Ok(eof) => stderr_eof = eof,
                Err(error) => break Err(error),
            }
        }
        let exited = match rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        ) {
            Ok(exited) => exited.is_some(),
            Err(error) => {
                break Err(HostError::errno(
                    "observe Docker query exit",
                    subject,
                    errno_of(error),
                ));
            }
        };
        if exited && stdout_eof && stderr_eof {
            break Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    };
    // Even after EOF, a background process may have closed its pipes and
    // kept running. The unreaped leader pins the group ID through KILL.
    let signaled = signal_command_group(pid, rustix::process::Signal::KILL, subject);
    let status = child
        .wait()
        .map_err(|error| io_error("wait Docker query", subject, &error))?;
    observed?;
    signaled?;
    let stderr = String::from_utf8_lossy(&stderr_bytes);
    if !status.success() {
        return Err(HostError::invalid(
            "query Docker",
            subject,
            format!("exited {status}: {stderr}"),
        ));
    }
    String::from_utf8(stdout_bytes).map_err(|error| {
        HostError::invalid(
            "query Docker",
            subject,
            format!("stdout is not UTF-8: {error}"),
        )
    })
}

/// Pump only a bounded number of nonblocking reads before checking the
/// deadline and the other pipe. The output cap is per pipe.
fn read_docker_query_pipe(
    pipe: &mut impl Read,
    bytes: &mut Vec<u8>,
    subject: &str,
) -> Result<bool, HostError> {
    let mut chunk = [0u8; 4096];
    for _ in 0..COMMAND_READS_PER_PUMP {
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                let remaining = DOCKER_QUERY_MAX_OUTPUT.saturating_sub(bytes.len());
                if n > remaining {
                    return Err(HostError::invalid(
                        "query Docker",
                        subject,
                        "output exceeded 1 MiB",
                    ));
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(io_error("read Docker output", subject, &error)),
        }
    }
    Ok(false)
}

struct OutputMessage {
    fd: OwnedFd,
    bytes: Vec<u8>,
    done: SyncSender<std::io::Result<()>>,
}

/// One writer for the process, ever. The zero-capacity channel holds no
/// queued output. If a consumer stalls the writer in `write_all`, later
/// actions fail to hand off output; they never create another blocked thread.
static COMMAND_OUTPUT_WRITER: OnceLock<Result<SyncSender<OutputMessage>, String>> = OnceLock::new();

fn command_output_writer(subject: &str) -> Result<&'static SyncSender<OutputMessage>, HostError> {
    match COMMAND_OUTPUT_WRITER.get_or_init(|| {
        let (send, receive) = mpsc::sync_channel::<OutputMessage>(0);
        thread::Builder::new()
            .name("diskgc-command-output".to_owned())
            .spawn(move || {
                for message in receive {
                    let result = std::fs::File::from(message.fd).write_all(&message.bytes);
                    let _ = message.done.send(result);
                }
            })
            .map_err(|error| format!("start command output writer: {error}"))?;
        Ok(send)
    }) {
        Ok(send) => Ok(send),
        Err(error) => Err(HostError::invalid("forward command output", subject, error)),
    }
}

fn run_command_permitted(
    _gate: &Permit,
    host: &LinuxHost,
    cwd: &LinuxFd,
    run: &CommandRun,
) -> CommandInvocation {
    let subject = run.executable.display().to_string();
    let docker_pin = if let Some(expected) = &run.docker_info {
        if run.arguments.first().and_then(|arg| arg.to_str())
            != Some("--host=unix:///var/run/docker.sock")
        {
            return CommandInvocation::NotStarted(HostError::invalid(
                "pin Docker prune",
                &subject,
                "Docker action has no fixed local socket argument",
            ));
        }
        let pin = match pin_docker_socket() {
            Ok(pin) => pin,
            Err(error) => return CommandInvocation::NotStarted(error),
        };
        let remaining = run.deadline.0.saturating_sub(host.now().0);
        if remaining.is_zero() {
            return CommandInvocation::DeadlineBeforeStart;
        }
        let info = match run_docker_query_ungated(
            DockerQuery::Info,
            Some((&run.executable, &pin)),
            remaining,
        ) {
            Ok(info) => info,
            Err(error) => {
                if run.deadline <= host.now() {
                    return CommandInvocation::DeadlineBeforeStart;
                }
                return CommandInvocation::NotStarted(error);
            }
        };
        let actual = match diskgc_core::docker_backend::DockerInfo::parse(&info) {
            Ok(actual) => actual,
            Err(error) => {
                return CommandInvocation::NotStarted(HostError::invalid(
                    "pin Docker prune",
                    &subject,
                    error,
                ));
            }
        };
        if &actual != expected {
            return CommandInvocation::NotStarted(HostError::invalid(
                "pin Docker prune",
                &subject,
                format!("pinned daemon info changed: expected {expected:?}, got {actual:?}"),
            ));
        }
        Some(pin)
    } else {
        None
    };
    let mut arguments = run.arguments.clone();
    if let Some(pin) = &docker_pin {
        arguments[0] = format!("--host={}", pin.host).into();
    }
    let mut command = process::Command::new(&run.executable);
    command
        .args(&arguments)
        .envs(run.environment.iter().cloned());
    if let Some(pin) = &docker_pin {
        command.env("DOCKER_HOST", &pin.host);
    }
    if run.deadline <= host.now() {
        return CommandInvocation::DeadlineBeforeStart;
    }
    if host.interrupted().is_some() {
        return CommandInvocation::NotStarted(HostError::invalid(
            "run command",
            &subject,
            "interrupted before child start",
        ));
    }
    command
        .process_group(0)
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped());
    let cwd_fd = cwd.as_fd().as_raw_fd();
    let docker_fd: Option<RawFd> = docker_pin.as_ref().map(|pin| pin.fd.as_raw_fd());
    // SAFETY: the closure calls syscalls on an fd held by the parent
    // through spawn (the child inherits it until pre_exec returns) and
    // `restore_inherited_in_child` (atomic loads, one syscall). It does not
    // allocate or lock.
    unsafe {
        command.pre_exec(move || {
            // SAFETY: `cwd` is alive through `spawn`; after fork this child
            // owns its copy of the same fd until exec.
            let fd = BorrowedFd::borrow_raw(cwd_fd);
            rustix::process::fchdir(fd).map_err(std::io::Error::from)?;
            if let Some(raw_fd) = docker_fd {
                let fd = BorrowedFd::borrow_raw(raw_fd);
                rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())
                    .map_err(std::io::Error::from)?;
            }
            // The profile's command gets the open-file limit diskgc was
            // started with, not the raised one (crate::nofile).
            crate::nofile::restore_inherited_in_child()
        });
    }
    if host.interrupted().is_some() {
        return CommandInvocation::NotStarted(HostError::invalid(
            "run command",
            &subject,
            "interrupted before child start",
        ));
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return CommandInvocation::NotStarted(io_error("spawn command", &subject, &error));
        }
    };
    let Some(pid) = rustix::process::Pid::from_raw(child.id().cast_signed()) else {
        let _ = child.kill();
        let _ = child.wait();
        return CommandInvocation::Started(Err(HostError::invalid(
            "run command",
            &subject,
            "spawned child returned an invalid pid",
        )));
    };
    let mut leader_reaped = false;
    let result = drive_command_output(host, run, &subject, pid, &mut child, &mut leader_reaped);
    if result.is_err() && !leader_reaped {
        // An output or wait failure must not leave a trusted cleaner
        // running after the caller has been told the action failed. A signal
        // or deadline uses the same TERM grace even if output setup failed.
        if host.interrupted().is_some() || host.now() >= run.deadline {
            let _ = signal_command_group(pid, rustix::process::Signal::TERM, &subject);
            let until = host.now().checked_add(COMMAND_TERMINATION_GRACE);
            while until.is_some_and(|until| host.now() < until) {
                // An exited leader does not prove its descendants exited.
                // Keep it waitable to pin the process-group ID until KILL.
                let remaining = until
                    .expect("loop requires an endpoint")
                    .saturating_since(host.now());
                host.sleep(Duration::from_millis(20).min(remaining));
            }
        }
        let _ = signal_command_group(pid, rustix::process::Signal::KILL, &subject);
        let _ = child.wait();
    }
    CommandInvocation::Started(result)
}

/// Reads both pipes in bounded, nonblocking chunks while monitoring the
/// deadline. A child cannot fill one pipe and block this thread from
/// observing the other or the deadline. Its output is forwarded to stderr
/// with a prefix; diskgc's stdout stays reserved for the result.
fn drive_command_output(
    host: &LinuxHost,
    run: &CommandRun,
    subject: &str,
    pid: rustix::process::Pid,
    child: &mut process::Child,
    leader_reaped: &mut bool,
) -> Result<CommandExit, HostError> {
    let mut stdout = child.stdout.take().ok_or_else(|| {
        HostError::invalid("run command", subject, "spawned child has no stdout pipe")
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        HostError::invalid("run command", subject, "spawned child has no stderr pipe")
    })?;
    for fd in [stdout.as_fd(), stderr.as_fd()] {
        let flags = rustix::fs::fcntl_getfl(fd)
            .map_err(|e| HostError::errno("fcntl command output", subject, errno_of(e)))?;
        rustix::fs::fcntl_setfl(fd, flags | OFlags::NONBLOCK)
            .map_err(|e| HostError::errno("fcntl command output", subject, errno_of(e)))?;
    }
    let prefix = format!("diskgc: {}: ", run.output_prefix);
    let target = OutputTarget {
        host,
        deadline: run.deadline,
        prefix: &prefix,
        subject,
    };
    let mut out_line = OutputLine::default();
    let mut err_line = OutputLine::default();
    let poll = Duration::from_millis(20);
    macro_rules! terminate {
        ($grace:expr) => {
            return terminate_command_group(
                host,
                subject,
                pid,
                child,
                leader_reaped,
                &mut stdout,
                &mut stderr,
                &mut out_line,
                &mut err_line,
                target,
                $grace,
            )
        };
    }
    loop {
        if host.interrupted().is_some() {
            terminate!(COMMAND_TERMINATION_GRACE);
        }
        let forwarded = drain_command_output(&mut stdout, &mut out_line, target)
            .and_then(|()| drain_command_output(&mut stderr, &mut err_line, target));
        if let Err(error) = forwarded {
            if host.interrupted().is_some() {
                terminate!(COMMAND_TERMINATION_GRACE);
            }
            if host.now() >= run.deadline {
                terminate!(COMMAND_TERMINATION_GRACE);
            }
            return Err(error);
        }
        let exited = match rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        ) {
            Ok(exited) => exited.is_some(),
            Err(_) if host.interrupted().is_some() => terminate!(COMMAND_TERMINATION_GRACE),
            Err(error) => {
                return Err(HostError::errno(
                    "observe command exit",
                    subject,
                    errno_of(error),
                ));
            }
        };
        if exited {
            // Leave the exited leader waitable while draining descendant
            // output. It pins the process-group ID if a descendant holds a
            // pipe open past the drain limit and must be KILLed on error.
            let drain = match finish_command_output(
                &mut stdout,
                &mut stderr,
                &mut out_line,
                &mut err_line,
                target,
            ) {
                Ok(drain) => drain,
                Err(_) if host.interrupted().is_some() => terminate!(COMMAND_TERMINATION_GRACE),
                Err(_) if host.now() >= run.deadline => OutputDrain::Deadline,
                Err(error) => return Err(error),
            };
            if host.interrupted().is_some() {
                terminate!(COMMAND_TERMINATION_GRACE);
            }
            if drain == OutputDrain::Deadline || host.now() >= run.deadline {
                terminate!(COMMAND_TERMINATION_GRACE);
            }
            // A background cleaner may close its inherited output pipes.
            // Kill any surviving member before reaping the leader; its
            // waitable PID still reserves the process-group ID here.
            signal_command_group(pid, rustix::process::Signal::KILL, subject)?;
            let status = child
                .wait()
                .map_err(|e| io_error("wait command", subject, &e))?;
            *leader_reaped = true;
            return command_exit(status, subject);
        }
        if host.now() >= run.deadline {
            terminate!(COMMAND_TERMINATION_GRACE);
        }
        host.sleep(poll.min(run.deadline.saturating_since(host.now())));
    }
}

/// After a deadline or signal, TERM output has its own bounded forwarding
/// window. A stalled parent stderr cannot delay KILL indefinitely. The
/// deadline and signal cancellation both allow up to 10 seconds before
/// KILL. The executor maps the result to interruption when its flag is set.
#[allow(clippy::too_many_arguments)]
fn terminate_command_group(
    host: &LinuxHost,
    subject: &str,
    pid: rustix::process::Pid,
    child: &mut process::Child,
    leader_reaped: &mut bool,
    stdout: &mut process::ChildStdout,
    stderr: &mut process::ChildStderr,
    out_line: &mut OutputLine,
    err_line: &mut OutputLine,
    target: OutputTarget<'_>,
    grace_duration: Duration,
) -> Result<CommandExit, HostError> {
    signal_command_group(pid, rustix::process::Signal::TERM, subject)?;
    let grace = host
        .now()
        .checked_add(grace_duration)
        .ok_or_else(|| HostError::invalid("run command", subject, "clock overflow"))?;
    let grace_target = OutputTarget {
        deadline: grace,
        ..target
    };
    let mut draining = true;
    while host.now() < grace {
        // The leader remains waitable so its PID pins the group until KILL.
        if draining
            && (drain_command_output(stdout, out_line, grace_target).is_err()
                || drain_command_output(stderr, err_line, grace_target).is_err())
        {
            // Forwarding failure cannot shorten the child's grace period.
            draining = false;
        }
        host.sleep(Duration::from_millis(20).min(grace.saturating_since(host.now())));
    }
    signal_command_group(pid, rustix::process::Signal::KILL, subject)?;
    child
        .wait()
        .map_err(|e| io_error("wait command", subject, &e))?;
    *leader_reaped = true;
    // Already-readable bytes and unterminated lines get one further bounded
    // chance to reach stderr. Failure is expected when its consumer stalled.
    let tail_deadline = host
        .now()
        .checked_add(COMMAND_STDERR_BACKPRESSURE)
        .ok_or_else(|| HostError::invalid("run command", subject, "clock overflow"))?;
    let tail_target = OutputTarget {
        deadline: tail_deadline,
        ..target
    };
    let _ = drain_command_output(stdout, out_line, tail_target);
    let _ = drain_command_output(stderr, err_line, tail_target);
    let _ = out_line.flush(tail_target);
    let _ = err_line.flush(tail_target);
    Ok(CommandExit::TimedOut)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputDrain {
    Complete,
    Deadline,
}

#[derive(Clone, Copy)]
struct OutputTarget<'a> {
    host: &'a LinuxHost,
    deadline: MonoTime,
    prefix: &'a str,
    subject: &'a str,
}

fn finish_command_output(
    stdout: &mut process::ChildStdout,
    stderr: &mut process::ChildStderr,
    out_line: &mut OutputLine,
    err_line: &mut OutputLine,
    target: OutputTarget<'_>,
) -> Result<OutputDrain, HostError> {
    let until = target
        .host
        .now()
        .checked_add(COMMAND_OUTPUT_DRAIN_AFTER_EXIT)
        .ok_or_else(|| {
            HostError::invalid("read command output", target.subject, "clock overflow")
        })?;
    while !(out_line.eof && err_line.eof) {
        if target.host.now() >= target.deadline {
            return Ok(OutputDrain::Deadline);
        }
        drain_command_output(stdout, out_line, target)?;
        drain_command_output(stderr, err_line, target)?;
        if out_line.eof && err_line.eof {
            break;
        }
        if target.host.now() >= target.deadline {
            return Ok(OutputDrain::Deadline);
        }
        if target.host.now() >= until {
            return Err(HostError::invalid(
                "read command output",
                target.subject,
                "output pipe remained open after the command leader exited; output may be incomplete",
            ));
        }
        target.host.sleep(
            Duration::from_millis(5).min(target.deadline.saturating_since(target.host.now())),
        );
    }
    out_line.flush(target)?;
    err_line.flush(target)?;
    Ok(if target.host.now() >= target.deadline {
        OutputDrain::Deadline
    } else {
        OutputDrain::Complete
    })
}

#[derive(Default)]
struct OutputLine {
    bytes: Vec<u8>,
    eof: bool,
}

impl OutputLine {
    fn push(&mut self, bytes: &[u8], target: OutputTarget<'_>) -> Result<(), HostError> {
        for &byte in bytes {
            self.bytes.push(byte);
            if byte == b'\n' || self.bytes.len() == COMMAND_OUTPUT_CHUNK {
                self.flush(target)?;
            }
        }
        Ok(())
    }

    fn flush(&mut self, target: OutputTarget<'_>) -> Result<(), HostError> {
        if self.bytes.is_empty() {
            return Ok(());
        }
        let mut segment = Vec::with_capacity(target.prefix.len() + self.bytes.len() + 1);
        segment.extend_from_slice(target.prefix.as_bytes());
        segment.extend_from_slice(&self.bytes);
        if !self.bytes.ends_with(b"\n") {
            segment.push(b'\n');
        }
        write_command_stderr(segment, target)?;
        self.bytes.clear();
        Ok(())
    }
}

fn write_command_stderr(bytes: Vec<u8>, target: OutputTarget<'_>) -> Result<(), HostError> {
    let writer = command_output_writer(target.subject)?;
    let fd = rustix::io::fcntl_dupfd_cloexec(std::io::stderr().as_fd(), 3)
        .map_err(|e| HostError::errno("duplicate stderr", target.subject, errno_of(e)))?;
    let (done_send, done_receive) = mpsc::sync_channel(1);
    let mut message = OutputMessage {
        fd,
        bytes,
        done: done_send,
    };
    let backpressure_until = target
        .host
        .now()
        .checked_add(COMMAND_STDERR_BACKPRESSURE)
        .unwrap_or(target.deadline)
        .min(target.deadline);
    loop {
        match writer.try_send(message) {
            Ok(()) => break,
            Err(TrySendError::Full(held)) => {
                message = held;
                let now = target.host.now();
                if now >= backpressure_until {
                    return Err(HostError::invalid(
                        "forward command output",
                        target.subject,
                        "stderr writer is not accepting output without blocking",
                    ));
                }
                target
                    .host
                    .sleep(Duration::from_millis(2).min(backpressure_until.saturating_since(now)));
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(HostError::invalid(
                    "forward command output",
                    target.subject,
                    "stderr writer stopped",
                ));
            }
        }
    }
    loop {
        // A completion already in the channel wins at the boundary. Do not
        // call an otherwise completed write a timeout due to scheduler lag.
        match done_receive.try_recv() {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => {
                return Err(io_error("forward command output", target.subject, &error));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(HostError::invalid(
                    "forward command output",
                    target.subject,
                    "stderr writer stopped without a result",
                ));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let now = target.host.now();
        if now >= backpressure_until {
            return Err(HostError::invalid(
                "forward command output",
                target.subject,
                "stderr is not accepting output without blocking",
            ));
        }
        match done_receive
            .recv_timeout(Duration::from_millis(2).min(backpressure_until.saturating_since(now)))
        {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => {
                return Err(io_error("forward command output", target.subject, &error));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(HostError::invalid(
                    "forward command output",
                    target.subject,
                    "stderr writer stopped without a result",
                ));
            }
        }
    }
}

fn drain_command_output<R: Read>(
    source: &mut R,
    line: &mut OutputLine,
    target: OutputTarget<'_>,
) -> Result<(), HostError> {
    if line.eof {
        return Ok(());
    }
    let mut bytes = [0_u8; COMMAND_OUTPUT_CHUNK];
    // Cap each drain so a continuously writing child cannot starve the
    // other pipe or the deadline check.
    for _ in 0..COMMAND_READS_PER_PUMP {
        match source.read(&mut bytes) {
            Ok(0) => {
                line.eof = true;
                line.flush(target)?;
                return Ok(());
            }
            Ok(n) => line.push(&bytes[..n], target)?,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_error("read command output", target.subject, &e)),
        }
    }
    Ok(())
}

fn signal_command_group(
    pid: rustix::process::Pid,
    signal: rustix::process::Signal,
    subject: &str,
) -> Result<(), HostError> {
    match rustix::process::kill_process_group(pid, signal) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(e) => Err(HostError::errno(
            "signal command group",
            subject,
            errno_of(e),
        )),
    }
}

fn command_exit(status: process::ExitStatus, subject: &str) -> Result<CommandExit, HostError> {
    match (status.code(), status.signal()) {
        (Some(code), _) => Ok(CommandExit::Exited(code)),
        (_, Some(signal)) => Ok(CommandExit::Signaled(signal)),
        _ => Err(HostError::invalid(
            "wait command",
            subject,
            format!("unrecognized exit status {status:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    //! Checks against the machine the tests run on (dev shell or Nix build
    //! sandbox): nothing here creates, changes or deletes files outside the
    //! test's own temp directory. Mount crossings, bind mounts, other users
    //! and a main thread that exits need the container tests (diskgc-e2e).

    use super::*;
    use diskgc_core::engine::Reason;
    use diskgc_core::host::{Cause, Ids};
    use diskgc_core::resolve::{ResolveError, Want, resolve, resolve_in};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
    use std::time::Instant;

    /// Serialises the tests that copy this process's fd table (spawning a
    /// child: `Holder::spawn`, `a_process_that_is_gone_is_gone`; any test
    /// that forks or unshares `CLONE_FILES` in this process) with the tests
    /// that judge what closing an fd releases: a `flock` lock, or a bound
    /// socket (which stays bound, and in the socket diagnostics, while any
    /// copy of its fd is open). A child holds a copy of every fd from its
    /// fork until its exec closes them (close-on-exec), so a test that
    /// closes an fd and then asserts its release must hold this guard from
    /// before the fd is opened to after the assertion. Both sides must take
    /// it: it excludes nothing for a test that does not. (The holder's own
    /// `unshare` happens in its process.)
    ///
    /// The guard does not make a release synchronous with `close`: any
    /// reader of `/proc/<pid>/fd` or `fdinfo` (an in-use scan in this or
    /// another process, `lsof`) can hold a short reference to the file
    /// (docs/safety.md, Host lock, says on which kernels), and when its
    /// `fput` is the last one, the lock or socket goes
    /// only as that reader's syscall returns (task-64 #37, pinned by
    /// `a_flock_can_outlive_its_close_while_a_proc_reader_holds_the_file_kernel_fact`).
    /// So a test asserts a release by waiting up to [`RELEASE_BOUND`],
    /// never with one attempt right after the close.
    static FD_TABLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// How long a test waits for a lock or a bound socket to go after its
    /// last fd was closed. The measured lag is one `/proc` reader's
    /// syscall (docs/safety.md, Host lock); the bound is for a reader
    /// preempted on a loaded machine, and a release that never happens
    /// still fails.
    const RELEASE_BOUND: Duration = Duration::from_secs(2);

    /// Polls `released` every millisecond until it holds or
    /// [`RELEASE_BOUND`] has passed; whether it held.
    fn released_within_bound(mut released: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + RELEASE_BOUND;
        loop {
            if released() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn fd_table_guard() -> std::sync::MutexGuard<'static, ()> {
        // A panicking test poisons the mutex; the next one may still run.
        FD_TABLE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn docker_query_collects_both_pipes_and_successful_exit() {
        let _fd_table = fd_table_guard();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("printf ready; printf warning >&2")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn query stand-in");
        let output =
            collect_docker_query_output(&mut child, "query stand-in", Duration::from_secs(2))
                .expect("collect output");
        assert_eq!(output, "ready");
    }

    #[test]
    fn docker_query_deadline_does_not_wait_for_descendant_pipe_holder() {
        const STAGE: &str = "DISKGC_DOCKER_QUERY_TEST_STAGE";
        const TEST: &str =
            "host::tests::docker_query_deadline_does_not_wait_for_descendant_pipe_holder";
        match std::env::var(STAGE).as_deref() {
            Ok("leader") => {
                let mut descendant =
                    Command::new(std::env::current_exe().expect("test executable"))
                        .args(["--exact", TEST, "--nocapture"])
                        .env(STAGE, "descendant")
                        .stdin(Stdio::null())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .spawn()
                        .expect("spawn pipe-holding descendant");
                descendant.wait().expect("wait for pipe-holding descendant");
                return;
            }
            Ok("descendant") => {
                rustix::process::setsid().expect("escape query process group");
                std::thread::sleep(Duration::from_secs(2));
                return;
            }
            _ => {}
        }
        let _fd_table = fd_table_guard();
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", TEST, "--nocapture"])
            .env(STAGE, "leader")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn query stand-in with descendant");
        let started = Instant::now();
        let result =
            collect_docker_query_output(&mut child, "query stand-in", Duration::from_millis(100));
        assert!(result.is_err(), "inherited output must hit the deadline");
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "reader waited for descendant after deadline: {:?}",
            started.elapsed()
        );
    }

    /// Each child using this helper is spawned while the fd-table guard is
    /// held. A failure to reach its expected exit cannot strand a child (or a copy
    /// of another test's flock fd) across the remaining parallel tests.
    struct GuardedChild {
        child: Child,
        reaped: bool,
    }

    impl GuardedChild {
        fn spawn(command: &mut Command, fd_table: &std::sync::MutexGuard<'_, ()>) -> Self {
            Self::try_spawn(command, fd_table).expect("spawn test child")
        }

        fn try_spawn(
            command: &mut Command,
            _fd_table: &std::sync::MutexGuard<'_, ()>,
        ) -> std::io::Result<Self> {
            Ok(Self {
                child: command.spawn()?,
                reaped: false,
            })
        }

        fn wait(&mut self) -> std::io::Result<ExitStatus> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = self.child.try_wait()? {
                    self.reaped = true;
                    return Ok(status);
                }
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("test child {} did not exit in 10 seconds", self.child.id()),
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Drop for GuardedChild {
        fn drop(&mut self) {
            if !self.reaped {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    /// A fresh directory under the system temp dir, removed on drop. Keep
    /// its basename short: tests bind Unix sockets inside it, and repo-local
    /// worktrees make `TMPDIR` long enough to reach Linux's sun_path limit.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("dg-{}-{serial}", std::process::id()));
            std::fs::create_dir(&dir).expect("create test dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn readable_open_flags_cannot_wait_for_a_fifo_or_acquire_a_terminal() {
        let flags = oflags(OpenHow {
            access: Access::Read,
            directory: false,
            nofollow: false,
            resolve: Default::default(),
        });
        assert!(flags.contains(OFlags::NONBLOCK));
        assert!(flags.contains(OFlags::NOCTTY));
    }

    #[test]
    fn config_reader_needs_no_host_and_refuses_non_regular_files() {
        let tmp = TempDir::new();
        let config = tmp.0.join("config.yaml");
        std::fs::write(&config, b"version: 1\n").expect("write");
        let link = tmp.0.join("link.yaml");
        std::os::unix::fs::symlink(&config, &link).expect("symlink");
        assert_eq!(
            read_config_file(&link, 1024).expect("read link"),
            b"version: 1\n"
        );
        assert!(
            read_config_file(&config, 2)
                .unwrap_err()
                .to_string()
                .contains("larger than")
        );
        let fifo = tmp.0.join("fifo");
        rustix::fs::mknodat(
            CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .expect("mkfifo");
        assert_eq!(
            read_config_file(&fifo, 1024).unwrap_err().cause,
            Cause::Invalid
        );
    }

    /// Set in the environment of [`Holder`] processes: the mode of
    /// [`holder_process`].
    const HOLDER_MODE: &str = "DISKGC_LINUX_TEST_HOLDER";
    const HOLDER_FILE: &str = "DISKGC_LINUX_TEST_FILE";
    const HOLDER_DIR: &str = "DISKGC_LINUX_TEST_DIR";
    /// A variable only the holder's initial environment has.
    const HOLDER_MARK: &str = "DISKGC_LINUX_TEST_MARK=holder-env";
    /// What the namespace tests need from the machine; said when they fail.
    const USERNS_NEEDED: &str = "the namespace tests need unprivileged user namespaces \
        (user.max_user_namespaces > 0, kernel.unprivileged_userns_clone = 1 where that sysctl \
        exists, no AppArmor restrict_unprivileged_userns), and, as root of such a namespace, \
        unsharing a pid and a mount namespace, mounting proc and writing kernel.ns_last_pid";

    /// The mode a `pidns-parent` helper starts as pid 1 of its namespace.
    const HOLDER_NEXT: &str = "DISKGC_LINUX_TEST_NEXT";
    /// How many times an `exec-loop` holder has exec'd (absent: none yet).
    const HOLDER_GENERATION: &str = "DISKGC_LINUX_TEST_GENERATION";
    /// The arguments that make this test binary run only [`holder_process`].
    const HOLDER_ARGS: [&str; 6] = [
        "--exact",
        "host::tests::holder_process",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
        "-q",
    ];
    /// Sizes for the "big" holder: an environment larger than the first read
    /// buffer, an fd table larger than one `getdents` buffer, and a memory
    /// map far longer than one `read` of `maps`.
    const BIG_ENV: usize = 100 * 1024;
    const BIG_FDS: usize = 4000;
    const BIG_MAPS: usize = 3000;

    /// A helper process using a file and a directory, with a fixed set of
    /// threads while it holds them. The readers are tested on it rather
    /// than on this test process, whose threads the test runner starts and
    /// ends all the time (a task list that does not settle is, correctly,
    /// `Failed`). It runs this test binary's [`holder_process`].
    struct Holder {
        child: GuardedChild,
        _stdout: BufReader<ChildStdout>,
    }

    impl Holder {
        /// Starts a holder in `mode` (`plain` or `unshare`) and waits until
        /// it holds `file` open and has `dir` as working directory.
        fn spawn(mode: &str, file: &Path, dir: &Path) -> Self {
            Self::spawn_with(mode, file, dir, &[])
        }

        /// [`Holder::spawn`] with extra environment variables.
        fn spawn_with(mode: &str, file: &Path, dir: &Path, env: &[(&str, &str)]) -> Self {
            let (name, value) = HOLDER_MARK.split_once('=').expect("NAME=value");
            let mut child = {
                // Spawning copies this process's fd table until the exec.
                let _fd_table = fd_table_guard();
                let mut command = Command::new(std::env::current_exe().expect("own executable"));
                command
                    .args(HOLDER_ARGS)
                    .env(HOLDER_MODE, mode)
                    .env(HOLDER_FILE, file)
                    .env(HOLDER_DIR, dir)
                    .env(name, value)
                    .envs(env.iter().copied())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    // Inherited, so a holder that panics says why.
                    .stderr(Stdio::inherit());
                GuardedChild::spawn(&mut command, &_fd_table)
            };
            let mut stdout = BufReader::new(child.child.stdout.take().expect("holder stdout"));
            let mut line = String::new();
            while line.trim() != "ready" {
                if stdout.buffer().is_empty() {
                    let mut poll = [rustix::event::PollFd::new(stdout.get_ref(), PollFlags::IN)];
                    let timeout = rustix::event::Timespec {
                        tv_sec: 10,
                        tv_nsec: 0,
                    };
                    let ready =
                        rustix::event::poll(&mut poll, Some(&timeout)).expect("poll holder stdout");
                    assert!(ready > 0, "the holder did not become ready in 10 seconds");
                }
                line.clear();
                let n = stdout.read_line(&mut line).expect("read holder");
                assert!(n > 0, "the holder ended before it was ready");
            }
            Self {
                child,
                _stdout: stdout,
            }
        }

        fn pid(&self) -> Pid {
            Pid(self.child.child.id())
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            drop(self.child.child.stdin.take());
            let _ = self.child.wait();
        }
    }

    /// Not a test: the body of [`Holder`] processes. Run on its own (with
    /// `--ignored`) it does nothing.
    #[test]
    #[ignore = "helper process of other tests; does nothing unless DISKGC_LINUX_TEST_HOLDER is set"]
    fn holder_process() {
        let Ok(mode) = std::env::var(HOLDER_MODE) else {
            return;
        };
        let lock_held_file = mode == "flock";
        let hold = move || {
            let file = PathBuf::from(std::env::var_os(HOLDER_FILE).expect("holder file"));
            let dir = PathBuf::from(std::env::var_os(HOLDER_DIR).expect("holder dir"));
            let held = std::fs::File::open(&file).expect("open held file");
            if lock_held_file {
                rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                    .expect("holder flock");
            }
            std::env::set_current_dir(&dir).expect("chdir");
            let mut out = std::io::stdout();
            out.write_all(b"ready\n").expect("write");
            out.flush().expect("flush");
            let mut sink = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut sink);
            drop(held);
        };
        match mode.as_str() {
            "plain" | "flock" => hold(),
            "nondumpable" => {
                rustix::process::set_dumpable_behavior(
                    rustix::process::DumpableBehavior::NotDumpable,
                )
                .expect("prctl(PR_SET_DUMPABLE, 0)");
                hold();
            }
            "big" => {
                // Many fds and many mappings, kept until the holder ends.
                let (soft, hard) = {
                    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
                    (limit.current, limit.maximum)
                };
                if soft.is_some_and(|s| s < BIG_FDS as u64 + 64) {
                    rustix::process::setrlimit(
                        rustix::process::Resource::Nofile,
                        rustix::process::Rlimit {
                            current: hard,
                            maximum: hard,
                        },
                    )
                    .expect("raise RLIMIT_NOFILE");
                }
                let fds: Vec<_> = (0..BIG_FDS)
                    .map(|_| std::fs::File::open("/dev/null").expect("open /dev/null"))
                    .collect();
                for i in 0..BIG_MAPS {
                    // Alternating protections keep neighbouring mappings apart.
                    let prot = if i % 2 == 0 {
                        rustix::mm::ProtFlags::READ
                    } else {
                        rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE
                    };
                    // SAFETY: a fresh anonymous mapping, never unmapped or used.
                    unsafe {
                        rustix::mm::mmap_anonymous(
                            std::ptr::null_mut(),
                            4096,
                            prot,
                            rustix::mm::MapFlags::PRIVATE,
                        )
                    }
                    .expect("mmap");
                }
                hold();
                drop(fds);
            }
            "unshare" => std::thread::spawn(move || {
                // SAFETY: this thread uses no fd another thread owns.
                unsafe {
                    rustix::thread::unshare_unsafe(
                        rustix::thread::UnshareFlags::FILES | rustix::thread::UnshareFlags::FS,
                    )
                }
                .expect("unshare");
                hold();
            })
            .join()
            .expect("holder thread"),
            "exec-loop" => exec_loop(),
            "pidns-parent" => {
                // Root of a new user namespace (made before this exec): a
                // new pid namespace for the children of this thread (a
                // process with threads may unshare that one), whose first
                // child is its pid 1, under the old namespace's /proc.
                // SAFETY: a pid namespace shares nothing this process uses.
                unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWPID) }
                    .unwrap_or_else(|e| panic!("unshare the pid namespace: {e}; {USERNS_NEEDED}"));
                let next = std::env::var(HOLDER_NEXT).unwrap_or_else(|_| "pidns-child".into());
                let status = Command::new(std::env::current_exe().expect("own executable"))
                    .args(HOLDER_ARGS)
                    .env(HOLDER_MODE, &next)
                    .status()
                    .expect("spawn pid 1 of the new namespace");
                assert!(status.success(), "{next}: {status}");
            }
            "pidns-remount" => pidns_remount(),
            "pidns-equal-init" => pidns_equal_init(),
            "pidns-child" => {
                let own = std::process::id();
                let proc_self = std::fs::read_link("/proc/self")
                    .map_or_else(|e| format!("({e})"), |l| l.display().to_string());
                match LinuxHost::new() {
                    Ok(_) => println!("host setup: ok as pid {own} (/proc/self: {proc_self})"),
                    Err(e) => {
                        println!("host setup refused as pid {own} (/proc/self: {proc_self}): {e}");
                    }
                }
            }
            other => panic!("unknown holder mode {other:?}"),
        }
    }

    /// The pid that [`pidns_remount`] gives its probe in two namespaces.
    const EQUAL_PID: u32 = 4242;

    /// Sets the number the next process of this process's pid namespace
    /// gets (`kernel.ns_last_pid`, kernel/pid_namespace.c
    /// `pid_ns_ctl_handler`: the writer's own namespace, with
    /// `CAP_CHECKPOINT_RESTORE` or `CAP_SYS_ADMIN` in its user namespace).
    fn set_next_pid(pid: u32) {
        std::fs::write("/proc/sys/kernel/ns_last_pid", (pid - 1).to_string())
            .unwrap_or_else(|e| panic!("write ns_last_pid: {e}; {USERNS_NEEDED}"));
    }

    /// pid 1 of a pid namespace A (in a user namespace of its own): mounts
    /// A's /proc in a mount namespace of its own, makes a child namespace B,
    /// and has B's init start a probe that is [`EQUAL_PID`] in both A and
    /// B, under A's /proc. Prints what the probe printed.
    fn pidns_remount() {
        use rustix::mount::{MountFlags, MountPropagationFlags, mount, mount_change};
        // SAFETY: a mount namespace for this thread shares nothing this
        // process relies on; the mounts below are private to it.
        unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWNS) }
            .unwrap_or_else(|e| panic!("unshare the mount namespace: {e}; {USERNS_NEEDED}"));
        mount_change(
            "/",
            MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
        )
        .unwrap_or_else(|e| panic!("make the mounts private: {e}; {USERNS_NEEDED}"));
        mount(
            PROC,
            PROC,
            "proc",
            MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
            None,
        )
        .unwrap_or_else(|e| panic!("mount the /proc of this pid namespace: {e}; {USERNS_NEEDED}"));
        // SAFETY: as in `pidns-parent`.
        unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWPID) }
            .unwrap_or_else(|e| panic!("unshare the pid namespace: {e}; {USERNS_NEEDED}"));
        let mut init = Command::new(std::env::current_exe().expect("own executable"))
            .args(HOLDER_ARGS)
            .env(HOLDER_MODE, "pidns-equal-init")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the init of the child namespace");
        let mut said = BufReader::new(init.stdout.take().expect("init stdout"));
        // The test runner prints its own lines first.
        let mut line = String::new();
        while line.trim() != "ready" {
            line.clear();
            let n = said.read_line(&mut line).expect("read init");
            assert!(
                n > 0,
                "the init of the child namespace ended before it was ready"
            );
        }
        // Nothing else starts in this namespace until the probe does.
        set_next_pid(EQUAL_PID);
        let mut go = init.stdin.take().expect("init stdin");
        go.write_all(b"go\n").expect("write init");
        drop(go);
        let mut rest = String::new();
        said.read_to_string(&mut rest).expect("read init");
        print!("{rest}");
        let status = init.wait().expect("wait init");
        assert!(status.success(), "pidns-equal-init: {status}");
    }

    /// pid 1 of namespace B: after `ready`/`go`, sets B's next pid and starts
    /// the probe (`pidns-child`).
    fn pidns_equal_init() {
        let mut out = std::io::stdout();
        out.write_all(b"ready\n").expect("write");
        out.flush().expect("flush");
        let mut go = String::new();
        std::io::stdin().read_line(&mut go).expect("read go");
        set_next_pid(EQUAL_PID);
        let status = Command::new(std::env::current_exe().expect("own executable"))
            .args(HOLDER_ARGS)
            .env(HOLDER_MODE, "pidns-child")
            .status()
            .expect("spawn the probe");
        assert!(status.success(), "pidns-child: {status}");
    }

    /// The `exec-loop` holder: a thread execs this test binary again (a
    /// non-leader exec, which swaps tids with the leader in `de_thread`),
    /// forever; the first image says `ready`. The process ends when stdin
    /// closes.
    fn exec_loop() {
        let generation: u64 = std::env::var(HOLDER_GENERATION)
            .ok()
            .map_or(0, |g| g.parse().expect("generation"));
        if generation == 0 {
            let mut out = std::io::stdout();
            out.write_all(b"ready\n").expect("write");
            out.flush().expect("flush");
        }
        std::thread::spawn(move || {
            let err = Command::new(std::env::current_exe().expect("own executable"))
                .args(std::env::args_os().skip(1))
                .env(HOLDER_GENERATION, (generation + 1).to_string())
                // Nobody reads what later images print.
                .stdout(Stdio::null())
                .exec();
            panic!("exec: {err}");
        });
        let _ = std::io::stdin().read_to_end(&mut Vec::new());
        std::process::exit(0);
    }

    fn open_dir(host: &LinuxHost, dir: &Path) -> LinuxFd {
        host.openat2(
            Base::Root,
            dir,
            OpenHow {
                access: Access::Read,
                directory: true,
                nofollow: true,
                resolve: Default::default(),
            },
        )
        .expect("open test dir")
    }

    #[test]
    fn resolves_the_root_and_own_proc_dir() {
        let host = LinuxHost::new().expect("host setup");
        let root = resolve(&host, Path::new("/"), Want::Directory).expect("resolve /");
        assert_eq!(root.stat.mount_root, Some(true));
        let proc = resolve(&host, Path::new(PROC), Want::Directory).expect("resolve /proc");
        assert_eq!(proc.mount.fs_type, "proc");
        // /proc/self is a symlink: refused.
        let err = resolve(&host, Path::new("/proc/self"), Want::Any).unwrap_err();
        assert_eq!(err.skip_reason(), Some(Reason::UnsafePath), "{err}");
    }

    #[test]
    fn root_statx_refuses_intermediate_symlinks_but_reports_a_final_symlink() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let actual = dir.0.join("actual");
        std::fs::create_dir(&actual).expect("create actual directory");
        std::fs::write(actual.join("file"), b"safe").expect("write file");
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&actual, &link).expect("create symlink");

        let stat = host.statx(Base::Root, &link).expect("stat final link");
        assert_eq!(stat.file_type, Some(FileType::Symlink));
        let dangling = dir.0.join("dangling");
        std::os::unix::fs::symlink("absent", &dangling).expect("create dangling symlink");
        assert_eq!(
            host.statx(Base::Root, &dangling)
                .expect("stat dangling final link")
                .file_type,
            Some(FileType::Symlink)
        );
        let err = host
            .statx(Base::Root, &link.join("file"))
            .expect_err("intermediate link must not be traversed");
        assert!(err.is(Errno::ELOOP), "{err}");
    }

    #[test]
    fn final_mount_point_symlink_is_unsafe_path() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let actual = dir.0.join("actual");
        std::fs::create_dir(&actual).expect("create actual directory");
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&actual, &link).expect("create symlink");

        // A stale mountinfo line names the symlink after the mount point
        // changed. Resolution must classify the refusal as unsafe_path.
        let mounts = host.mountinfo().expect("mountinfo");
        let mut fake = mounts
            .iter()
            .find(|entry| entry.mount_point == Path::new("/"))
            .expect("root mount")
            .clone();
        fake.mount_point = link.clone();
        // The real table around the stale line: the mount screen passes
        // every ancestor, so the refusal is step 2's (opening the point).
        let with = |extra: MountEntry| {
            let mut table = mounts.clone();
            table.push(extra);
            table
        };
        let err = resolve_in(&host, &with(fake), &link, Want::Directory)
            .expect_err("a symlink cannot be a mount point");
        assert!(
            matches!(err, ResolveError::UnsafePath { .. }),
            "not refused by step 2: {err}"
        );
        assert_eq!(err.skip_reason(), Some(Reason::UnsafePath), "{err}");

        let file = dir.0.join("regular");
        std::fs::write(&file, b"").expect("create regular file");
        let mut fake_file = mounts
            .iter()
            .find(|entry| entry.mount_point == Path::new("/"))
            .expect("root mount")
            .clone();
        fake_file.mount_point = file.clone();
        let err = resolve_in(&host, &with(fake_file), &file, Want::Directory)
            .expect_err("a regular file cannot be a mount point");
        assert_eq!(err.skip_reason(), Some(Reason::Missing), "{err}");
    }

    #[test]
    fn statvfs_reports_available_bytes() {
        let host = LinuxHost::new().expect("host setup");
        let root = resolve(&host, Path::new("/"), Want::Directory).expect("resolve /");
        let space = host.statvfs(&root.fd).expect("statvfs");
        assert!(space.avail_bytes <= space.size_bytes, "{space:?}");
    }

    #[test]
    fn read_dir_open_retries_only_noatime_permission_refusal() {
        let base = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let mut calls = Vec::new();
        let value = open_with_noatime(base, |flags| {
            calls.push(flags);
            if flags.contains(OFlags::NOATIME) {
                Err(rustix::io::Errno::PERM)
            } else {
                Ok(7)
            }
        });
        assert_eq!(value, Ok(7));
        assert_eq!(calls, [base | OFlags::NOATIME, base]);

        calls.clear();
        let error: rustix::io::Result<()> = open_with_noatime(base, |flags| {
            calls.push(flags);
            Err(rustix::io::Errno::NOENT)
        });
        assert_eq!(error, Err(rustix::io::Errno::NOENT));
        assert_eq!(calls, [base | OFlags::NOATIME]);
    }

    /// `listable` is `read_dir`'s open alone: a directory that can be
    /// searched but not read (mode `0333`) is refused by both (`EACCES`)
    /// while a name below it is still looked up; a readable directory
    /// passes; a non-directory is `ENOTDIR`. A `getdents` error after a
    /// good open (`EIO`) fails only the listing.
    #[test]
    fn listable_refuses_exactly_what_a_listing_refuses() {
        use std::os::unix::fs::PermissionsExt;
        let host = LinuxHost::new().expect("host setup");
        if rustix::process::geteuid().is_root() {
            eprintln!("skipped: root reads any directory");
            return;
        }
        let dir = TempDir::new();
        let locked = dir.0.join("searchable-only");
        std::fs::create_dir(&locked).expect("create dir");
        std::fs::write(locked.join("f"), b"").expect("create file");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o333)).expect("chmod");
        let locked_fd = resolve(&host, &locked, Want::Directory)
            .expect("resolve")
            .fd;
        let listing = host.read_dir(&locked_fd, 10).unwrap_err();
        let probe = host.listable(&locked_fd).unwrap_err();
        // Still searchable: the name below it is looked up.
        let found = host.statx(Base::Dir(&locked_fd), Path::new("f"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(listing.is(Errno::EACCES), "{listing}");
        assert!(probe.is(Errno::EACCES), "{probe}");
        assert!(found.is_ok(), "{found:?}");

        let open = resolve(&host, &dir.0, Want::Directory).expect("resolve").fd;
        host.listable(&open).expect("a readable directory");
        let file = resolve(&host, &locked.join("f"), Want::Any).expect("resolve file");
        let err = host.listable(&file.fd).unwrap_err();
        assert!(err.is(Errno::ENOTDIR), "{err}");
    }

    /// `read_dir` lists every name of a directory, from an `O_PATH` fd (as
    /// `resolve` returns) or a readable one, once per call (a second call
    /// is a complete listing again, not the end of the first), including a
    /// directory larger than one `getdents` buffer; a non-directory is
    /// `ENOTDIR`.
    #[test]
    fn read_dir_lists_every_name_on_each_call() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let mut want: Vec<OsString> = (0..3000)
            .map(|i| OsString::from(format!("entry with a fairly long name {i:05}")))
            .collect();
        for name in &want {
            std::fs::write(dir.0.join(name), b"").expect("create file");
        }
        std::fs::create_dir(dir.0.join("sub")).expect("create dir");
        std::os::unix::fs::symlink("nowhere", dir.0.join("link")).expect("create symlink");
        want.extend(["sub".into(), "link".into()]);
        want.sort();

        let path_fd = resolve(&host, &dir.0, Want::Directory).expect("resolve").fd;
        let read_fd = open_dir(&host, &dir.0);
        for fd in [&path_fd, &read_fd, &path_fd] {
            let DirListing::Names(mut got) = host.read_dir(fd, want.len()).expect("read_dir")
            else {
                panic!("a listing of exactly the limit is complete");
            };
            got.sort();
            assert_eq!(got.len(), want.len());
            assert!(got == want, "listing differs from the directory");
        }
        // One name over the limit stops the listing.
        assert_eq!(
            host.read_dir(&path_fd, want.len() - 1).expect("read_dir"),
            DirListing::TooMany
        );

        let empty = resolve(&host, &dir.0.join("sub"), Want::Directory).expect("resolve sub");
        assert_eq!(
            host.read_dir(&empty.fd, 0).expect("read_dir sub"),
            DirListing::Names(Vec::new())
        );
        let file = resolve(&host, &dir.0.join(&want[0]), Want::Any).expect("resolve file");
        let err = host.read_dir(&file.fd, 10).unwrap_err();
        assert!(err.is(Errno::ENOTDIR), "{err}");
    }

    /// `read_head` reads the start of the regular file an `O_PATH` fd
    /// refers to, up to the limit, also after the name was replaced: it
    /// reopens the object, not the name. A FIFO is refused without
    /// blocking, and so is a directory.
    #[test]
    fn read_head_reads_the_object_of_the_fd_and_refuses_non_files() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let tag = dir.0.join("CACHEDIR.TAG");
        std::fs::write(&tag, b"Signature: 8a477f597d28d172789f06886806bc55\nrest").expect("write");
        let fd = resolve(&host, &tag, Want::Any).expect("resolve").fd;
        assert_eq!(
            host.read_head(&fd, 43).expect("read_head"),
            b"Signature: 8a477f597d28d172789f06886806bc55"
        );
        assert_eq!(host.read_head(&fd, 1000).expect("read_head").len(), 48);
        assert_eq!(host.read_head(&fd, 0).expect("read_head"), b"");
        // The name now holds something else: the fd still reads its object.
        std::fs::rename(&tag, dir.0.join("moved")).expect("rename");
        std::fs::write(&tag, b"other").expect("write");
        assert_eq!(&host.read_head(&fd, 9).expect("read_head"), b"Signature");

        let fifo = dir.0.join("fifo");
        rustix::fs::mknodat(
            CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .expect("mkfifo");
        let fifo_fd = resolve(&host, &fifo, Want::Any).expect("resolve fifo").fd;
        let err = host.read_head(&fifo_fd, 10).unwrap_err();
        assert_eq!(err.cause, diskgc_core::host::Cause::Invalid, "{err}");
        let dir_fd = resolve(&host, &dir.0, Want::Directory)
            .expect("resolve dir")
            .fd;
        assert!(host.read_head(&dir_fd, 10).is_err());
    }

    /// A FIFO (or a device) is refused before it is opened: inotify reports
    /// every open of the object (`IN_OPEN`) but none for an `O_PATH` open,
    /// so a watch on the FIFO sees no open from `read_head`, while the same
    /// watch on a regular file sees `read_head`'s (the control: the watch
    /// works). A device node cannot be made unprivileged here; it takes the
    /// same path (its type is not regular).
    #[test]
    fn read_head_refuses_a_fifo_without_opening_it() {
        use rustix::fs::inotify;
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let fifo = dir.0.join("fifo");
        rustix::fs::mknodat(
            CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .expect("mkfifo");
        let file = dir.0.join("file");
        std::fs::write(&file, b"x").expect("write");
        let opens = |path: &Path, read: &dyn Fn(&LinuxFd)| -> usize {
            let fd = resolve(&host, path, Want::Any).expect("resolve").fd;
            let watch =
                inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC)
                    .expect("inotify_init");
            inotify::add_watch(&watch, path, inotify::WatchFlags::OPEN).expect("add_watch");
            read(&fd);
            let mut buf = [std::mem::MaybeUninit::uninit(); 4096];
            let mut reader = inotify::Reader::new(&watch, &mut buf);
            let mut n = 0;
            while reader.next().is_ok() {
                n += 1;
            }
            n
        };
        let refused = |fd: &LinuxFd| assert!(host.read_head(fd, 10).is_err());
        let read = |fd: &LinuxFd| {
            host.read_head(fd, 10).expect("read_head");
        };
        assert_eq!(opens(&fifo, &refused), 0, "read_head opened the FIFO");
        assert_eq!(opens(&file, &read), 1, "the control saw no open");
    }

    /// `reopen_regular` gives a lockable fd of the object an `O_PATH` fd
    /// refers to (flock refuses the `O_PATH` fd itself), also after the
    /// name was replaced, and refuses a FIFO without opening it (an inotify
    /// watch sees no open; the regular file is the control) and a
    /// directory.
    #[test]
    fn reopen_regular_locks_the_object_of_the_fd_and_never_opens_a_fifo() {
        use rustix::fs::inotify;
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let lock = dir.0.join(".cargo-lock");
        std::fs::write(&lock, b"").expect("write");
        let fd = resolve(&host, &lock, Want::Any).expect("resolve").fd;
        let original = rustix::fs::fstat(&fd).expect("fstat").st_ino;
        let err = match host.flock_try(
            resolve(&host, &lock, Want::Any).expect("resolve").fd,
            LockMode::Exclusive,
        ) {
            Err(e) => e,
            Ok(other) => panic!("flock on an O_PATH fd: {other:?}"),
        };
        assert!(err.is(Errno::EBADF), "{err}");
        // The name now holds another file: the reopened fd is the original.
        std::fs::rename(&lock, dir.0.join("moved")).expect("rename");
        std::fs::write(&lock, b"").expect("write");
        let readable = host.reopen_regular(&fd).expect("reopen_regular");
        assert_eq!(
            rustix::fs::fstat(&readable).expect("fstat").st_ino,
            original
        );
        match host
            .flock_try(readable, LockMode::Exclusive)
            .expect("flock")
        {
            TryLock::Acquired(_) => {}
            TryLock::Busy(_) => panic!("the reopened lock file is busy"),
        }

        let fifo = dir.0.join("fifo");
        rustix::fs::mknodat(
            CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .expect("mkfifo");
        let opens = |path: &Path, expect_ok: bool| -> usize {
            let fd = resolve(&host, path, Want::Any).expect("resolve").fd;
            let watch =
                inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC)
                    .expect("inotify_init");
            inotify::add_watch(&watch, path, inotify::WatchFlags::OPEN).expect("add_watch");
            let got = host.reopen_regular(&fd);
            assert_eq!(got.is_ok(), expect_ok, "{path:?}: {got:?}");
            if let Err(e) = got {
                assert_eq!(e.cause, diskgc_core::host::Cause::Invalid, "{e}");
            }
            let mut buf = [std::mem::MaybeUninit::uninit(); 4096];
            let mut reader = inotify::Reader::new(&watch, &mut buf);
            let mut n = 0;
            while reader.next().is_ok() {
                n += 1;
            }
            n
        };
        assert_eq!(opens(&fifo, false), 0, "reopen_regular opened the FIFO");
        assert_eq!(opens(&lock, true), 1, "the control saw no open");
        let dir_fd = resolve(&host, &dir.0, Want::Directory)
            .expect("resolve dir")
            .fd;
        assert!(host.reopen_regular(&dir_fd).is_err());
    }

    /// The wall clock is the realtime clock statx times are on.
    #[test]
    fn wall_clock_is_realtime() {
        let host = LinuxHost::new().expect("host setup");
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch");
        let got = host.wall_clock();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch");
        let got = Duration::new(u64::try_from(got.secs).expect("positive"), got.nanos);
        assert!(
            before <= got && got <= after,
            "{before:?} {got:?} {after:?}"
        );
    }

    /// A directory removed after it was opened fails its listing with
    /// `ENOENT` (getdents64 on a dead directory), as an error, never as an
    /// empty listing: the walk must fail closed there. `rustix::fs::Dir`
    /// would end the listing quietly instead (task-5 review).
    #[test]
    fn a_directory_removed_after_it_was_opened_is_an_error_not_empty() {
        let host = LinuxHost::new().expect("host setup");
        let tmp = TempDir::new();
        let doomed = tmp.0.join("doomed");
        std::fs::create_dir(&doomed).expect("mkdir");
        let readable = rustix::fs::open(
            &doomed,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open");
        let path_fd = resolve(&host, &doomed, Want::Directory)
            .expect("resolve")
            .fd;
        std::fs::remove_dir(&doomed).expect("rmdir");

        let err = for_each_name(&readable, |_| true).expect_err("a dead directory");
        assert_eq!(err, rustix::io::Errno::NOENT);
        let err = host
            .read_dir(&path_fd, 10)
            .expect_err("read_dir of a removed directory must fail");
        assert!(err.is(Errno::ENOENT), "{err}");
        // The control: rustix's Dir reads the same directory as empty.
        let again = rustix::fs::open(&tmp.0, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
            .expect("open tmp");
        std::fs::create_dir(&doomed).expect("mkdir again");
        let fd = rustix::fs::openat(
            &again,
            "doomed",
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .expect("open doomed");
        std::fs::remove_dir(&doomed).expect("rmdir again");
        let quiet: Vec<_> = rustix::fs::Dir::new(fd).expect("dir").collect();
        assert!(
            quiet.is_empty(),
            "rustix's Dir no longer hides ENOENT: {quiet:?} (the raw listing is still right)"
        );
    }

    #[test]
    fn a_process_is_read_through_its_tasks() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let file = dir.0.join("held");
        std::fs::write(&file, b"held").expect("write");
        let holder = Holder::spawn("plain", &file, &dir.0);
        let pid = holder.pid();
        assert!(host.pids().expect("pids").contains(&pid));
        let file_id = std::fs::metadata(&file).expect("stat file").ino();
        let fds = host.proc_fds(pid).expect("fds");
        assert!(
            fds.iter().any(|f| f.target.stat.ino == Some(file_id)),
            "the held file is missing: {fds:?}"
        );
        let exes = host.proc_links(pid, ProcLink::Exe).expect("exe");
        assert_eq!(exes.len(), 1, "every task shares one exe: {exes:?}");
        let maps = host.proc_maps(pid).expect("maps");
        let exe_id = exes[0].stat.dev_ino().expect("ino answered");
        assert!(
            maps.iter().any(|m| m.ino == exe_id.ino),
            "executable inode {exe_id} not among the mappings"
        );
        let dir_id = std::fs::metadata(&dir.0).expect("stat dir").ino();
        let cwds = host.proc_links(pid, ProcLink::Cwd).expect("cwd");
        assert!(cwds.iter().any(|c| c.stat.ino == Some(dir_id)), "{cwds:?}");
        let env = host.proc_environ(pid).expect("environ");
        let (name, value) = HOLDER_MARK.split_once('=').expect("NAME=value");
        assert_eq!(env.get(name), Some(value.as_ref()), "{env:?}");
    }

    /// A thread with its own fd table and working directory
    /// (`unshare(CLONE_FILES | CLONE_FS)`): its file and cwd are found
    /// although the leader's `/proc/<pid>/fd` and `cwd` do not show them.
    #[test]
    fn a_thread_with_its_own_fd_table_and_cwd_is_read() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let file = dir.0.join("held");
        std::fs::write(&file, b"held").expect("write");
        let holder = Holder::spawn("unshare", &file, &dir.0);
        let pid = holder.pid();
        let file_id = std::fs::metadata(&file).expect("stat file").ino();
        let dir_id = std::fs::metadata(&dir.0).expect("stat dir").ino();
        let leader_fds = std::fs::read_dir(format!("{PROC}/{pid}/fd"))
            .expect("leader fds")
            .filter_map(|e| std::fs::metadata(e.ok()?.path()).ok())
            .any(|m| m.ino() == file_id);
        let leader_cwd = std::fs::metadata(format!("{PROC}/{pid}/cwd")).map(|m| m.ino());
        assert!(
            !leader_fds,
            "the leader's table should not show the worker's file"
        );
        assert_ne!(
            leader_cwd.ok(),
            Some(dir_id),
            "the leader's cwd should not be the worker's"
        );
        let fds = host.proc_fds(pid).expect("fds");
        let cwds = host.proc_links(pid, ProcLink::Cwd).expect("cwds");
        assert!(
            fds.iter().any(|f| f.target.stat.ino == Some(file_id)),
            "the worker's file is missing: {fds:?}"
        );
        assert!(
            cwds.iter().any(|c| c.stat.ino == Some(dir_id)),
            "the worker's cwd is missing: {cwds:?}"
        );
        // The same through the scan's own reader, which reads the three
        // links together and compares fd tables before reading them
        // (TASK-115): the worker's table is not the leader's, so it is
        // read, and its cwd is its own.
        let view = read_proc_view(&host, pid, false).expect("view");
        assert!(
            view.fds.iter().any(|f| f.target.stat.ino == Some(file_id)),
            "the worker's file is missing from the view: {:?}",
            view.fds
        );
        assert!(
            view.cwd.iter().any(|c| c.stat.ino == Some(dir_id)),
            "the worker's cwd is missing from the view: {:?}",
            view.cwd
        );
        let [cwd, root, exe] = host.proc_links_all(pid).expect("links");
        assert_eq!(cwd, cwds);
        assert_eq!(root, host.proc_links(pid, ProcLink::Root).expect("root"));
        assert_eq!(exe, host.proc_links(pid, ProcLink::Exe).expect("exe"));
    }

    /// The combined links reader keeps what three separate ones keep: an
    /// exiting task's surviving links are results of the round, and a live
    /// task missing one makes the round untrusted (TASK-115).
    #[test]
    fn combined_links_keep_an_exiting_tasks_survivors_and_distrust_a_live_gap() {
        let target = |n: u64| LinkTarget {
            text: format!("/l{n}").into(),
            stat: Stat {
                dev: diskgc_core::host::Dev { major: 0, minor: 1 },
                file_type: None,
                perm: None,
                nlink: None,
                uid: None,
                gid: None,
                ino: Some(n),
                size: None,
                blocks: None,
                mtime: None,
                ctime: None,
                btime: None,
                mnt_id: None,
                mount_root: None,
            },
        };
        let stat = |exiting: bool| {
            let flags = if exiting { 4_194_564 } else { 4_194_560 };
            Ok(crate::tasks::parse_task_stat(
                format!("11 (x) R 1 1 1 0 -1 {flags} {} 7", ["0"; 12].join(" ")).as_bytes(),
            )
            .expect("stat"))
        };
        let run = |exiting_11: bool| {
            crate::tasks::collect(
                Process {
                    pid: 10,
                    list: || Ok(Some(Listing::new(vec![10, 11], 2, &[10, 11]))),
                    poll: || Ok(PidfdPoll::Running),
                    status: || Ok(None),
                },
                Tasks {
                    what: "set of cwd, root and exe links",
                    read: |tid| {
                        Ok(if tid == 10 {
                            [Some(target(1)), Some(target(2)), Some(target(3))]
                        } else {
                            [Some(target(4)), Some(target(5)), None]
                        })
                    },
                    is_empty: links_incomplete,
                    stat: |tid| stat(tid == 11 && exiting_11),
                    shares: never_shared,
                },
                crate::tasks::Want::EveryTask,
            )
        };
        let kept = run(true).expect("an exiting task's gap is no shortfall");
        assert!(
            kept.iter()
                .any(|links| links[0].as_ref().is_some_and(|t| t.stat.ino == Some(4))),
            "the exiting task's cwd was dropped: {kept:?}"
        );
        let refused = run(false);
        assert!(
            matches!(&refused, Err(ProcError::Failed(e)) if e.detail.contains("task 11 is live but gave an empty or incomplete set of cwd, root and exe links")),
            "{refused:?}"
        );
    }

    /// A held `stat` file read again at offset 0 shows the task as it is
    /// now, not as it was at the first read: every "live after" of a
    /// bracket rests on it (TASK-115: [`TaskStats`] preads the held file
    /// instead of opening it again).
    #[test]
    fn a_held_task_stat_read_again_is_fresh() {
        let host = LinuxHost::new().expect("host setup");
        let pid = Pid(std::process::id());
        let (tid_sender, tid_receiver) = mpsc::channel();
        let (go_sender, go_receiver) = mpsc::channel::<()>();
        let (done_sender, done_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            // SAFETY: gettid takes no pointer arguments and changes no state.
            let tid = unsafe { libc::syscall(libc::SYS_gettid) };
            tid_sender
                .send(u32::try_from(tid).expect("positive tid"))
                .expect("send tid");
            go_receiver.recv().expect("go");
            rustix::thread::set_name(c"t115-renamed").expect("rename thread");
            done_sender.send(()).expect("done");
            go_receiver.recv().expect("release");
        });
        let tid = tid_receiver.recv().expect("worker tid");
        let mut stats = TaskStats::new(host.root(), pid);
        let comm = |stats: &mut TaskStats<'_>| {
            let _ = stats.read(tid).expect("stat");
            let pin = stats.pins.get(&tid).expect("held");
            let mut text = [0; TASK_STAT_MAX];
            let n = rustix::io::pread(pin, &mut text, 0).expect("pread");
            String::from_utf8_lossy(&text[..n]).into_owned()
        };
        let before = comm(&mut stats);
        go_sender.send(()).expect("rename now");
        done_receiver.recv().expect("renamed");
        let after = comm(&mut stats);
        go_sender.send(()).expect("release");
        worker.join().expect("join");
        assert!(!before.contains("t115-renamed"), "{before}");
        assert!(
            after.contains("(t115-renamed)"),
            "a held stat read again was stale: {after}"
        );
    }

    /// The kernel fact the fd-table comparison relies on (TASK-115):
    /// `kcmp(KCMP_FILES)` says two threads of a process share their table,
    /// and says they do not once one of them has its own
    /// (`unshare(CLONE_FILES)`). Without the first the scan only loses
    /// time; the second is what keeps a private table from being skipped.
    #[test]
    fn kcmp_tells_a_shared_fd_table_from_a_private_one() {
        let gettid = || {
            // SAFETY: gettid takes no pointer arguments and changes no state.
            u32::try_from(unsafe { libc::syscall(libc::SYS_gettid) }).expect("positive tid")
        };
        let own = gettid();
        let (tid_sender, tid_receiver) = mpsc::channel();
        let (go_sender, go_receiver) = mpsc::channel::<()>();
        let (done_sender, done_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            tid_sender.send(gettid()).expect("send tid");
            go_receiver.recv().expect("go");
            // SAFETY: this thread uses no fd another thread owns, and it
            // exits right after the comparison.
            unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::FILES) }
                .expect("unshare");
            done_sender.send(()).expect("send done");
            go_receiver.recv().expect("release");
        });
        let tid = tid_receiver.recv().expect("worker tid");
        let shared = same_fd_table(own, tid).expect("kcmp permitted here");
        go_sender.send(()).expect("unshare now");
        done_receiver.recv().expect("unshared");
        let private = same_fd_table(own, tid).expect("kcmp permitted here");
        go_sender.send(()).expect("release worker");
        worker.join().expect("join worker");
        assert!(
            shared,
            "threads sharing a table are not reported as sharing"
        );
        assert!(!private, "a thread's own table is reported as shared");
    }

    /// Owner PID's namespace facts agree with what the process sees of
    /// itself without the held `/proc`: the inode of its own
    /// `/proc/self/ns/pid`. Whether this runs in the initial namespace
    /// depends on where the tests run (a Nix build sandbox has its own), so
    /// only the agreement is asserted.
    #[test]
    fn pid_namespace_agrees_with_the_namespace_inode() {
        let host = LinuxHost::new().expect("host setup");
        let facts = host.pid_namespace().expect("pid namespace facts");
        let inode = std::fs::metadata("/proc/self/ns/pid")
            .expect("stat own pid namespace")
            .ino();
        assert_eq!(facts.initial, inode == PROC_PID_INIT_INO, "{facts:?}");
    }

    /// ESRCH is the absence of a path only below a `/proc` root; elsewhere
    /// it carries no errno, and every other errno is kept.
    #[test]
    fn root_lookup_esrch_counts_as_absence_only_below_a_proc_root() {
        let shown = || "x".to_owned();
        let errno = |e: ProcError| match e {
            ProcError::Failed(e) => e.kernel_errno(),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            errno(root_lookup_error(Errno::ESRCH, || true, shown())),
            Some(Errno::ESRCH)
        );
        assert_eq!(
            errno(root_lookup_error(Errno::ESRCH, || false, shown())),
            None
        );
        assert_eq!(
            errno(root_lookup_error(
                Errno::ENOENT,
                || panic!("not asked"),
                shown()
            )),
            Some(Errno::ENOENT)
        );
    }

    /// TASK-185: diskgc out of file descriptors is never folded into an
    /// unreadable `uid_map`, an unopened root or an unresolvable lookup:
    /// each keeps it apart as `Exhausted`, an error of the run; every other
    /// failure keeps its old meaning.
    #[test]
    fn out_of_descriptors_is_kept_apart_in_every_errno_erasing_reader() {
        let e = |errno| HostError::errno("read", "/proc/9/uid_map", errno);
        for exhausted in [Errno::EMFILE, Errno::ENFILE] {
            assert!(
                matches!(uid_map_of(Err(e(exhausted))), Err(ProcError::Exhausted(x)) if x.is(exhausted)),
                "uid_map {exhausted:?}"
            );
            assert!(
                matches!(root_unopened(e(exhausted), "9/root"), ProcError::Exhausted(x) if x.is(exhausted)),
                "root {exhausted:?}"
            );
            for op in ["statx", "openat"] {
                assert!(
                    matches!(errno_erased(exhausted, op, "x".to_owned(), "w"), ProcError::Exhausted(x) if x.is(exhausted)),
                    "{op} {exhausted:?}"
                );
            }
            assert!(matches!(
                root_lookup_error(exhausted, || panic!("not asked"), "x".to_owned()),
                ProcError::Exhausted(_)
            ));
        }
        assert_eq!(uid_map_of(Err(e(Errno::ENOENT))), Ok(UidMap::Absent));
        assert!(matches!(
            uid_map_of(Err(e(Errno::EACCES))),
            Ok(UidMap::Unreadable(_))
        ));
        assert_eq!(
            uid_map_of(Ok(b"0 0 4294967295\n".to_vec())),
            Ok(UidMap::Map("0 0 4294967295\n".to_owned()))
        );
        // Another failure to open the root, or after the object was opened,
        // still carries no errno: unresolvable, never absent.
        for unresolvable in [
            root_unopened(e(Errno::ENOENT), "9/root"),
            errno_erased(Errno::ENOENT, "statx", "x".to_owned(), "w"),
        ] {
            assert!(
                matches!(&unresolvable, ProcError::Failed(x) if x.kernel_errno().is_none()),
                "{unresolvable:?}"
            );
        }
    }

    /// The pytest contract's readers on our own process (read-only, the
    /// host's real `/proc`): our uids and command line; a value resolved
    /// in our root is the object a plain `stat` names, `..` clamped at the
    /// root, the climb ending at the root's fixpoint; a relative value and
    /// a missing path are refused, a reaped child is `Gone`.
    #[test]
    fn pytest_readers_follow_the_process() {
        let host = LinuxHost::new().expect("host setup");
        let own = host.own_pid();
        let uids = host.proc_uids(own).expect("own uids");
        assert_eq!(uids.real, rustix::process::getuid().as_raw());
        let args = host.proc_cmdline(own).expect("own command line");
        let expected: Vec<OsString> = std::env::args_os().collect();
        assert_eq!(args, expected);
        let ino = |path: &str| std::fs::metadata(path).expect("stat").ino();
        let tmp = std::env::temp_dir();
        let tmp = tmp.to_str().expect("UTF-8 temp dir");
        let chain = host
            .proc_root_lookup(own, Path::new(&format!("/../..{tmp}")), true)
            .expect("temp dir in our root");
        assert_eq!(chain.first().expect("the object first").ino, ino(tmp));
        let root = chain.last().expect("a climb ends at the root");
        assert_eq!(root.ino, ino("/"), "{chain:?}");
        assert_eq!(
            host.proc_root_lookup(own, Path::new("/"), true)
                .expect("the root")
                .len(),
            1,
            "the root is its own fixpoint"
        );
        assert!(matches!(
            host.proc_root_lookup(own, Path::new("relative"), false),
            Err(ProcError::Failed(_))
        ));
        assert!(matches!(
            host.proc_root_lookup(own, Path::new("/nonexistent-diskgc-test-path"), false),
            Err(ProcError::Failed(_))
        ));
        let mut child = process::Command::new("true").spawn().expect("spawn true");
        let pid = Pid(child.id());
        child.wait().expect("reap");
        if !host.pid_alive(pid).expect("probe a reaped pid") {
            assert_eq!(host.proc_cmdline(pid), Err(ProcError::Gone));
            assert_eq!(host.proc_uids(pid), Err(ProcError::Gone));
            assert_eq!(
                host.proc_root_lookup(pid, Path::new("/"), false),
                Err(ProcError::Gone)
            );
        }
    }

    /// Our own NSpid starts with our pid; a reaped child's pid is no
    /// longer alive and its status is `Gone`.
    #[test]
    fn nspid_and_pid_alive_follow_the_process() {
        let host = LinuxHost::new().expect("host setup");
        let own = host.own_pid();
        let levels = host.proc_nspid(own).expect("own NSpid");
        assert_eq!(levels.first(), Some(&own.0), "{levels:?}");
        assert!(host.pid_alive(own).expect("own pidfd"));
        let mut child = process::Command::new("true").spawn().expect("spawn true");
        let pid = Pid(child.id());
        child.wait().expect("reap");
        // The number may be reused at once by another process: only a
        // `false` answer or a live reuse are legitimate, never an error.
        if !host.pid_alive(pid).expect("probe a reaped pid") {
            assert_eq!(host.proc_nspid(pid), Err(ProcError::Gone));
        }
    }

    /// No value outside 32 bits is folded onto a magic on the local list.
    #[test]
    fn an_f_type_outside_32_bits_is_no_magic() {
        assert_eq!(fs_magic_of(0x0102_1994), Some(0x0102_1994));
        assert_eq!(fs_magic_of(0xF2F5_2010), Some(0xF2F5_2010));
        // A 32-bit ABI's signed word for the same f2fs magic.
        assert_eq!(fs_magic_of(-0x0D0A_DFF0), Some(0xF2F5_2010));
        assert_eq!(fs_magic_of(0x1_0102_1994), None);
        assert_eq!(fs_magic_of(-0x1_0000_0000), None);
    }

    /// A child that exited and is not reaped is a zombie: its pid is still
    /// taken (the probe says alive) and its `NSpid` is readable, as the
    /// model says (`processes[].zombie`).
    #[test]
    fn a_real_zombie_keeps_its_pid_and_its_nspid() {
        let host = LinuxHost::new().expect("host setup");
        let mut child = process::Command::new("true").spawn().expect("spawn true");
        let pid = Pid(child.id());
        let deadline = Instant::now() + Duration::from_secs(5);
        let state = loop {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("status");
            let state = status
                .lines()
                .find_map(|line| line.strip_prefix("State:"))
                .map(str::trim)
                .unwrap_or_default()
                .to_owned();
            if state.starts_with('Z') || Instant::now() >= deadline {
                break state;
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(
            state.starts_with('Z'),
            "child did not become a zombie: {state}"
        );
        assert!(host.pid_alive(pid).expect("probe a zombie"));
        assert_eq!(
            host.proc_nspid(pid).expect("zombie NSpid").first(),
            Some(&pid.0)
        );
        child.wait().expect("reap");
    }

    #[test]
    fn fs_magic_reports_the_filesystem_type() {
        let host = LinuxHost::new().expect("host setup");
        let proc = resolve(&host, Path::new("/proc"), Want::Directory).expect("resolve /proc");
        assert_eq!(
            host.fs_magic(&proc.fd).expect("fstatfs"),
            u64::from(PROC_SUPER_MAGIC as u32)
        );
    }

    /// `proc_status` shows the ids of a process and whether it is dumpable
    /// (the owner of its `fd` directory): the effective uid while it is,
    /// root (or the overflow uid) once it is not, when its fds are `Denied`
    /// to the same user. A
    /// process that has exited and was reaped is `Gone`.
    #[test]
    fn proc_status_shows_ids_and_dumpability() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let file = dir.0.join("held");
        std::fs::write(&file, b"held").expect("write");
        let uid = rustix::process::getuid().as_raw();
        let euid = rustix::process::geteuid().as_raw();

        let plain = Holder::spawn("plain", &file, &dir.0);
        let status = host.proc_status(plain.pid()).expect("status");
        assert_eq!(status.uid.real, uid, "{status:?}");
        assert_eq!(status.uid.effective, euid, "{status:?}");
        assert_eq!(status.fd_dir_owner, Some(euid), "dumpable: {status:?}");
        assert!(!status.name.is_empty(), "{status:?}");
        assert!(host.proc_fds(plain.pid()).is_ok());

        let hidden = Holder::spawn("nondumpable", &file, &dir.0);
        let status = host.proc_status(hidden.pid()).expect("status");
        assert_eq!(status.uid, Ids::same(uid), "{status:?}");
        assert_eq!(status.uid.effective, euid, "{status:?}");
        if euid != 0 {
            // Root, or the overflow uid where root is not mapped (the Nix
            // build sandbox): not the effective uid, either way.
            assert!(
                status.fd_dir_owner.is_some_and(|o| o != euid),
                "non-dumpable: {status:?}"
            );
            assert!(
                matches!(host.proc_fds(hidden.pid()), Err(ProcError::Denied(_))),
                "a non-dumpable process's fds are denied to its own user"
            );
        }

        let gone = {
            let _fd_table = fd_table_guard();
            let mut command = Command::new("/proc/self/exe");
            command.arg("--help").stdout(Stdio::null());
            let mut child = GuardedChild::spawn(&mut command, &_fd_table);
            let pid = Pid(child.child.id());
            child.wait().expect("wait");
            pid
        };
        assert!(
            matches!(host.proc_status(gone), Err(ProcError::Gone)),
            "a reaped process is gone"
        );
    }

    /// The socket diagnostics report a socket bound to a file by the file's
    /// (device, inode), and no longer once the socket is closed (the file
    /// stays: a stale socket file).
    #[test]
    fn bound_unix_sockets_reports_a_bound_socket_file_until_it_closes() {
        // A child of another test, forked while the listener is open and
        // not yet exec'd, keeps a copy of its fd: the socket would stay
        // bound past `drop` (see `fd_table_guard`).
        let _fd_table = fd_table_guard();
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let path = dir.0.join("live.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind a socket");
        let meta = std::fs::symlink_metadata(&path).expect("stat the socket file");
        let id = diskgc_core::host::DevIno {
            dev: diskgc_core::host::Dev {
                major: rustix::fs::major(meta.dev()),
                minor: rustix::fs::minor(meta.dev()),
            },
            ino: meta.ino(),
        };
        let bound = host.bound_unix_sockets().expect("sock_diag dump");
        assert!(
            bound.iter().any(|s| s.is(id)),
            "{id} is not among {} bound sockets",
            bound.len()
        );
        drop(listener);
        assert!(path.exists(), "closing a socket leaves its file");
        // A `/proc` reader's reference can keep the socket bound briefly
        // after `drop` (see `fd_table_guard`).
        assert!(
            released_within_bound(|| {
                !host
                    .bound_unix_sockets()
                    .expect("sock_diag dump")
                    .iter()
                    .any(|s| s.is(id))
            }),
            "{id} is still reported {RELEASE_BOUND:?} after the socket closed"
        );
    }

    /// The in-use check on this machine, read-only: a candidate directory
    /// holding a file a helper process keeps open is `in_use`, by that
    /// process's pid; the same directory's sibling is clear. Prints what the
    /// scan of the live `/proc` found and how long it took (the freshness
    /// bound needs it well under a second).
    #[test]
    fn the_in_use_check_finds_a_holder_on_this_machine() {
        use diskgc_core::engine::{Candidate, Reason, Rules, walk};
        use diskgc_core::inuse::{Options, scan};
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let used = dir.0.join("used");
        let free = dir.0.join("free");
        std::fs::create_dir(&used).expect("mkdir");
        std::fs::create_dir(&free).expect("mkdir");
        let file = used.join("held");
        std::fs::write(&file, b"held").expect("write");
        std::fs::write(free.join("f"), b"f").expect("write");
        let holder = Holder::spawn("plain", &file, &dir.0);

        let uid = rustix::process::geteuid().as_raw();
        let parent = resolve(&host, &dir.0, Want::Directory).expect("resolve");
        let owners = [uid];
        let rules = Rules::new(&owners);
        let scan = scan(
            &host,
            &Options {
                users: &[uid],
                sockets: true,
                pid_lookup: true,
                pytest_facts: false,
            },
            None,
        )
        .expect("scan");
        // Owner PID's table over the real process list: the holder (a
        // live child of this test) is carried at its own number, and
        // `status` is readable for every listed process without hidepid.
        let table = scan.pid_table().expect("asked for");
        eprintln!(
            "pid table: namespace {:?}, {} unreadable",
            table.namespace(),
            table.unreadable().len()
        );
        if table.preconditions_hold() {
            assert_eq!(table.carrier(holder.pid().0), Some(holder.pid()));
        } else {
            // Not the initial PID namespace (the Nix build sandbox, a
            // container) or hidepid: Owner PID reads no `NSpid`, so the
            // table must stay empty (TASK-133). The facts themselves are
            // checked against the namespace inode in
            // `pid_namespace_agrees_with_the_namespace_inode`.
            eprintln!(
                "skipping the carrier check: Owner PID's preconditions fail here ({:?})",
                table.namespace()
            );
            assert_eq!(table.carrier(holder.pid().0), None, "a table built anyway");
        }
        if table.namespace().is_ok_and(|facts| !facts.hidepid) {
            assert_eq!(
                table.unreadable(),
                [],
                "status is world-readable without hidepid"
            );
        }
        eprintln!(
            "live scan: {} processes read, {} gone, {} unreadable ({} blocking), {} listings, \
             took {:?}",
            scan.processes(),
            scan.gone(),
            scan.unreadable().len(),
            scan.unreadable()
                .iter()
                .filter(|u| u.excuse.is_none())
                .count(),
            scan.listings(),
            scan.took()
        );
        for note in scan.notes() {
            eprintln!("  note: {note}");
        }
        let used_candidate = Candidate::new(&parent, &used).expect("a name");
        let record = walk(&host, &used_candidate, &rules, None).expect("walk");
        let skip = scan
            .check(&used_candidate, &record)
            .expect_err("a held file makes its candidate in use");
        let free_candidate = Candidate::new(&parent, &free).expect("a name");
        let free_record = walk(&host, &free_candidate, &rules, None).expect("walk");
        if let Some(why) = scan.blocked() {
            // Host process churn can exhaust the bounded listing (TASK-239).
            // That correctly refuses every candidate as in_use_unknown,
            // without holder-specific attribution; the model test
            // `only_listings_with_new_pids_count_against_the_bound` pins this
            // branch deterministically.
            assert_eq!(skip.reason, Reason::InUseUnknown, "{skip}");
            assert_eq!(skip.path, used, "{skip}");
            assert_eq!(skip.detail, why, "{skip}");
            let free_skip = scan
                .check(&free_candidate, &free_record)
                .expect_err("a blocked scan refuses every candidate");
            assert_eq!(free_skip.reason, Reason::InUseUnknown, "{free_skip}");
        } else {
            assert_eq!(skip.reason, Reason::InUse, "{skip}");
            assert!(
                skip.detail.starts_with(&format!("pid {} ", holder.pid())),
                "{skip}"
            );
            assert_eq!(skip.path, file, "{skip}");
            assert_eq!(scan.check(&free_candidate, &free_record), Ok(()));
        }
    }

    #[test]
    fn a_process_that_is_gone_is_gone() {
        let _fd_table = fd_table_guard();
        let host = LinuxHost::new().expect("host setup");
        let mut command = Command::new("/proc/self/exe");
        command
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = GuardedChild::spawn(&mut command, &_fd_table);
        let pid = Pid(child.child.id());
        child.wait().expect("wait");
        assert_eq!(host.proc_maps(pid), Err(ProcError::Gone));
        assert_eq!(host.proc_fds(pid), Err(ProcError::Gone));
        assert_eq!(host.proc_links(pid, ProcLink::Cwd), Err(ProcError::Gone));
        assert_eq!(host.proc_environ(pid), Err(ProcError::Gone));
    }

    /// A zombie child, not yet reaped: `pidfd_poll` says readable and its
    /// `status` shows it alone in its group, so it is Gone. (Pins the
    /// `POLLIN` branch: answering "running" there makes this `Failed`.) Once
    /// reaped, a held pidfd says released on every supported kernel.
    #[test]
    fn an_unreaped_zombie_is_gone() {
        let host = LinuxHost::new().expect("host setup");
        let mut child = {
            let _fd_table = fd_table_guard();
            let mut command = Command::new("/proc/self/exe");
            command
                .arg("--help")
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            GuardedChild::spawn(&mut command, &_fd_table)
        };
        let pid = Pid(child.child.id());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = group_status(host.root(), pid).expect("status of an unreaped child");
            if status.is_some_and(|s| s.state == 'Z' && s.threads == 1) {
                break;
            }
            assert!(Instant::now() < deadline, "never a lone zombie: {status:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
        let pidfd = open_pidfd(pid).expect("a zombie has a pidfd");
        assert_eq!(
            poll_pidfd(pid, &pidfd, Duration::ZERO),
            Ok(PidfdPoll::Readable)
        );
        assert_eq!(host.proc_fds(pid), Err(ProcError::Gone));
        assert_eq!(host.proc_links(pid, ProcLink::Cwd), Err(ProcError::Gone));
        assert_eq!(host.proc_maps(pid), Err(ProcError::Gone));
        assert_eq!(host.proc_environ(pid), Err(ProcError::Gone));
        child.wait().expect("reap");
        assert_eq!(pidfd_released(pid, &pidfd), Ok(true));
        assert_eq!(
            poll_pidfd(pid, &pidfd, Duration::ZERO),
            Ok(PidfdPoll::Hangup)
        );
    }

    /// Round 9 regression on the real kernel: a process whose threads exec
    /// in a loop (each exec swaps tids with the leader, the window in which
    /// `pidfd_poll` can say readable while the process lives) is read over
    /// and over for several seconds, and is never Gone.
    #[test]
    fn a_process_execing_from_a_thread_in_a_loop_is_never_gone() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let holder = Holder::spawn("exec-loop", &dir.0, &dir.0);
        let pid = holder.pid();
        let first = exec_generation(&host, pid);
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut answers: std::collections::BTreeMap<&str, usize> = Default::default();
        let mut tally = |what: &str, got: Result<(), ProcError>| {
            assert_ne!(got, Err(ProcError::Gone), "{what}: a live process is gone");
            let kind = match got {
                Ok(()) => "ok",
                Err(ProcError::Denied(_)) => "denied",
                Err(_) => "failed",
            };
            *answers.entry(kind).or_default() += 1;
        };
        while Instant::now() < deadline {
            tally("fds", host.proc_fds(pid).map(drop));
            tally("cwd", host.proc_links(pid, ProcLink::Cwd).map(drop));
            tally("maps", host.proc_maps(pid).map(drop));
            tally("environ", host.proc_environ(pid).map(drop));
        }
        let last = exec_generation(&host, pid);
        eprintln!("exec loop: generations {first}..{last}, answers {answers:?}");
        // Thousands of execs on an idle machine; a margin for heavy load.
        assert!(
            last >= first + 20,
            "the holder did not keep exec'ing: {first}..{last}"
        );
    }

    /// How many times an `exec-loop` holder has exec'd, from its current
    /// environment, read with the product's reader (one `read` call from a
    /// live task: a plain multi-call read can mix two images' memory across
    /// an exec). Retried while an exec makes the read fail, for up to 10 s.
    fn exec_generation(host: &LinuxHost, pid: Pid) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match host.proc_environ(pid) {
                Ok(env) => {
                    return env.get(HOLDER_GENERATION).map_or(0, |g| {
                        g.to_str().and_then(|g| g.parse().ok()).expect("generation")
                    });
                }
                Err(e) => assert!(
                    Instant::now() < deadline,
                    "no environment of {pid} to read: {e}"
                ),
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn pidfd_poll_events_are_classified() {
        let pid = Pid(10);
        let answer = |revents| pidfd_poll_answer(pid, revents);
        assert_eq!(answer(PollFlags::empty()), Ok(PidfdPoll::Running));
        assert_eq!(
            answer(PollFlags::IN | PollFlags::RDNORM),
            Ok(PidfdPoll::Readable)
        );
        assert_eq!(
            answer(PollFlags::IN | PollFlags::RDNORM | PollFlags::HUP),
            Ok(PidfdPoll::Hangup)
        );
        for odd in [PollFlags::NVAL, PollFlags::ERR, PollFlags::OUT] {
            assert!(matches!(answer(odd), Err(ProcError::Failed(_))), "{odd:?}");
        }
    }

    /// QA (round 9): the task listing is the tasks, the count, and the
    /// tasks again, and a task missing from the second listing leaves the
    /// listing unconfirmed.
    #[test]
    fn a_task_listing_is_confirmed_by_listing_again_after_the_count() {
        let calls = std::cell::RefCell::new(Vec::new());
        let mut listings = vec![vec![10, 11], vec![10]].into_iter();
        let got = task_listing(
            || {
                calls.borrow_mut().push("list");
                Ok(listings.next())
            },
            || {
                calls.borrow_mut().push("count");
                Ok(Some(2))
            },
            |_| Ok(Some(())),
            |_, _| Ok(true),
        );
        assert_eq!(got, Ok(Some(Listing::new(vec![10, 11], 2, &[10]))));
        assert_ne!(got, Ok(Some(Listing::new(vec![10, 11], 2, &[10, 11]))));
        assert_eq!(*calls.borrow(), ["list", "count", "list"]);
        // A step that vanished: no listing, and no step after it.
        let got = task_listing(
            || Ok(Some(vec![10])),
            || Ok(None),
            |_| Ok(Some(())),
            |_, _| Ok(true),
        );
        assert_eq!(got, Ok(None));
        let mut listings = vec![Some(vec![10]), None].into_iter();
        let got = task_listing(
            || Ok(listings.next().flatten()),
            || Ok(Some(1)),
            |_| Ok(Some(())),
            |_, _| Ok(true),
        );
        assert_eq!(got, Ok(None));
    }

    /// Two listings can have identical numbers while a different task was
    /// present at the count. Identity confirmation must reject that round.
    #[test]
    fn task_listing_rejects_a_same_number_different_task_aba() {
        let mut listings = vec![vec![10, 11], vec![10, 11]].into_iter();
        let got = task_listing(
            || Ok(listings.next()),
            || Ok(Some(2)),
            |tid| Ok(Some(tid)),
            |tid, pinned| Ok(tid == 10 && *pinned == 10),
        )
        .expect("listing read")
        .expect("listing exists");
        assert_ne!(got, Listing::new(vec![10, 11], 2, &[10, 11]));
    }

    /// The production procfs pin/compare closures reject a reused numeric
    /// tid even when both directory listings and the thread count agree.
    /// The fake tree makes the replacement exactly at the count boundary.
    #[test]
    fn anchored_task_listing_rejects_reused_numeric_tid() {
        let dir = TempDir::new();
        fake_proc(&dir.0, &std::process::id().to_string());
        let root = open_proc_root(&dir.0);
        let proc = ProcRoot(root.as_fd());
        let pid = Pid(4242);
        let task_dir = dir.0.join("4242/task");
        let listing = anchored_task_listing_with_count(proc, pid, || {
            std::fs::rename(task_dir.join("4243"), dir.0.join("old-task"))
                .expect("move old task after its directory was pinned");
            std::fs::create_dir(task_dir.join("4243")).expect("reuse numeric tid");
            Ok(Some(2))
        })
        .expect("listing read")
        .expect("listing exists");
        assert_ne!(
            listing,
            Listing::new(vec![4242, 4243], 2, &[4242, 4243]),
            "identical numeric listings must not validate a different task inode"
        );
    }

    fn errno(e: Errno) -> HostError {
        HostError::errno("read", SELF_STATUS, e)
    }

    /// B2 (round 9): `/proc` of another pid namespace, or none, and a
    /// blocked `pidfd_open` are refused when the host is set up.
    #[test]
    fn the_host_refuses_what_its_readers_cannot_stand_on() {
        let status = |nspid: &str| -> Result<Vec<u8>, HostError> {
            Ok(format!("Name:\tx\nTgid:\t4242\n{nspid}Threads:\t1\n").into_bytes())
        };
        assert_eq!(check_proc_namespace(4242, status("NSpid:\t4242\n")), Ok(()));
        let refused = |e: HostError| e.to_string();
        // N1 (round 10): /proc of an ancestor namespace, with this process
        // at the same number in both namespaces.
        let e = refused(check_proc_namespace(4242, status("NSpid:\t4242\t4242\n")).unwrap_err());
        assert!(e.contains("NSpid line is \"4242 4242\""), "{e}");
        assert!(e.contains("another pid namespace"), "{e}");
        let e = refused(check_proc_namespace(1, status("NSpid:\t855089\t1\n")).unwrap_err());
        assert!(e.contains("another pid namespace"), "{e}");
        let e = refused(check_proc_namespace(4242, status("NSpid:\t17\n")).unwrap_err());
        assert!(e.contains("this process is pid 4242"), "{e}");
        let e = refused(check_proc_namespace(4242, status("")).unwrap_err());
        assert!(e.contains("has no NSpid line"), "{e}");
        let e = refused(check_proc_namespace(42, Err(errno(Errno::ENOENT))).unwrap_err());
        assert!(e.contains("not visible in the mounted /proc"), "{e}");
        let e = check_proc_namespace(42, Err(errno(Errno::EACCES))).unwrap_err();
        assert!(e.is(Errno::EACCES), "{e}");
        assert_eq!(check_pidfd_open(Ok(())), Ok(()));
        for blocked in [Errno::ENOSYS, Errno::EPERM] {
            let e = refused(check_pidfd_open(Err(blocked)).unwrap_err());
            assert!(e.contains("pidfd_open is unavailable"), "{e}");
        }
        LinuxHost::new().expect("this test's own namespace and kernel");
    }

    /// Runs `pidns-parent` as root of a new user namespace (like `unshare
    /// -Ur`; no privilege needed), starting `next` as pid 1 of a new pid
    /// namespace, and returns what was printed.
    fn in_new_pid_namespace(next: &str) -> String {
        let mut parent = Command::new(std::env::current_exe().expect("own executable"));
        parent
            .args(HOLDER_ARGS)
            .env(HOLDER_MODE, "pidns-parent")
            .env(HOLDER_NEXT, next)
            .stderr(Stdio::inherit());
        // Made before the fork: the closure below must not allocate.
        let uid_map = format!("0 {} 1", rustix::process::getuid().as_raw()).into_bytes();
        // SAFETY: between fork and exec the closure makes only syscalls
        // (`unshare`, `open`, `write`) on memory made before the fork.
        unsafe {
            parent.pre_exec(move || {
                // A new user namespace, with this uid as its root, so the
                // exec'd helper keeps the namespace's capabilities.
                rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWUSER)?;
                let map = rustix::fs::open(c"/proc/self/uid_map", OFlags::WRONLY, Mode::empty())?;
                rustix::io::write(&map, &uid_map)?;
                Ok(())
            });
        }
        // Its stdout goes to a file, not a pipe: the wait below is bounded
        // (GuardedChild, task-64 H10) and no descendant holding a pipe can
        // stretch it.
        let dir = TempDir::new();
        let stdout_path = dir.0.join("said");
        parent.stdout(std::fs::File::create(&stdout_path).expect("helper stdout file"));
        // The user namespace is made in `pre_exec`, so a missing one fails
        // the spawn (`unshare` gives EPERM in a sandbox).
        let mut child = {
            let _fd_table = fd_table_guard();
            GuardedChild::try_spawn(&mut parent, &_fd_table).unwrap_or_else(|e| {
                panic!("start a process in a new user namespace: {e}; {USERNS_NEEDED}")
            })
        };
        let status = child
            .wait()
            .unwrap_or_else(|e| panic!("wait for the namespace helper: {e}"));
        let said = std::fs::read_to_string(&stdout_path).expect("read helper stdout");
        assert!(
            status.success(),
            "the namespace helper failed ({status}; its own panic message is above); \
             {USERNS_NEEDED}. It printed: {said}"
        );
        said
    }

    /// B2 on the real kernel: pid 1 of a new pid namespace still sees the
    /// old namespace's `/proc`, where it has another number. Setting up the
    /// host there is refused.
    #[test]
    fn the_host_is_refused_under_another_namespaces_proc() {
        let said = in_new_pid_namespace("pidns-child");
        assert!(
            said.contains("host setup refused as pid 1 ")
                && said.contains("but this process is pid 1 in its own namespace"),
            "{said}"
        );
    }

    /// N1 (round 10) on the real kernel: the probe is pid 4242 both in its
    /// own namespace B and in the parent namespace A whose `/proc` it sees,
    /// so `/proc/self` names `getpid()`; its `NSpid` still has two fields,
    /// and setting up the host is refused.
    #[test]
    fn the_host_is_refused_under_an_ancestors_proc_with_an_equal_pid() {
        let said = in_new_pid_namespace("pidns-remount");
        assert!(
            said.contains("host setup refused as pid 4242 (/proc/self: 4242): ")
                && said.contains("NSpid line is \"4242 4242\""),
            "{said}"
        );
    }

    /// A fake procfs tree: `self/status` naming this process, process 4242
    /// with tasks 4242 and 4243, their `status`, `stat`, `maps`, `environ`,
    /// `cwd` and one fd (symlinks stand in for magic links), and a
    /// `self/mountinfo`.
    fn fake_proc(dir: &Path, nspid: &str) {
        let write = |rel: &str, text: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, text).expect("write");
        };
        write(
            "self/status",
            &format!("Name:\tx\nNSpid:\t{nspid}\nThreads:\t1\n"),
        );
        write(
            "self/mountinfo",
            "1 0 0:99 / /fake-root rw - fakefs fake rw\n",
        );
        write(
            "4242/status",
            "Name:\tx\nState:\tS (sleeping)\nThreads:\t2\n",
        );
        for tid in ["4242", "4243"] {
            let task = format!("4242/task/{tid}");
            write(
                &format!("{task}/stat"),
                &format!(
                    "{tid} (x) S 1 1 1 0 -1 4194560 {} 12345\n",
                    ["0"; 12].join(" ")
                ),
            );
            write(
                &format!("{task}/maps"),
                "7f0000000000-7f0000001000 r--p 00000000 fd:01 777    /fake/lib.so\n",
            );
            write(&format!("{task}/environ"), "FAKE=1\0");
            std::fs::create_dir_all(dir.join(format!("{task}/fd"))).expect("mkdir fd");
            std::os::unix::fs::symlink(dir.join("4242/status"), dir.join(format!("{task}/fd/3")))
                .expect("fd link");
            std::os::unix::fs::symlink(dir, dir.join(format!("{task}/cwd"))).expect("cwd link");
        }
        std::fs::create_dir_all(dir.join("not-a-pid")).expect("mkdir");
    }

    fn open_proc_root(dir: &Path) -> OwnedFd {
        rustix::fs::open(
            dir,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open fake procfs")
    }

    /// Round 11 (Codex): every procfs access goes through the fd held since
    /// the host was set up, never through the path `/proc`. A host made on
    /// a fake procfs tree checks the namespace, lists pids, task listings,
    /// status, stat, fds, links, maps, environ and mountinfo there.
    #[test]
    fn every_reader_goes_through_the_held_procfs_fd() {
        let dir = TempDir::new();
        fake_proc(&dir.0, &std::process::id().to_string());
        let host = LinuxHost::with_proc(open_proc_root(&dir.0)).expect("host on the fake tree");
        let proc = host.root();
        assert_eq!(host.pids(), Ok(vec![Pid(4242)]));
        let pid = Pid(4242);
        let mut tids = task_ids(proc, pid)
            .expect("task ids")
            .expect("live process");
        tids.sort_unstable();
        assert_eq!(tids, vec![4242, 4243]);
        assert_eq!(
            group_status(proc, pid),
            Ok(Some(StatusFields {
                state: 'S',
                threads: 2
            }))
        );
        let stat = TaskStats::new(proc, pid).read(4243).expect("stat");
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.start_ticks, 12345);
        let fds = task_fds(proc, pid, 4242).expect("fds");
        let status_ino = std::fs::metadata(dir.0.join("4242/status"))
            .expect("stat")
            .ino();
        assert_eq!(fds.len(), 1, "{fds:?}");
        assert_eq!((fds[0].fd, fds[0].target.stat.ino), (3, Some(status_ino)));
        let cwd = proc.link_target(&task_path(pid, 4243, "cwd")).expect("cwd");
        assert_eq!(cwd.text, dir.0.as_os_str());
        let maps = read_maps(&Kernel(proc), &task_path(pid, 4242, "maps")).expect("maps");
        assert_eq!(parse_maps(&maps).expect("parse")[0].ino, 777);
        let env = read_environ(&Kernel(proc), &task_path(pid, 4242, "environ")).expect("environ");
        assert_eq!(env, b"FAKE=1\0");
        let mounts = host.mountinfo().expect("mountinfo");
        assert_eq!(mounts[0].mount_point, Path::new("/fake-root"));
        // The namespace check reads through the fd as well.
        let other = TempDir::new();
        fake_proc(&other.0, &format!("1\t{}", std::process::id()));
        let refused = LinuxHost::with_proc(open_proc_root(&other.0)).unwrap_err();
        assert!(
            refused.to_string().contains("another pid namespace"),
            "{refused}"
        );
    }

    /// The old and new tasks deliberately have the same clock-tick start
    /// time. A held `stat` file must keep reading the first incarnation
    /// after its numeric name is reused, never the replacement.
    #[test]
    fn task_stat_pin_does_not_follow_a_reused_tid_within_one_tick() {
        let dir = TempDir::new();
        fake_proc(&dir.0, &std::process::id().to_string());
        let root = open_proc_root(&dir.0);
        let proc = ProcRoot(root.as_fd());
        let pid = Pid(4242);
        let mut stats = TaskStats::new(proc, pid);
        assert_eq!(stats.read(4243).expect("old stat").start_ticks, 12345);

        let task_dir = dir.0.join("4242/task");
        std::fs::rename(task_dir.join("4243"), task_dir.join("old-task"))
            .expect("move old incarnation");
        std::fs::create_dir(task_dir.join("4243")).expect("reuse numeric tid");
        std::fs::write(
            task_dir.join("4243/stat"),
            format!(
                "4243 (new) R 1 1 1 0 -1 4194560 {} 12345\n",
                ["0"; 12].join(" ")
            ),
        )
        .expect("new stat in same tick");
        assert_eq!(
            TaskStats::new(proc, pid)
                .read(4243)
                .expect("new stat")
                .state,
            'R'
        );
        assert_eq!(stats.read(4243).expect("pinned old stat").state, 'S');
        // That the held file then fails once its task is released is a
        // procfs fact a plain directory cannot model: see
        // held_task_stat_disappears_after_thread_exit.
    }

    /// An already open task `stat` file must stop reading (`ESRCH`) when
    /// that particular thread has exited: its procfs inode holds the
    /// thread's `struct pid`, which no other task takes. This is the kernel
    /// fact the same-tick reuse check relies on ([`TaskStats`]), exercised
    /// without needing to force a numeric tid recycle on the host.
    #[test]
    fn held_task_stat_disappears_after_thread_exit() {
        let host = LinuxHost::new().expect("host setup");
        let pid = Pid(std::process::id());
        let (tid_sender, tid_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            // SAFETY: gettid takes no pointer arguments and changes no state.
            let tid = unsafe { libc::syscall(libc::SYS_gettid) };
            tid_sender
                .send(u32::try_from(tid).expect("positive tid"))
                .expect("send tid");
            release_receiver.recv().expect("release worker");
        });
        let tid = tid_receiver.recv().expect("worker tid");
        let mut stats = TaskStats::new(host.root(), pid);
        let _ = stats.read(tid).expect("live worker stat");
        release_sender.send(()).expect("release worker");
        worker.join().expect("join worker");
        let deadline = Instant::now() + Duration::from_secs(1);
        let after = loop {
            let after = stats.read(tid);
            if after.as_ref().is_err_and(is_vanished) || Instant::now() >= deadline {
                break after;
            }
            thread::sleep(Duration::from_millis(1));
        };
        assert!(
            after.as_ref().is_err_and(is_vanished),
            "a held task stat must not remain readable after thread exit: {after:?}"
        );
    }

    /// The grep check: outside its tests, the crate never names `/proc` as a
    /// path except where [`LinuxHost::new`] opens it and in messages
    /// ([`ProcRoot::shown`]), so nothing bypasses the held fd. Every source
    /// file of the crate is scanned, test modules excluded by module
    /// ([`crate::source_scan`]).
    #[test]
    fn procfs_is_reached_only_through_the_held_fd() {
        use crate::source_scan::{repo_root, rust_files, scan_crate};
        let files = rust_files(&repo_root(), Path::new("crates/diskgc-linux/src"))
            .unwrap_or_else(|e| panic!("{e}"));
        let scanned = scan_crate(&files).unwrap_or_else(|e| panic!("{e}"));
        assert!(scanned.iter().any(|s| s.path.ends_with("host.rs")));
        let literal: Vec<char> = ["\"", "/proc"].concat().chars().collect();
        let mut found = Vec::new();
        for file in &scanned {
            let view = &file.source.with_strings;
            for pos in 0..view.len().saturating_sub(literal.len()) {
                if view[pos..pos + literal.len()] == literal[..]
                    && !file.is_excluded(pos)
                    && !file.line_text(pos).starts_with("const PROC: &str =")
                {
                    found.push(file.at(pos));
                }
            }
            for t in file.tokens.iter().filter(|t| t.word == "PROC") {
                let defined = t.previous.as_deref() == Some("const");
                if !defined && !matches!(file.enclosing_fn(t.pos), Some("open_procfs" | "shown")) {
                    found.push(file.at(t.pos));
                }
            }
        }
        assert!(
            found.is_empty(),
            "procfs reached by path:\n{}",
            found.join("\n")
        );
    }

    /// AC #1 and #2 of task-50, on the developer's machine (never a test
    /// container): unlinking a file and an empty directory this test made
    /// itself is refused by the sandbox gate, with an error that is not a
    /// kernel answer and names the missing marker, and both stay; the
    /// read-only primitives keep working in the same process afterwards.
    #[test]
    fn deleting_outside_a_container_is_refused_by_the_gate() {
        let why = sandbox::check()
            .expect_err("this test must run outside a diskgc test container")
            .to_string();
        let host = LinuxHost::new().expect("host setup");
        let tmp = TempDir::new();
        let file = tmp.0.join("file");
        let empty_dir = tmp.0.join("empty-dir");
        std::fs::write(&file, b"keep me").expect("write test file");
        std::fs::create_dir(&empty_dir).expect("create test dir");
        let dir = open_dir(&host, &tmp.0);

        for (name, what) in [("file", Unlink::NonDir), ("empty-dir", Unlink::Dir)] {
            let err = host
                .unlinkat(&dir, OsStr::new(name), what)
                .expect_err("the sandbox gate must refuse outside a test container");
            assert_eq!(err.cause, Cause::SandboxGate, "{err}");
            assert_eq!(err.kernel_errno(), None, "{err}");
            assert!(
                err.detail.contains("pre-release sandbox gate") && err.detail.contains(&why),
                "the refusal must name the gate and the marker ({why}): {err}"
            );
        }
        // The gate comes first: even a name the primitive would reject is
        // refused by the gate.
        let err = host
            .unlinkat(&dir, OsStr::new("../x"), Unlink::NonDir)
            .expect_err("refused");
        assert_eq!(err.cause, Cause::SandboxGate, "{err}");
        // A permission change (make_writable) is gated like a deletion.
        let mode_before = std::fs::metadata(&empty_dir).expect("stat").mode();
        let sub = resolve(&host, &empty_dir, Want::Directory).expect("resolve");
        let err = host.fchmod(&sub.fd, None).expect_err("refused");
        assert_eq!(err.cause, Cause::SandboxGate, "{err}");
        assert_eq!(
            std::fs::metadata(&empty_dir).expect("stat").mode(),
            mode_before,
            "the gate let a permission change through"
        );

        assert_eq!(std::fs::read(&file).expect("file still there"), b"keep me");
        assert!(empty_dir.is_dir(), "the empty directory was deleted");

        // AC #2: the non-deleting primitives work here, after the refusal.
        let stat = host
            .statx(Base::Dir(&dir), Path::new("file"))
            .expect("statx after a refusal");
        assert_eq!(stat.size, Some(7));
        let sub = host
            .openat2(
                Base::Dir(&dir),
                Path::new("empty-dir"),
                OpenHow {
                    access: Access::Read,
                    directory: true,
                    nofollow: true,
                    resolve: Default::default(),
                },
            )
            .expect("openat2 after a refusal");
        host.statvfs(&sub).expect("statvfs after a refusal");
        host.mountinfo().expect("mountinfo after a refusal");
    }

    /// The deletion engine on the real kernel, on the developer's machine:
    /// the eligibility walk reads a real tree (and refuses, fail closed, a
    /// mode-000 directory, a socket and a FIFO inside), and the delete of an
    /// eligible tree is refused by the sandbox gate at its first unlink, as a
    /// gate refusal (a run error, never a skip or `changed`), with nothing
    /// removed.
    #[test]
    fn the_engine_walks_here_but_its_delete_is_refused_by_the_gate() {
        use diskgc_core::engine::{
            Candidate, DeleteOptions, End, Reason, Rules, WalkStop, delete_unchecked, walk,
        };
        sandbox::check().expect_err("this test must run outside a diskgc test container");
        let host = LinuxHost::new().expect("host setup");
        let tmp = TempDir::new();
        let me = rustix::process::geteuid().as_raw();
        let owners = [me];
        let rules = Rules::new(&owners);
        let parent = resolve(&host, &tmp.0, Want::Directory).expect("resolve");
        let walk_of = |name: &str| {
            let path = tmp.0.join(name);
            let candidate = Candidate::new(&parent, &path).expect("a name");
            walk(&host, &candidate, &rules, None)
        };
        let reason_of = |name: &str| match walk_of(name) {
            Err(WalkStop::Skipped(skip)) => skip.reason,
            other => panic!("{name}: expected a skip, got {other:?}"),
        };

        let tree = tmp.0.join("tree");
        std::fs::create_dir_all(tree.join("sub/deeper")).expect("mkdir");
        std::fs::write(tree.join("a"), b"x").expect("write");
        std::fs::write(tree.join("sub/deeper/b"), b"y").expect("write");
        let path = tmp.0.join("tree");
        let candidate = Candidate::new(&parent, &path).expect("a name");
        let record = walk(&host, &candidate, &rules, None).expect("eligible");
        assert_eq!(record.entries(), 5);
        let done = delete_unchecked(&host, &candidate, &record, DeleteOptions::default());
        match &done.end {
            End::GateRefused { path, error } => {
                assert_eq!(error.cause, Cause::SandboxGate, "{error}");
                assert_eq!(
                    path,
                    &tree.join("a"),
                    "the refused unlink's path is reported"
                );
            }
            other => panic!("expected the gate to refuse, got {other:?}"),
        }
        assert_eq!(done.removed_entries, 0);
        assert!(tree.join("sub/deeper/b").exists() && tree.join("a").exists());

        // Fail closed on a directory this user cannot list (AC #5 of
        // task-5; root could list it, so only as an unprivileged user).
        let locked = tmp.0.join("locked");
        std::fs::create_dir_all(locked.join("unreadable")).expect("mkdir");
        std::fs::write(locked.join("unreadable/f"), b"z").expect("write");
        std::fs::set_permissions(
            locked.join("unreadable"),
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .expect("chmod 000");
        if me != 0 {
            assert_eq!(reason_of("locked"), Reason::Unreadable);
            // The message names the directory by its path, never by the
            // Debug text of an fd (TASK-140: it read
            // `open LinuxFd(OwnedFd { fd: 4 })/.: EACCES (13)`).
            let Err(WalkStop::Skipped(skip)) = walk_of("locked") else {
                panic!("locked: expected a skip");
            };
            assert_eq!(
                skip.detail,
                format!(
                    "cannot list it (fail closed): open {}: EACCES (13)",
                    locked.join("unreadable").display()
                )
            );
        }
        std::fs::set_permissions(
            locked.join("unreadable"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .expect("chmod back");

        let ipc = tmp.0.join("ipc");
        std::fs::create_dir(&ipc).expect("mkdir");
        let _listener =
            std::os::unix::net::UnixListener::bind(ipc.join("socket")).expect("bind a socket");
        assert_eq!(reason_of("ipc"), Reason::ContainsIpc);
        std::fs::remove_file(ipc.join("socket")).expect("remove socket");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            ipc.join("fifo"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .expect("mkfifo");
        assert_eq!(reason_of("ipc"), Reason::ContainsIpc);
    }

    #[test]
    fn the_clock_moves_forward_and_kernel_release_is_supported() {
        let host = LinuxHost::new().expect("host setup");
        let a = host.now();
        host.sleep(Duration::from_millis(2));
        assert!(host.now().saturating_since(a) >= Duration::from_millis(2));
        let release = host.kernel_release().expect("uname");
        diskgc_core::kernel::check_release(&release).expect("tests run on Linux 5.8+");
    }

    #[test]
    fn a_lock_conflicts_until_its_fd_is_closed() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        // The holder takes its lock only after exec, in its own fd table,
        // so no fork of a parallel test can inherit it.
        let holder = Holder::spawn("flock", &dir.0, &dir.0);
        let second = match host.flock_try(open_dir(&host, &dir.0), LockMode::Shared) {
            Ok(TryLock::Busy(fd)) => fd,
            other => panic!("second lock should be busy: {other:?}"),
        };
        let started = host.now();
        let deadline = started.checked_add(Duration::from_millis(50));
        let second = match host.flock_wait(second, LockMode::Exclusive, deadline) {
            Ok(WaitLock::DeadlinePassed(fd)) => fd,
            other => panic!("wait should time out: {other:?}"),
        };
        assert!(host.now().saturating_since(started) >= Duration::from_millis(50));
        drop(holder);
        // Not one attempt with the passed deadline: a `/proc` reader can
        // make the release lag the holder's exit (task-64 #37).
        let bound = host.now().checked_add(RELEASE_BOUND);
        assert!(
            matches!(
                host.flock_wait(second, LockMode::Exclusive, bound),
                Ok(WaitLock::Acquired(_))
            ),
            "closing the holder's fd released the lock within {RELEASE_BOUND:?}"
        );
    }

    /// task-64 #37's cause, pinned: a `/proc` reader takes a short
    /// reference to the open file (kernel fs/proc/fd.c): reading
    /// `/proc/<pid>/fdinfo/<fd>` on every supported kernel, statting or
    /// reading a `/proc/<pid>/fd` link from 5.11, listing that directory
    /// from 6.7. The readers here do all three, so the test holds on every
    /// supported kernel. When the owner's `close` drops
    /// its last fd while a reader holds one, the reader's `fput` is the last
    /// one: the lock goes with `__fput` as the reader's syscall returns,
    /// after `close` has returned. Any in-use scan does this, including
    /// diskgc's own and another test's. So a `flock` tried once right after
    /// `close` can be busy; [`LinuxHost::flock_wait`]'s polling takes it.
    /// If the kernel ever releases synchronously, this test fails and the
    /// bounded waits after a close in these tests can go.
    #[test]
    // The kernel VMs run every `*_kernel_fact` test and assert how many
    // passed (diskgc-e2e/vm-old-kernel-pidfd.nix): adding or renaming one
    // means updating that count.
    fn a_flock_can_outlive_its_close_while_a_proc_reader_holds_the_file_kernel_fact() {
        // The readers hold references to every fd of this process: keep
        // them away from the tests that judge what a close releases.
        let _fd_table = fd_table_guard();
        const READERS: usize = 4;
        // The wait for the first late release is roughly exponential: on
        // one CPU its mean was about 0.6 s and its longest of 30 runs 1.6 s
        // (task-64 notes). A passing run stops at the first late release,
        // so the long bound costs only a run that is about to fail.
        const PROVOKE_FOR: Duration = Duration::from_secs(30);
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let stop = std::sync::atomic::AtomicBool::new(false);
        // Fds whose link stat and fdinfo read both succeeded: a reader that
        // never read would make a false kernel claim below.
        let stats = std::sync::atomic::AtomicU64::new(0);
        let (attempts, late) = std::thread::scope(|scope| {
            for _ in 0..READERS {
                scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        // Errors are fds closing under the reader; counted
                        // out by `stats`.
                        let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
                            continue;
                        };
                        for entry in entries.flatten() {
                            let fdinfo = Path::new("/proc/self/fdinfo").join(entry.file_name());
                            if std::fs::metadata(entry.path()).is_ok()
                                && std::fs::read(fdinfo).is_ok()
                            {
                                stats.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
            let started = Instant::now();
            let (mut attempts, mut late) = (0_u64, 0_u64);
            while late == 0 && started.elapsed() < PROVOKE_FOR {
                attempts += 1;
                // The previous round's lock can itself still be releasing.
                let bound = host.now().checked_add(RELEASE_BOUND);
                let first =
                    match host.flock_wait(open_dir(&host, &dir.0), LockMode::Exclusive, bound) {
                        Ok(WaitLock::Acquired(locked)) => locked,
                        other => panic!("first lock within {RELEASE_BOUND:?}: {other:?}"),
                    };
                let second = open_dir(&host, &dir.0);
                drop(first);
                match host.flock_try(second, LockMode::Exclusive).expect("flock") {
                    TryLock::Acquired(_) => {}
                    TryLock::Busy(second) => {
                        late += 1;
                        let bound = host.now().checked_add(RELEASE_BOUND);
                        assert!(
                            matches!(
                                host.flock_wait(second, LockMode::Exclusive, bound),
                                Ok(WaitLock::Acquired(_))
                            ),
                            "a lock released late was not acquired within {RELEASE_BOUND:?}"
                        );
                    }
                }
            }
            stop.store(true, Ordering::Relaxed);
            (attempts, late)
        });
        let stats = stats.load(Ordering::Relaxed);
        println!(
            "close then flock: {late} late release(s) in {attempts} attempt(s), {stats} stats"
        );
        assert!(stats > 0, "the /proc readers never read an fd");
        assert!(
            late > 0,
            "no close in {attempts} attempts with {READERS} /proc readers left the lock busy for \
             an immediate flock: the kernel released synchronously"
        );
    }

    /// Why the port never lets a held lock be locked again: `flock(2)`
    /// converts shared to exclusive by dropping the shared lock first, so a
    /// failed conversion leaves no lock at all (a kernel fact, pinned here
    /// with raw calls; `LinuxHost` cannot express it, see `Locked`).
    #[test]
    fn a_failed_flock_conversion_drops_the_shared_lock_kernel_fact() {
        let _fd_table = fd_table_guard();
        use rustix::fs::FlockOperation as Op;
        let dir = TempDir::new();
        let open = || std::fs::File::open(&dir.0).expect("open");
        let (a, b, probe) = (open(), open(), open());
        rustix::fs::flock(&a, Op::NonBlockingLockShared).expect("a shared");
        rustix::fs::flock(&b, Op::NonBlockingLockShared).expect("b shared");
        assert!(rustix::fs::flock(&a, Op::NonBlockingLockExclusive).is_err());
        drop(b);
        // a's shared lock is gone: an exclusive lock from elsewhere succeeds
        // once b's release is through (it can lag the close, task-64 #37).
        assert!(
            released_within_bound(|| {
                rustix::fs::flock(&probe, Op::NonBlockingLockExclusive).is_ok()
            }),
            "the kernel kept the shared lock; the hazard the port guards against is gone"
        );
    }

    /// The port's version of the same sequence: the shared lock cannot be
    /// converted, so it is still held after another holder leaves.
    #[test]
    fn a_held_shared_lock_stays_held() {
        let _fd_table = fd_table_guard();
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let lock = |mode| {
            host.flock_try(open_dir(&host, &dir.0), mode)
                .expect("flock")
        };
        let TryLock::Acquired(a) = lock(LockMode::Shared) else {
            panic!("a shared")
        };
        let TryLock::Acquired(b) = lock(LockMode::Shared) else {
            panic!("b shared")
        };
        assert!(matches!(lock(LockMode::Exclusive), TryLock::Busy(_)));
        drop(b);
        assert!(
            matches!(lock(LockMode::Exclusive), TryLock::Busy(_)),
            "a's shared lock must still be held"
        );
        drop(a);
        // The release can lag the close (task-64 #37).
        let bound = host.now().checked_add(RELEASE_BOUND);
        assert!(matches!(
            host.flock_wait(open_dir(&host, &dir.0), LockMode::Exclusive, bound),
            Ok(WaitLock::Acquired(_))
        ));
    }

    /// The readers on a real process larger than every first buffer: an
    /// environment over 64 KiB (`environ` re-read with a larger buffer, in
    /// one call), more fds than one `getdents` buffer holds (listing re-made
    /// with a larger buffer), and thousands of mappings (`maps` over many
    /// `read` calls, in two passes). The complete contents must come back.
    #[test]
    fn a_big_process_is_read_completely() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new();
        let file = dir.0.join("held");
        std::fs::write(&file, b"held").expect("write");
        let big = "e".repeat(BIG_ENV);
        let holder = Holder::spawn_with("big", &file, &dir.0, &[("DISKGC_LINUX_TEST_BIG", &big)]);
        let pid = holder.pid();

        let env = host.proc_environ(pid).expect("environ");
        assert_eq!(
            env.get("DISKGC_LINUX_TEST_BIG").map(|v| v.len()),
            Some(BIG_ENV),
            "the large variable came back cut"
        );
        let fds = host.proc_fds(pid).expect("fds");
        let dev_null = fds.iter().filter(|f| f.target.text == "/dev/null").count();
        assert!(
            dev_null >= BIG_FDS,
            "{dev_null} of {BIG_FDS} fds on /dev/null were listed"
        );
        let maps = host.proc_maps(pid).expect("maps");
        let expected = std::fs::read_to_string(format!("{PROC}/{pid}/maps"))
            .expect("read maps")
            .lines()
            .count();
        assert!(expected > BIG_MAPS, "the holder has {expected} mappings");
        assert_eq!(maps.len(), expected, "every mapping of the idle holder");
    }

    /// A scripted `/proc`: per open, the content one `read` call returns
    /// (later calls of that open return nothing, as `environ` and `maps` do
    /// once their memory map is gone), whether reading from the start again
    /// finds anything, and an fd directory of `fds` entries of 24 bytes each.
    struct Scripted {
        opens: std::cell::RefCell<std::collections::VecDeque<(Vec<u8>, bool)>>,
        fds: u32,
    }

    struct ScriptedFile {
        content: Vec<u8>,
        read_once: bool,
        alive_after: bool,
    }

    impl ProcFiles for Scripted {
        type File = ScriptedFile;

        fn open(&self, _: &str) -> Result<ScriptedFile, HostError> {
            let (content, alive_after) =
                self.opens.borrow_mut().pop_front().expect("scripted open");
            Ok(ScriptedFile {
                content,
                read_once: false,
                alive_after,
            })
        }

        fn read(&self, file: &mut ScriptedFile, buf: &mut [u8]) -> Result<usize, HostError> {
            if file.read_once {
                return Ok(0);
            }
            file.read_once = true;
            let n = buf.len().min(file.content.len());
            buf[..n].copy_from_slice(&file.content[..n]);
            Ok(n)
        }

        fn rewind(&self, file: &mut ScriptedFile) -> Result<(), HostError> {
            file.read_once = !file.alive_after;
            Ok(())
        }

        fn list_once(&self, _: &str, size: usize) -> Result<(Vec<u32>, bool), HostError> {
            let fit = u32::try_from(size / 24).unwrap_or(u32::MAX).min(self.fds);
            Ok(((0..fit).collect(), fit == self.fds))
        }
    }

    fn scripted(opens: Vec<(&[u8], bool)>, fds: u32) -> Scripted {
        Scripted {
            opens: std::cell::RefCell::new(
                opens.into_iter().map(|(c, a)| (c.to_vec(), a)).collect(),
            ),
            fds,
        }
    }

    /// `environ` larger than the first buffer: the reader must re-open with a
    /// larger buffer and take the content of one call, never a prefix spread
    /// over several calls (the scripted file gives nothing after its first
    /// call, like an `environ` whose memory map went away).
    #[test]
    fn environ_is_read_whole_in_one_call() {
        let content = vec![b'e'; FIRST_BUFFER + 1000];
        let files = scripted(vec![(&content, true), (&content, true)], 0);
        assert_eq!(read_environ(&files, "environ"), Ok(content.clone()));
    }

    /// `maps`: two passes on separate opens, each confirmed, united. The
    /// first pass missed a mapping (moved below its position), the second
    /// has it; a pass whose memory map went away (nothing on re-reading
    /// from the start) makes the read find nothing.
    #[test]
    fn maps_are_read_in_two_confirmed_passes() {
        let files = scripted(
            vec![
                (b"1 r 0 0:1 7 /a\n", true),
                (b"0 r 0 0:1 9 /x\n1 r 0 0:1 7 /a\n", true),
            ],
            0,
        );
        let got = read_maps(&files, "maps").expect("maps");
        let text = String::from_utf8(got).expect("utf8");
        assert!(text.contains("/x") && text.contains("/a"), "{text:?}");
        let files = scripted(
            vec![(b"1 r 0 0:1 7 /a\n", false), (b"1 r 0 0:1 7 /a\n", true)],
            0,
        );
        assert_eq!(
            read_maps(&files, "maps"),
            Ok(Vec::new()),
            "an unconfirmed pass"
        );
    }

    /// The fd table comes from one complete `getdents` call: a listing that
    /// did not fit is made again with a larger buffer, never accepted.
    #[test]
    fn fd_numbers_come_from_one_complete_listing() {
        let fds = u32::try_from(FIRST_BUFFER / 24).unwrap() + 500;
        let files = scripted(vec![], fds);
        assert_eq!(fd_numbers(&files, "fd").map(|v| v.len()), Ok(fds as usize));
    }

    // ---- diskgc's own state (docs/architecture.md, The Host port) ----

    fn state_host(dir: &Path) -> LinuxHost {
        LinuxHost::new()
            .expect("host setup")
            .with_state_dir(dir.join("state/diskgc"))
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn exclusive(host: &LinuxHost, key: &FsKey) -> Locked<LinuxFd> {
        match host
            .flock_try(
                host.state_lock(key).expect("lock file").fd,
                LockMode::Exclusive,
            )
            .expect("flock")
        {
            TryLock::Acquired(locked) => locked,
            TryLock::Busy(_) => panic!("the single-writer lock is busy"),
        }
    }

    #[test]
    fn state_lock_creates_a_private_directory_and_lock_file_and_reuses_them() {
        let tmp = TempDir::new();
        let host = state_host(&tmp.0);
        let key = FsKey::dev(Dev {
            major: 0,
            minor: 42,
        });
        let first = host.state_lock(&key).expect("first");
        let state = tmp.0.join("state/diskgc");
        let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o7777;
        assert_eq!(mode(&state), 0o700);
        assert_eq!(mode(&state.join("index-dev-0-42.lock")), 0o600);
        drop(first);
        drop(host.state_lock(&key).expect("again"));
        assert_eq!(names_in(&state), ["index-dev-0-42.lock"]);
        // Nothing to read yet.
        assert_eq!(host.state_read(&key, 1 << 20).expect("read"), None);
    }

    #[test]
    fn a_state_directory_others_may_write_to_is_refused() {
        let tmp = TempDir::new();
        let host = state_host(&tmp.0);
        let key = FsKey::dev(Dev {
            major: 0,
            minor: 42,
        });
        drop(host.state_lock(&key).expect("made"));
        let state = tmp.0.join("state/diskgc");
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        let refused = host.state_lock(&key).expect_err("group/other-writable");
        assert!(
            refused.to_string().contains("writable by no one else"),
            "{refused}"
        );
        assert!(host.state_read(&key, 1 << 20).is_err());
    }

    #[test]
    fn state_write_replaces_the_snapshot_whole_and_leaves_nothing_else() {
        let tmp = TempDir::new();
        let host = state_host(&tmp.0);
        let key = FsKey::dev(Dev {
            major: 0,
            minor: 42,
        });
        let locked = exclusive(&host, &key);
        host.state_write(&locked, &key, b"first snapshot")
            .expect("write");
        host.state_write(&locked, &key, b"second").expect("rewrite");
        drop(locked);
        let file = host
            .state_read(&key, 1 << 20)
            .expect("read")
            .expect("a file");
        assert_eq!(file.file_type, FileType::Regular);
        assert_eq!(file.mode & 0o077, 0, "{:o}", file.mode);
        assert_eq!(file.uid, rustix::process::geteuid().as_raw());
        assert_eq!(file.bytes.as_deref(), Some(&b"second"[..]));
        assert_eq!(
            names_in(&tmp.0.join("state/diskgc")),
            ["index-dev-0-42", "index-dev-0-42.lock"]
        );
        // Larger than the limit: not read.
        let file = host.state_read(&key, 3).expect("read").expect("a file");
        assert_eq!((file.size, file.bytes), (6, None));
    }

    #[test]
    fn a_complete_leftover_new_file_is_promoted_then_replaced() {
        let tmp = TempDir::new();
        let host = state_host(&tmp.0);
        let key = FsKey::dev(Dev {
            major: 0,
            minor: 42,
        });
        let locked = exclusive(&host, &key);
        let state = tmp.0.join("state/diskgc");
        std::fs::write(state.join("index-dev-0-42.new"), b"left by a crash").expect("plant");
        host.state_write(&locked, &key, b"fresh").expect("write");
        let file = host
            .state_read(&key, 1 << 20)
            .expect("read")
            .expect("a file");
        assert_eq!(file.bytes.as_deref(), Some(&b"fresh"[..]));
        assert_eq!(names_in(&state), ["index-dev-0-42", "index-dev-0-42.lock"]);
    }

    #[test]
    fn a_symlinked_snapshot_is_reported_never_followed() {
        let tmp = TempDir::new();
        let host = state_host(&tmp.0);
        let key = FsKey::dev(Dev {
            major: 0,
            minor: 42,
        });
        drop(host.state_lock(&key).expect("made"));
        let target = tmp.0.join("elsewhere");
        std::fs::write(&target, b"not a snapshot").expect("target");
        std::os::unix::fs::symlink(&target, tmp.0.join("state/diskgc/index-dev-0-42"))
            .expect("symlink");
        let file = host
            .state_read(&key, 1 << 20)
            .expect("read")
            .expect("an entry");
        assert_eq!((file.file_type, file.bytes), (FileType::Symlink, None));
    }

    #[test]
    fn state_write_refuses_another_keys_lock() {
        let tmp = TempDir::new();
        let host = state_host(&tmp.0);
        let key = FsKey::dev(Dev {
            major: 0,
            minor: 42,
        });
        let other = FsKey::dev(Dev {
            major: 0,
            minor: 43,
        });
        let locked = exclusive(&host, &other);
        drop(host.state_lock(&key).expect("its own lock file"));
        let refused = host
            .state_write(&locked, &key, b"x")
            .expect_err("another key's lock");
        assert!(
            refused.to_string().contains("not this snapshot's"),
            "{refused}"
        );
        assert_eq!(host.state_read(&key, 1 << 20).expect("read"), None);
    }

    /// The space guard compares filesystems by the device mountinfo names
    /// (the superblock's), as the reclaimed `PATH`'s mount does.
    #[test]
    fn the_state_lock_names_its_filesystem_as_mountinfo_does() {
        let tmp = TempDir::new();
        let host = state_host(&tmp.0);
        let key = FsKey::dev(Dev {
            major: 0,
            minor: 42,
        });
        let lock = host.state_lock(&key).expect("lock file");
        let dir = resolve(&host, &tmp.0.join("state/diskgc"), Want::Directory).expect("resolve");
        assert_eq!(lock.dir_dev, dir.mount.dev);
    }

    #[test]
    fn without_a_state_directory_the_state_operations_fail() {
        let host = LinuxHost::new().expect("host setup");
        let key = FsKey::dev(Dev {
            major: 0,
            minor: 42,
        });
        assert!(host.state_lock(&key).is_err());
        assert!(host.state_read(&key, 1).is_err());
    }

    // ---- The per-user lock (docs/safety.md, Host lock; TASK-277) ----

    /// `open_user_lock_ungated` on `dir` (the place itself is
    /// `user_lock_path`'s: tested below), with the host's `/proc`.
    fn open_user_lock(
        host: &LinuxHost,
        dir: &Path,
        make: MakeUserLock,
    ) -> Result<Option<(OwnedFd, PathBuf)>, UserLockError> {
        crate::state::open_user_lock_ungated(dir, make, host.proc.as_fd())
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::symlink_metadata(p)
            .expect("stat")
            .permissions()
            .mode()
            & 0o7777
    }

    #[test]
    fn the_per_user_lock_makes_a_private_directory_and_file_and_reuses_them() {
        let tmp = TempDir::new();
        let host = LinuxHost::new().expect("host setup");
        let dir = tmp.0.join("run/diskgc");
        // Never: nothing is made.
        assert!(matches!(
            open_user_lock(&host, &dir, MakeUserLock::Never),
            Ok(None)
        ));
        assert!(!tmp.0.join("run").exists());
        let (fd, path) = open_user_lock(&host, &dir, MakeUserLock::IfAbsent)
            .expect("made")
            .expect("open");
        assert_eq!(path, dir.join("lock"));
        assert_eq!(
            mode_of(&tmp.0.join("run")),
            0o700,
            "a missing parent is made 0700"
        );
        assert_eq!(mode_of(&dir), 0o700);
        assert_eq!(mode_of(&dir.join("lock")), 0o600);
        // Exclusive, so a second open of the same file is busy until dropped.
        let first = LinuxFd { fd, shown: path };
        let TryLock::Acquired(held) = host.flock_try(first, LockMode::Exclusive).expect("flock")
        else {
            panic!("the per-user lock is busy");
        };
        let (fd, shown) = open_user_lock(&host, &dir, MakeUserLock::Never)
            .expect("reused")
            .expect("present");
        assert!(matches!(
            host.flock_try(LinuxFd { fd, shown }, LockMode::Shared)
                .expect("flock"),
            TryLock::Busy(_)
        ));
        drop(held);
        assert_eq!(names_in(&dir), ["lock"]);
    }

    /// The parent is reached following symlinks (`~/.local` a symlink to
    /// another directory of the user's), as the state directory is.
    #[test]
    fn the_per_user_lock_follows_a_symlinked_parent() {
        let tmp = TempDir::new();
        let host = LinuxHost::new().expect("host setup");
        std::fs::create_dir_all(tmp.0.join("home")).expect("home");
        std::fs::create_dir_all(tmp.0.join("elsewhere")).expect("elsewhere");
        std::os::unix::fs::symlink(tmp.0.join("elsewhere"), tmp.0.join("home/.local"))
            .expect("symlink");
        let dir = tmp.0.join("home/.local/state/diskgc");
        let (_, path) = open_user_lock(&host, &dir, MakeUserLock::IfAbsent)
            .expect("made")
            .expect("open");
        assert_eq!(path, dir.join("lock"));
        assert!(tmp.0.join("elsewhere/state/diskgc/lock").is_file());
    }

    #[test]
    fn the_per_user_lock_refuses_what_another_user_could_swap_or_hold() {
        let symlinked_dir = |root: &Path| {
            std::fs::create_dir_all(root.join("elsewhere")).expect("target");
            std::fs::create_dir_all(root.join("run")).expect("run");
            std::os::unix::fs::symlink(root.join("elsewhere"), root.join("run/diskgc"))
                .expect("symlink");
        };
        let file_as_dir = |root: &Path| {
            std::fs::create_dir_all(root.join("run")).expect("run");
            std::fs::write(root.join("run/diskgc"), b"").expect("file");
        };
        let open_dir = |root: &Path| {
            std::fs::create_dir_all(root.join("run/diskgc")).expect("dir");
            std::fs::set_permissions(
                root.join("run/diskgc"),
                std::fs::Permissions::from_mode(0o777),
            )
            .expect("chmod");
        };
        let symlinked_lock = |root: &Path| {
            std::fs::create_dir_all(root.join("run/diskgc")).expect("dir");
            std::fs::write(root.join("target"), b"").expect("target");
            std::os::unix::fs::symlink(root.join("target"), root.join("run/diskgc/lock"))
                .expect("symlink");
        };
        let open_lock = |root: &Path| {
            std::fs::create_dir_all(root.join("run/diskgc")).expect("dir");
            std::fs::write(root.join("run/diskgc/lock"), b"").expect("lock");
            std::fs::set_permissions(
                root.join("run/diskgc/lock"),
                std::fs::Permissions::from_mode(0o644),
            )
            .expect("chmod");
        };
        let lock_dir = |root: &Path| {
            std::fs::create_dir_all(root.join("run/diskgc/lock")).expect("a directory");
        };
        // Opened (read-only, non-blocking) like a file: refused by its type.
        let lock_fifo = |root: &Path| {
            std::fs::create_dir_all(root.join("run/diskgc")).expect("dir");
            rustix::fs::mknodat(
                CWD,
                root.join("run/diskgc/lock"),
                rustix::fs::FileType::Fifo,
                Mode::from_raw_mode(0o600),
                0,
            )
            .expect("mkfifo");
        };
        // (case, what to plant below the test root, why it is refused)
        type Plant = dyn Fn(&Path);
        let cases: [(&str, &Plant, &str); 7] = [
            ("symlinked directory", &symlinked_dir, "is a symlink"),
            ("a file as the directory", &file_as_dir, "not a directory"),
            ("open directory", &open_dir, "writable by no one else"),
            ("symlinked lock", &symlinked_lock, "is a symlink"),
            ("lock others may open", &open_lock, "no one else may open"),
            (
                "a directory as the lock",
                &lock_dir,
                "must be a regular file",
            ),
            ("a FIFO as the lock", &lock_fifo, "must be a regular file"),
        ];
        let host = LinuxHost::new().expect("host setup");
        for (case, plant, why) in cases {
            for make in [MakeUserLock::IfAbsent, MakeUserLock::Never] {
                let tmp = TempDir::new();
                plant(&tmp.0);
                let refused =
                    open_user_lock(&host, &tmp.0.join("run/diskgc"), make).expect_err(case);
                let UserLockError::Unsafe { error, .. } = &refused else {
                    panic!("{case}: {refused:?}");
                };
                assert!(error.to_string().contains(why), "{case}: {error}");
            }
        }
    }

    /// TASK-277 review (Codex 1): another user's directory, even one this
    /// user cannot read or search (0700), is inspected through `O_PATH`
    /// and refused as unsafe, never an `EACCES`. Root-owned directories
    /// stand in for another user's (`/proc` everywhere, `/root` where it is
    /// 0700 and not this user's).
    #[test]
    fn another_users_per_user_lock_directory_is_unsafe_not_eacces() {
        let host = LinuxHost::new().expect("host setup");
        let uid = rustix::process::geteuid().as_raw();
        let mut tried = 0;
        for dir in ["/proc", "/root"] {
            let Ok(meta) = std::fs::symlink_metadata(dir) else {
                continue;
            };
            if meta.uid() == uid || !meta.is_dir() {
                continue;
            }
            tried += 1;
            let refused =
                open_user_lock(&host, Path::new(dir), MakeUserLock::IfAbsent).expect_err(dir);
            let UserLockError::Unsafe { error, .. } = &refused else {
                panic!("{dir}: {refused:?}");
            };
            assert!(
                error.to_string().contains("must be owned by uid"),
                "{dir}: {error}"
            );
        }
        assert!(tried > 0, "no directory of another user to try");
    }

    /// A per-user lock that cannot be made where it belongs is an errno
    /// (here `EACCES`, a parent the user may not write), which the core
    /// counts as the environment's: the reclaim then runs without a lock.
    #[test]
    fn a_per_user_lock_in_an_unwritable_parent_fails_with_its_errno() {
        let tmp = TempDir::new();
        let host = LinuxHost::new().expect("host setup");
        std::fs::create_dir_all(tmp.0.join("ro")).expect("dir");
        std::fs::set_permissions(tmp.0.join("ro"), std::fs::Permissions::from_mode(0o500))
            .expect("chmod");
        let failed = open_user_lock(&host, &tmp.0.join("ro/diskgc"), MakeUserLock::IfAbsent)
            .expect_err("unwritable");
        std::fs::set_permissions(tmp.0.join("ro"), std::fs::Permissions::from_mode(0o700))
            .expect("chmod back");
        let UserLockError::Failed { error, .. } = &failed else {
            panic!("{failed:?}");
        };
        if rustix::process::geteuid().is_root() {
            return; // root writes anyway; nothing to show
        }
        assert!(error.is(Errno::EACCES), "{error}");
        assert!(diskgc_core::hostlock::environmental(error));
    }

    /// A runtime directory counts only as logind makes it: a directory, not
    /// a symlink, of the user, mode 0700.
    #[test]
    fn only_the_users_own_0700_directory_is_a_runtime_directory() {
        let tmp = TempDir::new();
        let uid = rustix::process::geteuid().as_raw();
        let dir = tmp.0.join("1000");
        std::fs::create_dir(&dir).expect("dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        assert!(is_users_runtime_dir(&dir, uid));
        assert!(!is_users_runtime_dir(&dir, uid + 1), "another user's");
        std::os::unix::fs::symlink(&dir, tmp.0.join("link")).expect("symlink");
        assert!(!is_users_runtime_dir(&tmp.0.join("link"), uid), "a symlink");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(!is_users_runtime_dir(&dir, uid), "mode 0755");
        assert!(!is_users_runtime_dir(&tmp.0.join("absent"), uid));
    }

    /// The place is the system's, never the environment's: the runtime
    /// directory when `/run/user/<uid>` is the user's (0700), otherwise the
    /// account's home.
    #[test]
    fn the_per_user_lock_path_is_the_runtime_directory_or_the_home() {
        let host = LinuxHost::new().expect("host setup");
        let uid = rustix::process::geteuid().as_raw();
        let path = host.user_lock_path();
        if runtime_dir_is_users(uid) {
            assert_eq!(path, Some(diskgc_core::hostlock::runtime_user_lock(uid)));
        } else {
            let home = account_home(uid)
                .expect("account database")
                .map(|home| diskgc_core::hostlock::home_user_lock(&home));
            assert_eq!(path, home);
        }
        // The fallback is used only without an account home.
        let with_fallback = LinuxHost::new()
            .expect("host setup")
            .with_home_fallback(PathBuf::from("/nonexistent-fallback"));
        if account_home(uid).ok().flatten().is_some() || runtime_dir_is_users(uid) {
            assert_eq!(with_fallback.user_lock_path(), path);
        }
    }

    #[test]
    fn the_boot_id_is_the_kernels() {
        let host = LinuxHost::new().expect("host setup");
        let id = host.boot_id().expect("boot id");
        let kernel = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").expect("read");
        assert_eq!(id, kernel.trim());
    }
}
