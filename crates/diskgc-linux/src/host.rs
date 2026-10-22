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
    Access, Base, BoundSocket, CommandExit, CommandInvocation, CommandRun, Delete, DirListing,
    DockerEnvironment, DockerQuery, Environ, Errno, FileType, Host, HostError, LinkTarget,
    LockMode, Locked, MAX_COMMAND_OUTPUT_PREFIX_BYTES, MapEntry, MonoTime, MountEntry, OpenHow,
    Pid, ProcError, ProcFd, ProcLink, ProcStatus, ProcView, Space, Stat, Timestamp, TryLock,
    UidMap, Unlink, WaitLock, is_single_name, read_proc_view,
};
use diskgc_core::report::Signal;
use rustix::event::PollFlags;
use rustix::fs::{AtFlags, CWD, FlockOperation, Mode, OFlags, PROC_SUPER_MAGIC, ResolveFlags};

use crate::mountinfo::parse_mountinfo;
use crate::procfs::{
    StatusFields, list_in_one_call, parse_environ, parse_maps, parse_status, parse_status_ids,
    read_confirmed, read_in_one_call, read_two_passes,
};
use crate::sandbox::{self, Permit};
use crate::sockdiag;
use crate::stat::{STATX_MASK, stat_from_statx};
use crate::tasks::{
    Listed, Listing, PidfdPoll, Polled, Process, StatusRead, TaskStat, Tasks, Want, collect,
    is_vanished, owner_through_live_task, parse_task_stat,
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

/// An open file descriptor (close-on-exec). Dropping it closes it.
#[derive(Debug)]
pub struct LinuxFd(OwnedFd);

impl AsFd for LinuxFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// The real host. Made only by [`LinuxHost::new`], which checks what the
/// `/proc` readers stand on.
#[derive(Debug)]
pub struct LinuxHost {
    /// The procfs mount checked at construction (`O_PATH` directory).
    proc: OwnedFd,
    #[cfg(not(feature = "release-without-sandbox-gate"))]
    clock_offset_seconds: u64,
}

impl LinuxHost {
    /// Install the cooperative handlers before an `ensure` starts. The
    /// binary runs one command then exits, so the first signal is retained.
    pub fn install_interrupt_handlers() -> Result<(), HostError> {
        install_interrupt_handlers_ungated()
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
        let proc = rustix::fs::open(
            PROC,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| HostError::errno("open", PROC, errno_of(e)))?;
        let fs = rustix::fs::fstatfs(&proc)
            .map_err(|e| HostError::errno("fstatfs", PROC, errno_of(e)))?;
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
        Self::with_proc(proc)
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
            #[cfg(not(feature = "release-without-sandbox-gate"))]
            clock_offset_seconds,
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

/// Resolve a configured user through the system name service. The system
/// account cutoff comes from docs/reference.md; the caller also rejects
/// duplicate resolved UIDs. Static musl builds inherit musl's local-account
/// resolver limits (TASK91).
pub fn resolve_owner(name: &str) -> Result<u32, String> {
    let cname = std::ffi::CString::new(name)
        .map_err(|_| format!("owners: name {name:?} contains a NUL byte"))?;
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
        if code == libc::ERANGE && storage.len() < (1 << 20) {
            storage.resize(storage.len() * 2, 0);
            continue;
        }
        if code != 0 {
            return Err(format!(
                "owners: cannot resolve {name:?}: {}",
                std::io::Error::from_raw_os_error(code)
            ));
        }
        if result.is_null() {
            return Err(format!(
                "owners: user {name:?} does not exist in this binary's account resolver \
                 (static musl builds cannot load LDAP/sssd NSS modules; use a trusted \
                 {{uid: N}} entry, a local passwd entry, or nscd)"
            ));
        }
        // SAFETY: getpwnam_r returned success and a non-null pointer to entry.
        let uid = unsafe { entry.assume_init().pw_uid };
        if uid < 1000 {
            return Err(format!(
                "owners: user {name:?} resolves to system uid {uid} (< 1000)"
            ));
        }
        return Ok(uid);
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
        let text = rustix::fs::readlinkat(self.0, rel, Vec::new())
            .map_err(|e| HostError::errno("readlink", Self::shown(rel), errno_of(e)))?;
        Ok(LinkTarget {
            text: OsString::from_vec(text.into_bytes()),
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
    let mut buf: Vec<u8> = Vec::with_capacity(DIRENT_BUFFER);
    let mut entries = rustix::fs::RawDir::new(fd, buf.spare_capacity_mut());
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
        Ok(text) => {
            let text = String::from_utf8_lossy(&text);
            let nspid = text.lines().find_map(|line| line.strip_prefix("NSpid:"));
            match nspid.map(|line| line.split_whitespace().collect::<Vec<_>>()) {
                Some(fields) if fields == [own.to_string()] => return Ok(()),
                Some(fields) => format!(
                    "its NSpid line is {:?} (pids from the namespace of the mounted /proc down \
                     to this process's own), but this process is pid {own} in its own \
                     namespace: /proc belongs to another pid namespace",
                    fields.join(" ")
                ),
                None => "has no NSpid line, so its pid namespace cannot be confirmed".to_owned(),
            }
        }
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

fn errno_of(e: rustix::io::Errno) -> Errno {
    Errno(e.raw_os_error())
}

fn io_error(op: &'static str, subject: impl std::fmt::Display, e: &std::io::Error) -> HostError {
    match e.raw_os_error() {
        Some(raw) => HostError::errno(op, subject, Errno(raw)),
        None => HostError::invalid(op, subject, e.to_string()),
    }
}

fn shown(base: &Base<'_, LinuxFd>, path: &Path) -> String {
    match base {
        Base::Root => path.display().to_string(),
        Base::Dir(fd) => format!("{fd:?}/{}", path.display()),
    }
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

/// `<pid>/task/<tid>/<rest>`, relative to the procfs root.
fn task_path(pid: Pid, tid: u32, rest: &str) -> String {
    format!("{pid}/task/{tid}/{rest}")
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
    let dir = format!("{pid}/task");
    let listed = proc.names(&dir).and_then(|names| {
        names
            .iter()
            .map(|name| {
                std::str::from_utf8(name)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| {
                        HostError::invalid(
                            "getdents",
                            ProcRoot::shown(&dir),
                            format!("unexpected entry {:?}", String::from_utf8_lossy(name)),
                        )
                    })
            })
            .collect()
    });
    vanished_as_none(listed)
}

/// One read of `/proc/<pid>/status` (see [`StatusFields`]); `None` when it
/// said `ENOENT`.
fn group_status(proc: ProcRoot<'_>, pid: Pid) -> Result<Option<StatusFields>, ProcError> {
    let path = format!("{pid}/status");
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
            let path = format!("{pid}/task/{tid}");
            match proc.open(&path, OFlags::PATH | OFlags::DIRECTORY) {
                Ok(fd) => Ok(Some(fd)),
                Err(e) if is_vanished(&e) => Ok(None),
                Err(e) => Err(ProcError::from_host(e)),
            }
        },
        |tid, old| {
            let path = format!("{pid}/task/{tid}");
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
            let old_stat = rustix::fs::fstat(old)
                .map_err(|e| ProcError::from_host(HostError::errno("fstat", &path, errno_of(e))))?;
            let fresh_stat = rustix::fs::fstat(&fresh)
                .map_err(|e| ProcError::from_host(HostError::errno("fstat", &path, errno_of(e))))?;
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
        Err(e) => Err(ProcError::Failed(HostError::errno(
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
        _ => Err(ProcError::Failed(HostError::errno(
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
        .map_err(|e| ProcError::Failed(HostError::errno("poll", pid, errno_of(e))))?;
    let answer = pidfd_poll_answer(pid, fds[0].revents())?;
    if answer == PidfdPoll::Readable && pidfd_released(pid, pidfd)? {
        Ok(PidfdPoll::Hangup)
    } else {
        Ok(answer)
    }
}

/// How the readers reach process `pid`: its task listings, its pidfd and
/// its `status` (for the rule, see [`crate::tasks::has_exited`]).
fn process<'a>(
    proc: ProcRoot<'a>,
    pid: Pid,
    pidfd: &'a OwnedFd,
) -> Process<
    impl FnMut() -> Listed + 'a,
    impl FnMut() -> Polled + 'a,
    impl FnMut() -> StatusRead + 'a,
> {
    let mut polls = 0;
    Process {
        pid: pid.0,
        list: move || anchored_task_listing(proc, pid),
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

/// Holds the procfs inode for each task whose `stat` is read. A procfs task
/// inode holds its `struct pid`, so a later read through the directory fd
/// cannot silently follow a reused numeric tid to another `struct pid`.
/// Keep the handle through all rounds: a vanished incarnation must make the
/// read untrustworthy, even when the replacement has the same start tick.
struct TaskStats<'a> {
    proc: ProcRoot<'a>,
    pid: Pid,
    pins: BTreeMap<u32, OwnedFd>,
}

impl<'a> TaskStats<'a> {
    fn new(proc: ProcRoot<'a>, pid: Pid) -> Self {
        Self {
            proc,
            pid,
            pins: BTreeMap::new(),
        }
    }

    fn read(&mut self, tid: u32) -> Result<TaskStat, HostError> {
        let dir = format!("{}/task/{tid}", self.pid);
        if !self.pins.contains_key(&tid) {
            let pin = self.proc.open(&dir, OFlags::PATH | OFlags::DIRECTORY)?;
            self.pins.insert(tid, pin);
        }
        let path = task_path(self.pid, tid, "stat");
        let pin = self.pins.get(&tid).expect("task directory was just pinned");
        let fd = rustix::fs::openat(pin, "stat", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| HostError::errno("open", ProcRoot::shown(&path), errno_of(e)))?;
        let mut text = Vec::new();
        std::fs::File::from(fd)
            .read_to_end(&mut text)
            .map_err(|e| io_error("read", ProcRoot::shown(&path), &e))?;
        parse_task_stat(&text).map_err(|e| HostError::invalid("parse", ProcRoot::shown(&path), e))
    }
}

/// Bounds of the one-call reads: the first buffer, and the largest one
/// before giving up (`Failed`). 16 MiB of directory entries hold several
/// hundred thousand fds; 64 MiB exceeds any environment `execve` accepts.
const FIRST_BUFFER: usize = 64 * 1024;
const MAX_FD_LISTING: usize = 16 * 1024 * 1024;
const MAX_ENVIRON: usize = 64 * 1024 * 1024;

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
    type File = std::fs::File;

    fn open(&self, path: &str) -> Result<std::fs::File, HostError> {
        self.0.open(path, OFlags::RDONLY).map(std::fs::File::from)
    }

    fn read(&self, file: &mut std::fs::File, buf: &mut [u8]) -> Result<usize, HostError> {
        file.read(buf)
            .map_err(|e| io_error("read", format!("{file:?}"), &e))
    }

    fn rewind(&self, file: &mut std::fs::File) -> Result<(), HostError> {
        file.seek(SeekFrom::Start(0))
            .map(|_| ())
            .map_err(|e| io_error("seek", format!("{file:?}"), &e))
    }

    fn list_once(&self, dir: &str, size: usize) -> Result<(Vec<u32>, bool), HostError> {
        let fd = self.0.open(dir, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let mut buf: Vec<u8> = Vec::with_capacity(size);
        let mut entries = rustix::fs::RawDir::new(&fd, buf.spare_capacity_mut());
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
                            HostError::invalid(
                                "getdents",
                                dir,
                                format!("unexpected entry {name:?}"),
                            )
                        })?;
                    fds.push(fd);
                }
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
    let mut fds = Vec::new();
    for fd in fd_numbers(&Kernel(proc), &dir)? {
        match proc.link_target(&format!("{dir}/{fd}")) {
            Ok(target) => fds.push(ProcFd { fd, target }),
            Err(e) if e.is(Errno::ENOENT) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(fds)
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

/// Reads `maps` of one task in two passes on separate opens, each to the
/// end and confirmed (see [`read_confirmed`]), and takes their union (see
/// [`read_two_passes`]).
fn read_maps(files: &impl ProcFiles, path: &str) -> Result<Vec<u8>, HostError> {
    read_two_passes(|| {
        read_confirmed(
            || files.open(path),
            |file| {
                let mut content = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    match files.read(file, &mut chunk)? {
                        0 => return Ok(content),
                        n => content.extend_from_slice(&chunk[..n]),
                    }
                }
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
) -> Result<(String, Vec<u8>), ProcError> {
    let pidfd = open_pidfd(pid)?;
    let mut stats = TaskStats::new(proc, pid);
    let mut got = collect(
        process(proc, pid, &pidfd),
        Tasks {
            what,
            read: |tid| {
                let path = task_path(pid, tid, name);
                read(&path).map(|bytes| (path, bytes))
            },
            is_empty: |(_, bytes): &(String, Vec<u8>)| bytes.is_empty(),
            stat: |tid| stats.read(tid),
        },
        Want::FirstLiveTask,
    )?;
    // Empty only when every live task is a kernel thread without the file:
    // no memory map, no environment.
    Ok(got.pop().unwrap_or_default())
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
            Ok(fd) => Ok(LinuxFd(fd)),
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
        let subject = format!("{dir:?}");
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

    fn read_head(&self, file: &LinuxFd, limit: usize) -> Result<Vec<u8>, HostError> {
        let subject = format!("{file:?}");
        let reopened = self.open_regular(file, &subject, "only marker files are read")?;
        let mut head = Vec::new();
        std::fs::File::from(reopened)
            .take(u64::try_from(limit).unwrap_or(u64::MAX))
            .read_to_end(&mut head)
            .map_err(|e| io_error("read", &subject, &e))?;
        Ok(head)
    }

    fn reopen_regular(&self, file: &LinuxFd) -> Result<LinuxFd, HostError> {
        let subject = format!("{file:?}");
        self.open_regular(file, &subject, "only lock files are reopened")
            .map(LinuxFd)
    }

    fn statvfs(&self, fd: &LinuxFd) -> Result<Space, HostError> {
        let subject = format!("{fd:?}");
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
        })
    }

    fn flock_try(&self, fd: LinuxFd, mode: LockMode) -> Result<TryLock<LinuxFd>, HostError> {
        loop {
            match rustix::fs::flock(&fd, flock_operation(mode)) {
                Ok(()) => return Ok(TryLock::Acquired(Locked::granted(fd, mode))),
                Err(rustix::io::Errno::WOULDBLOCK) => return Ok(TryLock::Busy(fd)),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(HostError::errno("flock", format!("{fd:?}"), errno_of(e))),
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
            process(proc, pid, &pidfd),
            Tasks {
                what: "fd table",
                read: |tid| task_fds(proc, pid, tid),
                is_empty: |fds: &Vec<ProcFd>| fds.is_empty(),
                stat: |tid| stats.read(tid),
            },
            Want::EveryTask,
        )?;
        // Tasks sharing an fd table list the same fds: keep each once.
        let mut seen = HashSet::new();
        let mut fds: Vec<ProcFd> = per_task
            .into_iter()
            .flatten()
            .filter(|f| seen.insert((f.fd, f.target.stat.dev_ino(), f.target.text.clone())))
            .collect();
        fds.sort_by_key(|f| f.fd);
        Ok(fds)
    }

    fn proc_links(&self, pid: Pid, which: ProcLink) -> Result<Vec<LinkTarget>, ProcError> {
        let proc = self.root();
        let pidfd = open_pidfd(pid)?;
        let mut stats = TaskStats::new(proc, pid);
        let per_task = collect(
            process(proc, pid, &pidfd),
            Tasks {
                what: which.name(),
                read: |tid| proc.link_target(&task_path(pid, tid, which.name())),
                // A link always names something; nothing is ENOENT.
                is_empty: |_: &LinkTarget| false,
                stat: |tid| stats.read(tid),
            },
            Want::EveryTask,
        )?;
        let mut seen = HashSet::new();
        Ok(per_task
            .into_iter()
            .filter(|t| seen.insert((t.stat.dev_ino(), t.text.clone())))
            .collect())
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
        Ok(parse_environ(&bytes))
    }

    /// Reads the processes on up to [`PROC_READERS`] threads (this one
    /// included), each taking the next unread pid: a scan of a desktop is
    /// dominated by multi-threaded processes whose every task's fd table is
    /// read (task-6 measured about 0.8 s for 480 processes on one thread).
    /// A helper thread that cannot be started leaves its share to the
    /// others; a reader that panics is a bug and panics here too.
    fn proc_views(&self, pids: &[Pid]) -> Vec<Result<ProcView, ProcError>> {
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
                done.push((i, read_proc_view(self, *pid)));
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

    /// `fcntl(F_SETFD)` without `FD_CLOEXEC`, on that one fd: every other fd
    /// stays close-on-exec.
    fn inherit_lock(&self, lock: &Locked<LinuxFd>) -> Result<(), HostError> {
        let fd = lock.fd();
        let subject = format!("{fd:?}");
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
            let uid_map = match proc.read(&format!("{pid}/uid_map")) {
                Ok(bytes) => String::from_utf8(bytes)
                    .map_or_else(|_| UidMap::Unreadable("not UTF-8".into()), UidMap::Map),
                Err(e) if e.is(Errno::ENOENT) => UidMap::Absent,
                Err(e) => UidMap::Unreadable(e.to_string()),
            };
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
}

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
        let subject = format!("{dir:?}");
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
    .map_err(|e| HostError::errno("open", format!("{subject}/."), errno_of(e)))
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
/// sandbox proof. `pre_exec` uses only the async-signal-safe `fchdir` syscall
/// on the already checked fd; `process_group(0)` makes the child a new group
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
    // SAFETY: the closure calls one syscall on an fd held by the parent
    // through spawn; the child inherits it until pre_exec returns. It does
    // not allocate, lock, or inspect shared state.
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
            Ok(())
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
    use diskgc_core::host::{Cause, Ids};
    use diskgc_core::resolve::{Want, resolve, resolve_in};
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
    static FD_TABLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        fn spawn(command: &mut Command, _fd_table: &std::sync::MutexGuard<'_, ()>) -> Self {
            Self {
                child: command.spawn().expect("spawn test child"),
                reaped: false,
            }
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

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("diskgc-linux-{name}-{}", std::process::id()));
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
        let tmp = TempDir::new("config-read");
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
        assert_eq!(err.reason(), "unsafe_path", "{err}");
    }

    #[test]
    fn root_statx_refuses_intermediate_symlinks_but_reports_a_final_symlink() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new("root-statx-symlink");
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
        let dir = TempDir::new("mount-point-symlink");
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
        let err = resolve_in(&host, &[fake], &link, Want::Directory)
            .expect_err("a symlink cannot be a mount point");
        assert_eq!(err.reason(), "unsafe_path", "{err}");

        let file = dir.0.join("regular");
        std::fs::write(&file, b"").expect("create regular file");
        let mut fake_file = mounts
            .iter()
            .find(|entry| entry.mount_point == Path::new("/"))
            .expect("root mount")
            .clone();
        fake_file.mount_point = file.clone();
        let err = resolve_in(&host, &[fake_file], &file, Want::Directory)
            .expect_err("a regular file cannot be a mount point");
        assert_eq!(err.reason(), "missing", "{err}");
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

    /// `read_dir` lists every name of a directory, from an `O_PATH` fd (as
    /// `resolve` returns) or a readable one, once per call (a second call
    /// is a complete listing again, not the end of the first), including a
    /// directory larger than one `getdents` buffer; a non-directory is
    /// `ENOTDIR`.
    #[test]
    fn read_dir_lists_every_name_on_each_call() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new("read-dir");
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
        let dir = TempDir::new("read-head");
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
        let dir = TempDir::new("read-head-unopened");
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
        let dir = TempDir::new("reopen-regular");
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
        let tmp = TempDir::new("read-dir-removed");
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
        let dir = TempDir::new("plain");
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
        let dir = TempDir::new("unshare");
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
    }

    /// `proc_status` shows the ids of a process and whether it is dumpable
    /// (the owner of its `fd` directory): the effective uid while it is,
    /// root (or the overflow uid) once it is not, when its fds are `Denied`
    /// to the same user. A
    /// process that has exited and was reaped is `Gone`.
    #[test]
    fn proc_status_shows_ids_and_dumpability() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new("status");
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
        let dir = TempDir::new("sockets");
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
        let bound = host.bound_unix_sockets().expect("sock_diag dump");
        assert!(
            !bound.iter().any(|s| s.is(id)),
            "{id} is still reported after the socket closed"
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
        let dir = TempDir::new("inuse");
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
            },
        )
        .expect("scan");
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
        assert_eq!(skip.reason, Reason::InUse, "{skip}");
        if scan.unreadable().iter().all(|u| u.excuse.is_some()) {
            assert!(
                skip.detail.starts_with(&format!("pid {} ", holder.pid())),
                "{skip}"
            );
            assert_eq!(skip.path, file, "{skip}");
            let free_candidate = Candidate::new(&parent, &free).expect("a name");
            let record = walk(&host, &free_candidate, &rules, None).expect("walk");
            assert_eq!(scan.check(&free_candidate, &record), Ok(()));
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
        let dir = TempDir::new("exec-loop");
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
        let dir = TempDir::new("fake-listing-reuse");
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
        let out = {
            let _fd_table = fd_table_guard();
            parent.output().unwrap_or_else(|e| {
                panic!("start a process in a new user namespace: {e}; {USERNS_NEEDED}")
            })
        };
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "the namespace helper failed ({}; its own panic message is above); \
             {USERNS_NEEDED}. It printed: {said}",
            out.status
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
        let dir = TempDir::new("fake-proc");
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
        let other = TempDir::new("fake-proc-ancestor");
        fake_proc(&other.0, &format!("1\t{}", std::process::id()));
        let refused = LinuxHost::with_proc(open_proc_root(&other.0)).unwrap_err();
        assert!(
            refused.to_string().contains("another pid namespace"),
            "{refused}"
        );
    }

    /// The old and new tasks deliberately have the same clock-tick start
    /// time. A directory fd must keep the first incarnation's inode even
    /// after its numeric name is reused; when that inode goes away its
    /// `stat` must fail instead of switching to the replacement.
    #[test]
    fn task_stat_pin_does_not_follow_a_reused_tid_within_one_tick() {
        let dir = TempDir::new("fake-task-reuse");
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

        std::fs::rename(task_dir.join("old-task/stat"), task_dir.join("old-stat"))
            .expect("simulate old task release");
        assert!(
            stats.read(4243).is_err_and(|e| is_vanished(&e)),
            "pinned stat must disappear with its incarnation"
        );
    }

    /// Procfs must stop resolving `stat` through an already open task
    /// directory when that particular thread has exited. This is the kernel
    /// fact the same-tick reuse check relies on, exercised without needing
    /// to force a numeric tid recycle on the host.
    #[test]
    fn held_task_directory_stat_disappears_after_thread_exit() {
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
            "a held task directory must not remain readable after thread exit: {after:?}"
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
                if !defined && !matches!(file.enclosing_fn(t.pos), Some("new" | "shown")) {
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
        let tmp = TempDir::new("sandbox-gate");
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
        let tmp = TempDir::new("engine");
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
        let dir = TempDir::new("flock");
        // The holder takes its lock only after exec, in its own fd table.
        // A parallel test's fork cannot inherit this fd and keep it alive
        // after the holder exits. This removes one possible cause of the
        // rare failure recorded in task-64 #37; its cause was not observed.
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
        assert!(
            matches!(
                host.flock_wait(second, LockMode::Exclusive, deadline),
                Ok(WaitLock::Acquired(_))
            ),
            "closing the holder's fd released the lock"
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
        let dir = TempDir::new("convert");
        let open = || std::fs::File::open(&dir.0).expect("open");
        let (a, b, probe) = (open(), open(), open());
        rustix::fs::flock(&a, Op::NonBlockingLockShared).expect("a shared");
        rustix::fs::flock(&b, Op::NonBlockingLockShared).expect("b shared");
        assert!(rustix::fs::flock(&a, Op::NonBlockingLockExclusive).is_err());
        drop(b);
        // a's shared lock is gone: an exclusive lock from elsewhere succeeds.
        assert!(
            rustix::fs::flock(&probe, Op::NonBlockingLockExclusive).is_ok(),
            "the kernel kept the shared lock; the hazard the port guards against is gone"
        );
    }

    /// The port's version of the same sequence: the shared lock cannot be
    /// converted, so it is still held after another holder leaves.
    #[test]
    fn a_held_shared_lock_stays_held() {
        let _fd_table = fd_table_guard();
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new("shared");
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
        assert!(matches!(lock(LockMode::Exclusive), TryLock::Acquired(_)));
    }

    /// The readers on a real process larger than every first buffer: an
    /// environment over 64 KiB (`environ` re-read with a larger buffer, in
    /// one call), more fds than one `getdents` buffer holds (listing re-made
    /// with a larger buffer), and thousands of mappings (`maps` over many
    /// `read` calls, in two passes). The complete contents must come back.
    #[test]
    fn a_big_process_is_read_completely() {
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new("big");
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
}
