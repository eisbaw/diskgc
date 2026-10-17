//! `LinuxHost`: the [`Host`] port on the running kernel.
//!
//! `openat2`, `statx`, `fstatvfs`, `flock`, `uname` and `clock_gettime` go
//! through rustix's `linux_raw` backend, i.e. raw syscalls without a libc
//! wrapper, so the static musl build behaves exactly like the gnu build
//! (and needs no libc support for `openat2` or `statx`).
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

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::time::Duration;

use diskgc_core::host::{
    Access, Base, Environ, Errno, Host, HostError, LinkTarget, LockMode, Locked, MapEntry,
    MonoTime, MountEntry, OpenHow, Pid, ProcError, ProcFd, ProcLink, Space, Stat, TryLock,
    WaitLock,
};
use rustix::event::PollFlags;
use rustix::fs::{AtFlags, CWD, FlockOperation, Mode, OFlags, ResolveFlags};

use crate::mountinfo::parse_mountinfo;
use crate::procfs::{
    StatusFields, list_in_one_call, parse_environ, parse_maps, parse_status, read_confirmed,
    read_in_one_call, read_two_passes,
};
use crate::stat::{STATX_MASK, stat_from_statx};
use crate::tasks::{
    Listed, Listing, PidfdPoll, Polled, Process, StatusRead, TaskStat, Tasks, Want, collect,
    is_vanished, parse_task_stat,
};

/// Where procfs is expected. Named only where [`LinuxHost::new`] opens it
/// and in messages ([`ProcRoot::shown`]); every access goes through the
/// held fd (checked by the test `procfs_is_reached_only_through_the_held_fd`).
const PROC: &str = "/proc";

/// First and longest pause between attempts while waiting for a lock.
const LOCK_POLL_FIRST: Duration = Duration::from_millis(10);
const LOCK_POLL_MAX: Duration = Duration::from_millis(100);

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
}

impl LinuxHost {
    /// Checks, failing closed, that the `/proc` readers can work here:
    ///
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
        Ok(Self { proc })
    }

    /// The procfs mount every reader goes through.
    fn root(&self) -> ProcRoot<'_> {
        ProcRoot(self.proc.as_fd())
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

    /// The names in directory `rel`, without `.` and `..`.
    fn names(self, rel: &str) -> Result<Vec<Vec<u8>>, HostError> {
        let fd = self.open(rel, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let failed = |e| HostError::errno("getdents", Self::shown(rel), errno_of(e));
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(&fd).map_err(failed)? {
            let entry = entry.map_err(failed)?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                names.push(name.to_vec());
            }
        }
        Ok(names)
    }

    /// A magic link: its text and the object it refers to (`statx`
    /// following it).
    fn link_target(self, rel: &str) -> Result<LinkTarget, HostError> {
        let stat = rustix::fs::statx(self.0, rel, AtFlags::empty(), STATX_MASK)
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
        Access::Read => OFlags::RDONLY,
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

/// One [`Listing`]: the tasks (`list`), the thread count read right after
/// them (`count`), and the tasks listed again right after the count. `None`
/// as soon as one of them vanished.
fn task_listing(
    mut list: impl FnMut() -> Result<Option<Vec<u32>>, ProcError>,
    count: impl FnOnce() -> Result<Option<usize>, ProcError>,
) -> Result<Option<Listing>, ProcError> {
    let Some(tids) = list()? else {
        return Ok(None);
    };
    let Some(threads) = count()? else {
        return Ok(None);
    };
    let Some(again) = list()? else {
        return Ok(None);
    };
    Ok(Some(Listing::new(tids, threads, &again)))
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

/// Polls `pidfd` once, without waiting.
fn poll_pidfd(pid: Pid, pidfd: &OwnedFd) -> Result<PidfdPoll, ProcError> {
    let mut fds = [rustix::event::PollFd::new(pidfd, PollFlags::IN)];
    let now = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    rustix::event::poll(&mut fds, Some(&now))
        .map_err(|e| ProcError::Failed(HostError::errno("poll", pid, errno_of(e))))?;
    pidfd_poll_answer(pid, fds[0].revents())
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
    Process {
        pid: pid.0,
        list: move || {
            task_listing(
                || task_ids(proc, pid),
                || Ok(group_status(proc, pid)?.map(|status| status.threads)),
            )
        },
        poll: move || poll_pidfd(pid, pidfd),
        status: move || group_status(proc, pid),
    }
}

fn read_task_stat(proc: ProcRoot<'_>, pid: Pid, tid: u32) -> Result<TaskStat, HostError> {
    let path = task_path(pid, tid, "stat");
    let text = proc.read(&path)?;
    parse_task_stat(&text).map_err(|e| HostError::invalid("parse", ProcRoot::shown(&path), e))
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
    let mut got = collect(
        process(proc, pid, &pidfd),
        Tasks {
            what,
            read: |tid| {
                let path = task_path(pid, tid, name);
                read(&path).map(|bytes| (path, bytes))
            },
            is_empty: |(_, bytes): &(String, Vec<u8>)| bytes.is_empty(),
            stat: |tid| read_task_stat(proc, pid, tid),
        },
        Want::FirstLiveTask,
    )?;
    // Empty only when every live task is a kernel thread without the file:
    // no memory map, no environment.
    Ok(got.pop().unwrap_or_default())
}

impl Host for LinuxHost {
    type Fd = LinuxFd;

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
        rustix::fs::openat2(dir, path, oflags(how), Mode::empty(), resolve_flags(how))
            .map(LinuxFd)
            .map_err(|e| HostError::errno("openat2", shown(&base, path), errno_of(e)))
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
            Base::Root => (CWD, AtFlags::SYMLINK_NOFOLLOW),
            Base::Dir(fd) if empty => (fd.as_fd(), AtFlags::SYMLINK_NOFOLLOW | AtFlags::EMPTY_PATH),
            Base::Dir(fd) => (fd.as_fd(), AtFlags::SYMLINK_NOFOLLOW),
        };
        rustix::fs::statx(dir, path, flags, STATX_MASK)
            .map(|x| stat_from_statx(&x))
            .map_err(|e| HostError::errno("statx", shown(&base, path), errno_of(e)))
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
            fd = match self.flock_try(fd, mode)? {
                TryLock::Acquired(locked) => return Ok(WaitLock::Acquired(locked)),
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

    fn now(&self) -> MonoTime {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
        // CLOCK_BOOTTIME is never negative and tv_nsec is below 10^9.
        MonoTime(Duration::new(
            u64::try_from(t.tv_sec).expect("CLOCK_BOOTTIME seconds are non-negative"),
            u32::try_from(t.tv_nsec).expect("tv_nsec is below one second"),
        ))
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

    fn pids(&self) -> Result<Vec<Pid>, HostError> {
        proc_pids(self.root())
    }

    fn proc_fds(&self, pid: Pid) -> Result<Vec<ProcFd>, ProcError> {
        let proc = self.root();
        let pidfd = open_pidfd(pid)?;
        let per_task = collect(
            process(proc, pid, &pidfd),
            Tasks {
                what: "fd table",
                read: |tid| task_fds(proc, pid, tid),
                is_empty: |fds: &Vec<ProcFd>| fds.is_empty(),
                stat: |tid| read_task_stat(proc, pid, tid),
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
        let per_task = collect(
            process(proc, pid, &pidfd),
            Tasks {
                what: which.name(),
                read: |tid| proc.link_target(&task_path(pid, tid, which.name())),
                // A link always names something; nothing is ENOENT.
                is_empty: |_: &LinkTarget| false,
                stat: |tid| read_task_stat(proc, pid, tid),
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
}

#[cfg(test)]
mod tests {
    //! Checks against the machine the tests run on (dev shell or Nix build
    //! sandbox): nothing here creates, changes or deletes files outside the
    //! test's own temp directory. Mount crossings, bind mounts, other users
    //! and a main thread that exits need the container tests (diskgc-e2e).

    use super::*;
    use diskgc_core::resolve::{Want, resolve};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, ChildStdout, Command, Stdio};
    use std::time::Instant;

    /// Serialises the tests that copy this process's fd table (spawning a
    /// child: `Holder::spawn`, `a_process_that_is_gone_is_gone`; any test
    /// that forks or unshares `CLONE_FILES` in this process) with the tests
    /// that judge `flock` locks: a copied table holds an extra reference to
    /// a lock fd that a lock test just closed, so the lock would seem not to
    /// be released. (The holder's own `unshare` happens in its process.)
    static FD_TABLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fd_table_guard() -> std::sync::MutexGuard<'static, ()> {
        // A panicking test poisons the mutex; the next one may still run.
        FD_TABLE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        child: Child,
        stdout: BufReader<ChildStdout>,
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
                Command::new(std::env::current_exe().expect("own executable"))
                    .args(HOLDER_ARGS)
                    .env(HOLDER_MODE, mode)
                    .env(HOLDER_FILE, file)
                    .env(HOLDER_DIR, dir)
                    .env(name, value)
                    .envs(env.iter().copied())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    // Inherited, so a holder that panics says why.
                    .stderr(Stdio::inherit())
                    .spawn()
                    .expect("spawn holder")
            };
            let mut stdout = BufReader::new(child.stdout.take().expect("holder stdout"));
            let mut line = String::new();
            while line.trim() != "ready" {
                line.clear();
                let n = stdout.read_line(&mut line).expect("read holder");
                assert!(n > 0, "the holder ended before it was ready");
            }
            Self { child, stdout }
        }

        fn pid(&self) -> Pid {
            Pid(self.child.id())
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            drop(self.child.stdin.take());
            let mut rest = String::new();
            let _ = self.stdout.read_to_string(&mut rest);
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
        let hold = move || {
            let file = PathBuf::from(std::env::var_os(HOLDER_FILE).expect("holder file"));
            let dir = PathBuf::from(std::env::var_os(HOLDER_DIR).expect("holder dir"));
            let held = std::fs::File::open(&file).expect("open held file");
            std::env::set_current_dir(&dir).expect("chdir");
            let mut out = std::io::stdout();
            out.write_all(b"ready\n").expect("write");
            out.flush().expect("flush");
            let mut sink = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut sink);
            drop(held);
        };
        match mode.as_str() {
            "plain" => hold(),
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
    fn statvfs_reports_available_bytes() {
        let host = LinuxHost::new().expect("host setup");
        let root = resolve(&host, Path::new("/"), Want::Directory).expect("resolve /");
        let space = host.statvfs(&root.fd).expect("statvfs");
        assert!(space.avail_bytes <= space.size_bytes, "{space:?}");
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

    #[test]
    fn a_process_that_is_gone_is_gone() {
        let _fd_table = fd_table_guard();
        let host = LinuxHost::new().expect("host setup");
        let mut child = std::process::Command::new("/proc/self/exe")
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn");
        let pid = Pid(child.id());
        child.wait().expect("wait");
        assert_eq!(host.proc_maps(pid), Err(ProcError::Gone));
        assert_eq!(host.proc_fds(pid), Err(ProcError::Gone));
        assert_eq!(host.proc_links(pid, ProcLink::Cwd), Err(ProcError::Gone));
        assert_eq!(host.proc_environ(pid), Err(ProcError::Gone));
    }

    /// A zombie child, not yet reaped: `pidfd_poll` says readable and its
    /// `status` shows it alone in its group, so it is Gone. (Pins the
    /// `POLLIN` branch: answering "running" there makes this `Failed`.) Once
    /// reaped, a pidfd opened before says `POLLHUP` (Linux 6.9 and later).
    #[test]
    fn an_unreaped_zombie_is_gone() {
        let host = LinuxHost::new().expect("host setup");
        let mut child = {
            let _fd_table = fd_table_guard();
            Command::new("/proc/self/exe")
                .arg("--help")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn")
        };
        let pid = Pid(child.id());
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
        assert_eq!(poll_pidfd(pid, &pidfd), Ok(PidfdPoll::Readable));
        assert_eq!(host.proc_fds(pid), Err(ProcError::Gone));
        assert_eq!(host.proc_links(pid, ProcLink::Cwd), Err(ProcError::Gone));
        assert_eq!(host.proc_maps(pid), Err(ProcError::Gone));
        assert_eq!(host.proc_environ(pid), Err(ProcError::Gone));
        child.wait().expect("reap");
        let release = host.kernel_release().expect("release");
        let version = diskgc_core::kernel::parse_release(&release).expect("version");
        let hangup_since = diskgc_core::kernel::KernelVersion { major: 6, minor: 9 };
        let after_reaping = poll_pidfd(pid, &pidfd);
        if version >= hangup_since {
            assert_eq!(after_reaping, Ok(PidfdPoll::Hangup), "{release}");
        } else {
            assert_eq!(after_reaping, Ok(PidfdPoll::Readable), "{release}");
        }
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
        );
        assert_eq!(got, Ok(Some(Listing::new(vec![10, 11], 2, &[10]))));
        assert_ne!(got, Ok(Some(Listing::new(vec![10, 11], 2, &[10, 11]))));
        assert_eq!(*calls.borrow(), ["list", "count", "list"]);
        // A step that vanished: no listing, and no step after it.
        let got = task_listing(|| Ok(Some(vec![10])), || Ok(None));
        assert_eq!(got, Ok(None));
        let mut listings = vec![Some(vec![10]), None].into_iter();
        let got = task_listing(|| Ok(listings.next().flatten()), || Ok(Some(1)));
        assert_eq!(got, Ok(None));
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
                &format!("{tid} (x) S 1 1 1 0 -1 4194560 0"),
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
        assert_eq!(task_ids(proc, pid), Ok(Some(vec![4242, 4243])));
        assert_eq!(
            group_status(proc, pid),
            Ok(Some(StatusFields {
                state: 'S',
                threads: 2
            }))
        );
        let stat = read_task_stat(proc, pid, 4243).expect("stat");
        assert_eq!(stat.state, 'S');
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

    /// The grep check: outside its tests, the crate never names `/proc` as a
    /// path except where [`LinuxHost::new`] opens it and in messages
    /// ([`ProcRoot::shown`]), so nothing bypasses the held fd.
    #[test]
    fn procfs_is_reached_only_through_the_held_fd() {
        let sources = [
            ("host.rs", include_str!("host.rs")),
            ("tasks.rs", include_str!("tasks.rs")),
            ("procfs.rs", include_str!("procfs.rs")),
            ("mountinfo.rs", include_str!("mountinfo.rs")),
            ("stat.rs", include_str!("stat.rs")),
            ("sandbox.rs", include_str!("sandbox.rs")),
            ("lib.rs", include_str!("lib.rs")),
        ];
        let literal = ["\"", "/proc"].concat();
        let mut found = Vec::new();
        for (name, source) in sources {
            let code = source.split("#[cfg(test)]").next().expect("code");
            let mut function = "";
            for (n, line) in code.lines().enumerate() {
                let line = line.trim_start();
                if line.starts_with("//") {
                    continue;
                }
                if let Some(rest) = line.split("fn ").nth(1)
                    && (line.starts_with("fn ") || line.contains(" fn "))
                {
                    function = rest.split(['(', '<']).next().unwrap_or("");
                }
                let named_constant = line.starts_with("const PROC: &str =");
                if line.contains(&literal) && !named_constant {
                    found.push(format!("{name}:{}: {line}", n + 1));
                }
                let uses_proc = line
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|word| word == "PROC");
                if uses_proc && !named_constant && !matches!(function, "new" | "shown") {
                    found.push(format!("{name}:{} (in fn {function}): {line}", n + 1));
                }
            }
        }
        assert!(
            found.is_empty(),
            "procfs reached by path:\n{}",
            found.join("\n")
        );
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
        let _fd_table = fd_table_guard();
        let host = LinuxHost::new().expect("host setup");
        let dir = TempDir::new("flock");
        let first = match host.flock_try(open_dir(&host, &dir.0), LockMode::Exclusive) {
            Ok(TryLock::Acquired(locked)) => locked,
            other => panic!("first lock: {other:?}"),
        };
        assert_eq!(first.mode(), LockMode::Exclusive);
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
        drop(first);
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
