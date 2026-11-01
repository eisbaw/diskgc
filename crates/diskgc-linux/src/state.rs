//! diskgc's own state files (docs/architecture.md, The Host port: diskgc's
//! own state): the index snapshot `index-<fs>` and its single-writer lock
//! `index-<fs>.lock`, in the state directory `LinuxHost` was given at
//! construction (`$XDG_STATE_HOME/diskgc`, decided by the binary). Names
//! come only from a typed [`FsKey`], never from a caller.
//!
//! The per-user reclaim lock (docs/safety.md, Host lock) lives here too:
//! `lock` in the per-user lock directory `LinuxHost` was given
//! (`$XDG_RUNTIME_DIR/diskgc`, otherwise the state directory).
//!
//! The functions here that create or rename anything, [`make_dirs_ungated`],
//! [`open_lock_file_ungated`], [`open_user_lock_ungated`] and
//! [`write_snapshot_ungated`], are the only ungated writes in production
//! code: each is an entry of the source
//! scan's allowlist with its reason and exact match count
//! (`source_scan::GATED_ALLOWED`; docs/safety.md, Pre-release sandbox
//! gate). They are not behind the sandbox gate: they write diskgc's cache,
//! never user data, and `watch --tally-only` must work anywhere.

use std::ffi::OsStr;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use diskgc_core::host::{
    Dev, FileType, FsKey, HostError, MakeUserLock, MountEntry, StateFile, StateWritten,
    UserLockError,
};
use diskgc_core::hostlock::USER_LOCK_NAME;
use rustix::fs::{AtFlags, Mode, OFlags};

use crate::host::{errno_of, io_error};

/// Mode of the state directory diskgc creates.
const DIR_MODE: u32 = 0o700;

/// Mode of the files diskgc creates there (the umask can only narrow it).
const FILE_MODE: u32 = 0o600;

/// The in-kernel network filesystems (those of
/// `diskgc_core::places::space::NETWORK_FS_TYPES` the kernel itself
/// mounts), by `statfs` magic (`include/uapi/linux/magic.h`): NFS, SMB
/// (`cifs` and `smb3`), Ceph, 9p, AFS (both), Coda. Their `flock` is a lock
/// on the server, and an NFS client's an emulated byte-range lock whose
/// exclusive form needs a descriptor open for writing (flock(2); the
/// kernel's `F_WRLCK` check, `check_fmode_for_setlk`, gives `EBADF`).
const NETWORK_FS_MAGICS: &[u64] = &[
    0x6969,      // NFS_SUPER_MAGIC
    0xFF53_4D42, // CIFS_SUPER_MAGIC
    0xFE53_4D42, // SMB2_SUPER_MAGIC
    0x00C3_6400, // CEPH_SUPER_MAGIC
    0x0102_1997, // V9FS_MAGIC
    0x5346_414F, // AFS_SUPER_MAGIC
    0x6B41_4653, // AFS_FS_MAGIC
    0x7375_7245, // CODA_SUPER_MAGIC
];

/// Whether a lock file on a filesystem of `statfs` magic `f_type` is
/// reopened for writing ([`reopen_for_flock`]): only on a network
/// filesystem ([`NETWORK_FS_MAGICS`]). Anywhere else a read-only
/// descriptor is enough for `flock`, and a writable open is not harmless:
/// on overlayfs it copies a lower file up (a write, TASK-288 review).
pub(crate) fn flock_needs_write(f_type: u64) -> bool {
    NETWORK_FS_MAGICS.contains(&f_type)
}

/// Reopens the lock file `checked` (an `O_PATH` descriptor of the object
/// its caller inspected) through `/proc/self/fd`, for `flock` only, never
/// truncated and never written: for writing on a network filesystem
/// ([`flock_needs_write`]; docs/safety.md, Host lock), read-only elsewhere.
/// Where the user may not write it (`EACCES`: a lock file made read-only;
/// `EPERM`: `chattr +i` or `+a`; `EROFS`), read-only too, as before
/// TASK-288: an exclusive lock there then fails on NFS (exit 3), and works
/// on the other network types that do not check the mode.
fn reopen_for_flock(proc: BorrowedFd<'_>, checked: BorrowedFd<'_>) -> rustix::io::Result<OwnedFd> {
    let f_type = crate::host::fs_magic_of(i128::from(rustix::fs::fstatfs(checked)?.f_type));
    reopen_lock(proc, checked, f_type.is_some_and(flock_needs_write))
}

/// [`reopen_for_flock`] once it is decided whether to try writing.
pub(crate) fn reopen_lock(
    proc: BorrowedFd<'_>,
    checked: BorrowedFd<'_>,
    write: bool,
) -> rustix::io::Result<OwnedFd> {
    let through = format!("self/fd/{}", checked.as_raw_fd());
    let open = |access: OFlags| {
        rustix::fs::openat(
            proc,
            through.as_str(),
            access | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
    };
    if !write {
        return open(OFlags::RDONLY);
    }
    match open(OFlags::RDWR) {
        Err(rustix::io::Errno::ACCESS | rustix::io::Errno::PERM | rustix::io::Errno::ROFS) => {
            open(OFlags::RDONLY)
        }
        other => other,
    }
}

fn file_type_of(st_mode: u32) -> FileType {
    FileType::from_mode(st_mode)
}

/// The state directory, open, checked: a directory owned by this user and
/// writable by no one else (a directory others can write to could have the
/// snapshot replaced under diskgc). Symlinks on the way to it are followed,
/// as for the configuration file: it is the user's own home layout.
/// `ENOENT`: it does not exist.
pub(crate) fn open_state_dir(dir: &Path) -> Result<OwnedFd, HostError> {
    let shown = dir.display().to_string();
    let fd = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| HostError::errno("open", &shown, errno_of(e)))?;
    check_state_dir(fd.as_fd(), &shown)?;
    Ok(fd)
}

/// The state directory open as `fd` (at `shown`) is still one diskgc may
/// use: owned by this user and writable by no one else.
fn check_state_dir(fd: BorrowedFd<'_>, shown: &str) -> Result<(), HostError> {
    let st = rustix::fs::fstat(fd).map_err(|e| HostError::errno("fstat", shown, errno_of(e)))?;
    let uid = rustix::process::geteuid().as_raw();
    if st.st_uid != uid || st.st_mode & 0o022 != 0 {
        return Err(HostError::invalid(
            "open",
            shown,
            format!(
                "the state directory must be owned by uid {uid} and writable by no one else \
                 (owner {}, mode {:04o})",
                st.st_uid,
                st.st_mode & 0o7777
            ),
        ));
    }
    Ok(())
}

/// Opens (creating it, and the state directory, if absent) the
/// single-writer lock file of `key`, for `flock` only: nothing is ever
/// written to it or renamed onto it (docs/architecture.md, Single writer).
/// The directory is created mode 0700 with its missing parents. The file
/// is inspected through an `O_PATH | O_NOFOLLOW` open before anything opens
/// it for reading or writing: it must be a regular file of this user (a
/// symlink, a FIFO or a device there is refused without being opened);
/// missing, it is made mode 0600 with `O_EXCL` (never over something
/// else; one made meanwhile by another run is inspected like any). The
/// checked object is then reopened ([`reopen_for_flock`]: read-only but on
/// a network filesystem) and checked to be the same object. Returns the
/// lock file's fd and path, and the state directory's fd: what the
/// snapshot is written into, and so what its space guard samples and
/// places (never the lock file's mount: a file can be bind mounted over
/// the lock file).
pub(crate) fn open_lock_file_ungated(
    dir: &Path,
    key: &FsKey,
    proc: BorrowedFd<'_>,
) -> Result<(OwnedFd, PathBuf, OwnedFd), HostError> {
    let dir_fd = match open_state_dir(dir) {
        Err(e) if e.is(diskgc_core::host::Errno::ENOENT) => {
            make_dirs_ungated(dir)?;
            open_state_dir(dir)?
        }
        other => other?,
    };
    let name = key.lock_name();
    let path = dir.join(&name);
    let shown = path.display().to_string();
    let failed = |op: &'static str, e| HostError::errno(op, &shown, errno_of(e));
    let mut made = false;
    let (file, st) = loop {
        match inspect_name(dir_fd.as_fd(), OsStr::new(name.as_str())) {
            Ok(Some(found)) => break found,
            Ok(None) if made => {
                return Err(HostError::invalid(
                    "open",
                    &shown,
                    "the lock file vanished right after it was made",
                ));
            }
            Ok(None) => {
                match rustix::fs::openat(
                    &dir_fd,
                    name.as_str(),
                    OFlags::RDONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::NONBLOCK
                        | OFlags::NOCTTY
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(FILE_MODE),
                ) {
                    // Made: inspected like one found, above.
                    Ok(fd) => drop(fd),
                    Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(failed("open", e)),
                }
                made = true;
            }
            Err((op, e)) => return Err(failed(op, e)),
        }
    };
    let uid = rustix::process::geteuid().as_raw();
    if file_type_of(st.st_mode) != FileType::Regular || st.st_uid != uid {
        return Err(HostError::invalid(
            "open",
            &shown,
            format!(
                "the lock file must be a regular file of uid {uid} (a {}, owner {})",
                file_type_of(st.st_mode),
                st.st_uid
            ),
        ));
    }
    let fd = reopen_for_flock(proc, file.as_fd()).map_err(|e| failed("open", e))?;
    let opened = rustix::fs::fstat(&fd).map_err(|e| failed("fstat", e))?;
    if (opened.st_dev, opened.st_ino) != (st.st_dev, st.st_ino) {
        return Err(HostError::invalid(
            "open",
            &shown,
            "the lock file changed between checking it and opening it",
        ));
    }
    Ok((fd, path, dir_fd))
}

/// `mkdir -p dir`: each missing directory mode 0700 (the umask can only
/// narrow it); one made meanwhile by another run is fine. Only for the
/// directories of diskgc's own state ([`open_lock_file_ungated`],
/// [`open_user_lock_ungated`]); symlinks on the way are followed, as for
/// the state directory, and the callers check what they open.
fn make_dirs_ungated(dir: &Path) -> Result<(), HostError> {
    let mut at = PathBuf::new();
    for component in dir.components() {
        at.push(component);
        match rustix::fs::mkdir(&at, Mode::from_raw_mode(DIR_MODE)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(HostError::errno("mkdir", at.display(), errno_of(e))),
        }
    }
    Ok(())
}

/// What [`open_user_lock_ungated`] found at one name, through an `O_PATH`
/// open that never follows a symlink.
enum Inspected {
    Absent,
    /// The object, its `fstat`.
    Present(OwnedFd, rustix::fs::Stat),
}

/// `name` beneath `dir`, inspected without opening it for reading: an
/// `O_PATH | O_NOFOLLOW` open (a symlink is opened as itself) and `fstat`.
/// Needs search permission on `dir` only, so another user's object is
/// inspected and refused, never an `EACCES` (TASK-277 review, Codex 1).
/// `None`: absent; an error names its operation.
fn inspect_name(
    dir: BorrowedFd<'_>,
    name: &OsStr,
) -> Result<Option<(OwnedFd, rustix::fs::Stat)>, (&'static str, rustix::io::Errno)> {
    match rustix::fs::openat(
        dir,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let st = rustix::fs::fstat(&fd).map_err(|e| ("fstat", e))?;
            Ok(Some((fd, st)))
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(e) => Err(("open", e)),
    }
}

/// [`inspect_name`] for [`open_user_lock_ungated`], its failure a
/// [`UserLockError::Failed`] at `shown`.
fn inspect(dir: BorrowedFd<'_>, name: &OsStr, shown: &Path) -> Result<Inspected, UserLockError> {
    match inspect_name(dir, name) {
        Ok(Some((fd, st))) => Ok(Inspected::Present(fd, st)),
        Ok(None) => Ok(Inspected::Absent),
        Err((op, e)) => Err(UserLockError::Failed {
            path: shown.to_path_buf(),
            error: HostError::errno(op, shown.display(), errno_of(e)),
        }),
    }
}

/// Opens the per-user reclaim lock `dir/lock` (docs/safety.md, Host lock)
/// for `flock` only: nothing is ever written to it or renamed onto it. `dir` is the per-user lock directory `LinuxHost` found
/// (`/run/user/<uid>/diskgc` or `<home>/.local/state/diskgc`). Its parent
/// is reached following symlinks (made, mode 0700 with its missing
/// parents, when `make` allows). The directory and the file are inspected
/// before anything opens them for reading ([`inspect`]): the directory must
/// be a directory of this user that no one else may write to, the file a
/// regular file of this user that no one else may open (`mode & 0o077`:
/// they could hold the lock); anything else, whoever owns it, is
/// [`UserLockError::Unsafe`]. Then the checked file is reopened
/// ([`reopen_for_flock`]: read-only but on a network filesystem) and
/// checked to be the same object. What is
/// missing is made when `make` is [`MakeUserLock::IfAbsent`] (the directory
/// mode 0700, the file mode 0600 with `O_EXCL`, so never over something
/// else; a name made meanwhile by another run is inspected like any), and
/// is `Ok(None)` otherwise. A kernel failure is [`UserLockError::Failed`].
pub(crate) fn open_user_lock_ungated(
    dir: &Path,
    make: MakeUserLock,
    proc: BorrowedFd<'_>,
) -> Result<Option<(OwnedFd, PathBuf)>, UserLockError> {
    let path = dir.join(USER_LOCK_NAME);
    let failed = |op, at: &Path, e| UserLockError::Failed {
        path: path.clone(),
        error: HostError::errno(op, at.display(), errno_of(e)),
    };
    let unsafe_ = |at: &Path, why: String| UserLockError::Unsafe {
        path: path.clone(),
        error: HostError::invalid("open", at.display(), why),
    };
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(unsafe_(
            dir,
            "the per-user lock directory has no parent".into(),
        ));
    };
    let open_parent = || {
        rustix::fs::open(
            parent,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
    };
    let parent_fd = match open_parent() {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) if make == MakeUserLock::Never => return Ok(None),
        Err(rustix::io::Errno::NOENT) => {
            make_dirs_ungated(parent).map_err(|error| UserLockError::Failed {
                path: path.clone(),
                error,
            })?;
            open_parent().map_err(|e| failed("open", parent, e))?
        }
        Err(e) => return Err(failed("open", parent, e)),
    };
    let uid = rustix::process::geteuid().as_raw();
    // The directory: inspected; made if absent and allowed, then inspected.
    let mut made = false;
    let (dir_fd, st) = loop {
        match inspect(parent_fd.as_fd(), name, dir)? {
            Inspected::Present(fd, st) => break (fd, st),
            Inspected::Absent if make == MakeUserLock::Never || made => return Ok(None),
            Inspected::Absent => {
                match rustix::fs::mkdirat(&parent_fd, name, Mode::from_raw_mode(DIR_MODE)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => made = true,
                    Err(e) => return Err(failed("mkdir", dir, e)),
                }
            }
        }
    };
    match file_type_of(st.st_mode) {
        FileType::Directory => {}
        FileType::Symlink => {
            return Err(unsafe_(
                dir,
                "the per-user lock directory is a symlink".into(),
            ));
        }
        other => {
            return Err(unsafe_(
                dir,
                format!("the per-user lock directory is not a directory (a {other})"),
            ));
        }
    }
    if st.st_uid != uid || st.st_mode & 0o022 != 0 {
        return Err(unsafe_(
            dir,
            format!(
                "the per-user lock directory must be owned by uid {uid} and writable by no \
                 one else (owner {}, mode {:04o})",
                st.st_uid,
                st.st_mode & 0o7777
            ),
        ));
    }
    // The file: inspected; made exclusively if absent and allowed.
    let mut made = false;
    let (file, st) = loop {
        match inspect(dir_fd.as_fd(), OsStr::new(USER_LOCK_NAME), &path)? {
            Inspected::Present(fd, st) => break (fd, st),
            Inspected::Absent if make == MakeUserLock::Never || made => return Ok(None),
            Inspected::Absent => {
                match rustix::fs::openat(
                    &dir_fd,
                    USER_LOCK_NAME,
                    OFlags::RDONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::NONBLOCK
                        | OFlags::NOCTTY
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(FILE_MODE),
                ) {
                    // Made: inspected like one found, below.
                    Ok(fd) => drop(fd),
                    Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(failed("open", &path, e)),
                }
                made = true;
            }
        }
    };
    let file_type = file_type_of(st.st_mode);
    if file_type == FileType::Symlink {
        return Err(unsafe_(&path, "the per-user lock is a symlink".into()));
    }
    if file_type != FileType::Regular || st.st_uid != uid {
        return Err(unsafe_(
            &path,
            format!(
                "the per-user lock must be a regular file of uid {uid} (a {file_type}, owner {})",
                st.st_uid
            ),
        ));
    }
    if st.st_mode & 0o077 != 0 {
        return Err(unsafe_(
            &path,
            format!(
                "no one else may open the per-user lock, or they could hold it (mode {:04o}; \
                 chmod 0600 it)",
                st.st_mode & 0o7777
            ),
        ));
    }
    // Reopened through the checked object, never by name again.
    let lockable = reopen_for_flock(proc, file.as_fd()).map_err(|e| failed("open", &path, e))?;
    let opened = rustix::fs::fstat(&lockable).map_err(|e| failed("fstat", &path, e))?;
    if (opened.st_dev, opened.st_ino) != (st.st_dev, st.st_ino) {
        return Err(unsafe_(
            &path,
            "the per-user lock changed between checking it and opening it".into(),
        ));
    }
    Ok(Some((lockable, path)))
}

/// What is at `index-<key>` in the state directory, never following a
/// symlink, with its contents if it is a regular file of at most `limit`
/// bytes. `None`: no state directory, or no such file.
pub(crate) fn read_snapshot(
    dir: &Path,
    key: &FsKey,
    limit: u64,
    proc: BorrowedFd<'_>,
) -> Result<Option<StateFile>, HostError> {
    let dir_fd = match open_state_dir(dir) {
        Ok(fd) => fd,
        Err(e) if e.is(diskgc_core::host::Errno::ENOENT) => return Ok(None),
        Err(e) => return Err(e),
    };
    let name = key.snapshot_name();
    let shown = dir.join(&name).display().to_string();
    let fd = match rustix::fs::openat(
        &dir_fd,
        name.as_str(),
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(HostError::errno("open", &shown, errno_of(e))),
    };
    let st = rustix::fs::fstat(&fd).map_err(|e| HostError::errno("fstat", &shown, errno_of(e)))?;
    let file_type = file_type_of(st.st_mode);
    let size = u64::try_from(st.st_size).unwrap_or(0);
    let mut file = StateFile {
        file_type,
        uid: st.st_uid,
        mode: st.st_mode & 0o7777,
        size,
        bytes: None,
    };
    if file_type != FileType::Regular || size > limit {
        return Ok(Some(file));
    }
    // Reopened for reading through the checked object, never by name again
    // (a FIFO or device swapped in meanwhile is never opened).
    let readable = rustix::fs::openat(
        proc,
        format!("self/fd/{}", fd.as_raw_fd()).as_str(),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| HostError::errno("open", &shown, errno_of(e)))?;
    let opened =
        rustix::fs::fstat(&readable).map_err(|e| HostError::errno("fstat", &shown, errno_of(e)))?;
    if (opened.st_dev, opened.st_ino) != (st.st_dev, st.st_ino) {
        return Err(HostError::invalid(
            "open",
            &shown,
            "the snapshot changed between checking it and reading it",
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::from(readable)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| io_error("read", &shown, &e))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= limit {
        file.bytes = Some(bytes);
    }
    Ok(Some(file))
}

/// Whether `lock` is the open lock file of `key` in the state directory
/// (the same object), so a write is made only under its own key's lock.
fn is_lock_of(
    dir_fd: BorrowedFd<'_>,
    key: &FsKey,
    lock: BorrowedFd<'_>,
) -> Result<bool, HostError> {
    let name = key.lock_name();
    let named = rustix::fs::statat(dir_fd, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| HostError::errno("fstatat", &name, errno_of(e)))?;
    let held =
        rustix::fs::fstat(lock).map_err(|e| HostError::errno("fstat", &name, errno_of(e)))?;
    Ok((named.st_dev, named.st_ino) == (held.st_dev, held.st_ino))
}

/// Replaces `index-<key>` with `bytes` (docs/architecture.md, Snapshot:
/// Atomicity): an `O_TMPFILE` (mode 0600, never `O_TRUNC`) is filled
/// completely, linked in as `index-<key>.new` and renamed onto
/// `index-<key>`, the only name anything is renamed onto. A failure before
/// the link leaves nothing behind; a crash between the link and the rename
/// leaves a complete `.new`, which the next write finds (`EEXIST`) and
/// renames onto the snapshot first. `lock` must be this key's lock file
/// (checked by identity), held exclusively by the caller. No `fsync`: a
/// file torn by a crash fails its checksum and is ignored. When
/// `cancelled` says so (a second signal, asked between chunks and right
/// before the link), the file is never linked in:
/// [`StateWritten::Cancelled`], the old snapshot unchanged. Everything is
/// done relative to `dir_fd`, the state directory held since the lock was
/// opened (TASK-286 review: the directory the space guard sampled, never
/// one reopened by path); `dir` names it in messages only. It must still
/// be one diskgc may use (owner, mode).
pub(crate) fn write_snapshot_ungated(
    dir: &Path,
    dir_fd: BorrowedFd<'_>,
    key: &FsKey,
    lock: BorrowedFd<'_>,
    bytes: &[u8],
    proc: BorrowedFd<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<StateWritten, HostError> {
    check_state_dir(dir_fd, &dir.display().to_string())?;
    let target = key.snapshot_name();
    let shown = dir.join(&target).display().to_string();
    if !is_lock_of(dir_fd, key, lock)? {
        return Err(HostError::invalid(
            "write",
            &shown,
            "the lock given is not this snapshot's single-writer lock",
        ));
    }
    let temp = rustix::fs::openat(
        dir_fd,
        ".",
        OFlags::TMPFILE | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::from_raw_mode(FILE_MODE),
    )
    .map_err(|e| HostError::errno("open O_TMPFILE", dir.display(), errno_of(e)))?;
    // `cancelled` (the caller's: a second signal to `watch`) is asked
    // between chunks and right before each step that names something: the
    // link, a crashed writer's `.new` promoted first, and the link after
    // it. Until the link the file has no name, and dropping it leaves
    // nothing behind; the link is the commit point, the rename after it
    // is not asked.
    let mut rest = bytes;
    while !rest.is_empty() {
        if cancelled() {
            return Ok(StateWritten::Cancelled);
        }
        match rustix::io::write(&temp, rest) {
            Ok(0) => {
                return Err(HostError::invalid(
                    "write",
                    &shown,
                    "the write made no progress",
                ));
            }
            Ok(n) => rest = &rest[n..],
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(HostError::errno("write", &shown, errno_of(e))),
        }
    }
    let new = key.new_name();
    let link = |dir_fd: BorrowedFd<'_>| {
        rustix::fs::linkat(
            proc,
            format!("self/fd/{}", temp.as_fd().as_raw_fd()).as_str(),
            dir_fd,
            new.as_str(),
            AtFlags::SYMLINK_FOLLOW,
        )
    };
    let promote = |dir_fd: BorrowedFd<'_>| {
        rustix::fs::renameat(dir_fd, new.as_str(), dir_fd, target.as_str())
            .map_err(|e| HostError::errno("renameat", &shown, errno_of(e)))
    };
    if cancelled() {
        return Ok(StateWritten::Cancelled);
    }
    match link(dir_fd) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST) => {
            // A complete `.new` a crashed writer left: promote it, then link.
            if cancelled() {
                return Ok(StateWritten::Cancelled);
            }
            promote(dir_fd)?;
            if cancelled() {
                return Ok(StateWritten::Cancelled);
            }
            link(dir_fd).map_err(|e| HostError::errno("linkat", &new, errno_of(e)))?;
        }
        Err(e) => return Err(HostError::errno("linkat", &new, errno_of(e))),
    }
    promote(dir_fd).map(|()| StateWritten::Replaced)
}

/// `BTRFS_IOC_FS_INFO` (`include/uapi/linux/btrfs.h`):
/// `_IOR(0x94, 31, struct btrfs_ioctl_fs_info_args)`, 1024 bytes. No
/// capability is needed; it only reports.
const BTRFS_IOC_FS_INFO: rustix::ioctl::Opcode = 0x8400_941f;

/// `struct btrfs_ioctl_fs_info_args` (`include/uapi/linux/btrfs.h`).
#[repr(C)]
#[derive(Clone, Copy)]
struct BtrfsFsInfo {
    max_id: u64,
    num_devices: u64,
    fsid: [u8; 16],
    nodesize: u32,
    sectorsize: u32,
    clone_alignment: u32,
    csum_type: u16,
    csum_size: u16,
    flags: u64,
    generation: u64,
    metadata_uuid: [u8; 16],
    reserved: [u8; 944],
}

const _: () = assert!(std::mem::size_of::<BtrfsFsInfo>() == 1024);

/// What `BTRFS_IOC_FS_INFO` on a descriptor of a btrfs says of it.
pub(crate) struct BtrfsInfo {
    /// The FSID as blkid and `/sys/fs/btrfs` spell it: lowercase hex,
    /// 8-4-4-4-12. One for every subvolume and member device, read from
    /// the filesystem itself, never a path (TASK-286 review: a mountinfo
    /// source can name another device in diskgc's namespace).
    pub fsid: String,
    /// `max_id`: the highest device id among the filesystem's own devices
    /// (`fs_devices->devices`, a replace target's 0 included). A seed
    /// filesystem's devices have lower ids: a sprout's new device takes the
    /// next id after every id its chunk tree holds, the seed's included
    /// (`find_next_devid`).
    pub max_id: u64,
}

/// `BTRFS_IOC_FS_INFO` on `fd`, which must be opened for reading (an
/// `O_PATH` descriptor gets `EBADF`).
pub(crate) fn btrfs_info(fd: BorrowedFd<'_>, shown: &str) -> Result<BtrfsInfo, HostError> {
    // SAFETY: BTRFS_IOC_FS_INFO fills exactly one `struct
    // btrfs_ioctl_fs_info_args` through its pointer argument (with `flags`
    // 0 on entry, which the getter's zeroed buffer gives); `BtrfsFsInfo`
    // is that struct's layout.
    let info = unsafe {
        rustix::ioctl::ioctl(
            fd,
            rustix::ioctl::Getter::<BTRFS_IOC_FS_INFO, BtrfsFsInfo>::new(),
        )
    }
    .map_err(|e| HostError::errno("ioctl BTRFS_IOC_FS_INFO", shown, errno_of(e)))?;
    let hex: Vec<String> = info.fsid.iter().map(|b| format!("{b:02x}")).collect();
    Ok(BtrfsInfo {
        fsid: format!(
            "{}-{}-{}-{}-{}",
            hex[0..4].concat(),
            hex[4..6].concat(),
            hex[6..8].concat(),
            hex[8..10].concat(),
            hex[10..16].concat()
        ),
        max_id: info.max_id,
    })
}

/// Whether `mount` is a btrfs whose superblock device is anonymous (its
/// files' devices are its subvolumes', never a block device's).
fn is_btrfs(mount: &MountEntry) -> bool {
    mount.fs_type == "btrfs"
}

/// The UUID of the filesystem `mount` shows, reached as `fd`
/// ([`diskgc_core::host::Host::fs_uuid`]): for btrfs its FSID read from
/// `fd` ([`btrfs_info`]; `fd` opened for reading); otherwise the name of the `/dev/disk/by-uuid`
/// link whose block device is the mount's device. `None`: no such link (a
/// container, a system without udev, tmpfs, an overlay). Read-only: names
/// are listed and their targets `stat`ed, nothing is opened.
pub(crate) fn fs_uuid(fd: BorrowedFd<'_>, mount: &MountEntry) -> Result<Option<String>, HostError> {
    if is_btrfs(mount) {
        let shown = mount.mount_point.display().to_string();
        return btrfs_info(fd, &shown).map(|info| Some(info.fsid));
    }
    const BY_UUID: &str = "/dev/disk/by-uuid";
    if mount.dev.major == 0 {
        return Ok(None);
    }
    let want = rustix::fs::makedev(mount.dev.major, mount.dev.minor);
    let entries = match std::fs::read_dir(BY_UUID) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error("read_dir", BY_UUID, &e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_error("read_dir", BY_UUID, &e))?;
        let Ok(st) = rustix::fs::stat(entry.path()) else {
            continue; // a dangling link
        };
        if file_type_of(st.st_mode) == FileType::BlockDevice && st.st_rdev == want {
            let name = entry.file_name();
            return Ok(std::str::from_utf8(name.as_bytes()).ok().map(str::to_owned));
        }
    }
    Ok(None)
}

/// `/sys/dev/block/<dev>`: the sysfs directory of a block device.
fn sys_block(dev: Dev) -> PathBuf {
    PathBuf::from(format!("/sys/dev/block/{dev}"))
}

/// A sysfs `dev` file's `MAJ:MIN`.
fn read_dev_file(path: &Path) -> Result<Dev, HostError> {
    let text = std::fs::read_to_string(path).map_err(|e| io_error("read", path.display(), &e))?;
    let parsed = text.trim().split_once(':').and_then(|(major, minor)| {
        Some(Dev {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        })
    });
    parsed
        .ok_or_else(|| HostError::invalid("read", path.display(), format!("not MAJ:MIN: {text:?}")))
}

/// The loop device block device `dev` is: itself (it has a `loop`
/// directory in sysfs), the loop device it is a partition of (`losetup
/// -P`: a `partition` file, and the parent has `loop`), or none.
fn loop_of(dev: Dev) -> Result<Option<Dev>, HostError> {
    if dev.major == 0 {
        return Ok(None);
    }
    let dir = sys_block(dev);
    let exists = |path: &Path| match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_error("stat", path.display(), &e)),
    };
    if dev.major == diskgc_core::places::LOOP_MAJOR || exists(&dir.join("loop"))? {
        return Ok(Some(dev));
    }
    if !exists(&dir)? || !exists(&dir.join("partition"))? {
        return Ok(None);
    }
    let parent = std::fs::canonicalize(&dir)
        .map_err(|e| io_error("canonicalize", dir.display(), &e))?
        .parent()
        .map(Path::to_owned)
        .ok_or_else(|| HostError::invalid("read", dir.display(), "a partition without a parent"))?;
    if exists(&parent.join("loop"))? {
        Ok(Some(read_dev_file(&parent.join("dev"))?))
    } else {
        Ok(None)
    }
}

/// `BTRFS_IOC_DEV_INFO` (`include/uapi/linux/btrfs.h`):
/// `_IOWR(0x94, 30, struct btrfs_ioctl_dev_info_args)`, 4096 bytes. No
/// capability is needed; it only reports.
const BTRFS_IOC_DEV_INFO: rustix::ioctl::Opcode = 0xd000_941e;

/// `BTRFS_DEVICE_PATH_NAME_MAX`.
const BTRFS_DEVICE_PATH_NAME_MAX: usize = 1024;

/// `struct btrfs_ioctl_dev_info_args` (`include/uapi/linux/btrfs.h`).
#[repr(C)]
#[derive(Clone, Copy)]
struct BtrfsDevInfo {
    devid: u64,
    uuid: [u8; 16],
    bytes_used: u64,
    total_bytes: u64,
    fsid: [u8; 16],
    unused: [u64; 377],
    path: [u8; BTRFS_DEVICE_PATH_NAME_MAX],
}

const _: () = assert!(std::mem::size_of::<BtrfsDevInfo>() == 4096);

/// The highest device id diskgc asks about, so a corrupt or hostile
/// `max_id` cannot keep it asking: a btrfs takes the next id on every
/// `device add`, so a filesystem past this many has seen thousands of
/// device changes. More is an error (fail closed), never a shorter list.
const BTRFS_DEVID_MAX: u64 = 4096;

/// The name `BTRFS_IOC_DEV_INFO` gives the device with id `devid` of the
/// btrfs `fd` is on: `Ok(None)` when no device has that id (`ENODEV`: ids
/// have gaps once a device is removed), `Ok(Some(empty))` for a member
/// without a name (missing: it has no block device, so no loop device
/// holds an image for it). Looked up among the filesystem's own devices,
/// then its seed filesystems' (`btrfs_find_device`).
fn btrfs_dev_name(
    fd: BorrowedFd<'_>,
    devid: u64,
    shown: &str,
) -> Result<Option<Vec<u8>>, HostError> {
    // SAFETY: zeroes are a valid `btrfs_ioctl_dev_info_args` (an empty
    // uuid asks by id alone); BTRFS_IOC_DEV_INFO reads and fills exactly
    // one through its pointer argument, whose layout `BtrfsDevInfo` is.
    let mut args: BtrfsDevInfo = unsafe { std::mem::zeroed() };
    args.devid = devid;
    let asked = unsafe {
        rustix::ioctl::ioctl(
            fd,
            rustix::ioctl::Updater::<BTRFS_IOC_DEV_INFO, BtrfsDevInfo>::new(&mut args),
        )
    };
    match asked {
        Ok(()) => {
            let end = args
                .path
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(args.path.len());
            Ok(Some(args.path[..end].to_vec()))
        }
        Err(rustix::io::Errno::NODEV) => Ok(None),
        Err(e) => Err(HostError::errno(
            "ioctl BTRFS_IOC_DEV_INFO",
            format!("{shown} (device id {devid})"),
            errno_of(e),
        )),
    }
}

/// The names of every member of a btrfs whose highest id is `max_id`
/// (seed devices and a replace target, id 0, included), asked one id at a
/// time by `ask` ([`btrfs_dev_name`]); a member without a name (missing)
/// is left out. Ids above [`BTRFS_DEVID_MAX`] are an error.
fn btrfs_member_names(
    max_id: u64,
    mut ask: impl FnMut(u64) -> Result<Option<Vec<u8>>, HostError>,
    shown: &str,
) -> Result<Vec<Vec<u8>>, HostError> {
    if max_id > BTRFS_DEVID_MAX {
        return Err(HostError::invalid(
            "ioctl BTRFS_IOC_FS_INFO",
            shown,
            format!("device ids up to {max_id}, more than the {BTRFS_DEVID_MAX} diskgc asks about"),
        ));
    }
    let mut names = Vec::new();
    for devid in 0..=max_id {
        if let Some(name) = ask(devid)?
            && !name.is_empty()
        {
            names.push(name);
        }
    }
    Ok(names)
}

/// The block device a btrfs member's name is in diskgc's namespace
/// (`stat`, following links, as `/dev/disk/by-*` names are). A name that
/// is not there, or not a block device, cannot be told: an error.
fn member_device(name: &[u8], shown: &str) -> Result<Dev, HostError> {
    let path = Path::new(OsStr::from_bytes(name));
    let st = rustix::fs::stat(path).map_err(|e| {
        HostError::errno(
            "stat",
            format!("{} (a member of {shown})", path.display()),
            errno_of(e),
        )
    })?;
    if file_type_of(st.st_mode) != FileType::BlockDevice {
        return Err(HostError::invalid(
            "stat",
            path.display(),
            format!("a member of {shown} is not a block device here"),
        ));
    }
    Ok(Dev {
        major: rustix::fs::major(st.st_rdev),
        minor: rustix::fs::minor(st.st_rdev),
    })
}

/// The loop devices the filesystem `mount` shows is stored on
/// ([`diskgc_core::host::Host::loop_devices`]). A btrfs's members are read
/// through `fd`, a directory of it opened for reading: every device id up
/// to `max_id` (`BTRFS_IOC_FS_INFO`) asked by `BTRFS_IOC_DEV_INFO`, which
/// finds the filesystem's own devices (a replace target included) and its
/// seed filesystems'; each name is the block device it stands for here,
/// and a loop device or a partition of one among them is reported (TASK-299
/// review: no count is compared, a seed's link hid a missing member). A
/// member without a name is missing and holds no image. Any failure is an
/// error (fail closed); so is a btrfs without `fd`: mountinfo's source
/// names one member only. Read-only.
pub(crate) fn loop_devices(
    mount: &MountEntry,
    fd: Option<BorrowedFd<'_>>,
) -> Result<Vec<Dev>, HostError> {
    let shown = mount.mount_point.display().to_string();
    let mut devices = Vec::new();
    if mount.dev.major != 0 {
        devices.push(mount.dev);
    } else if is_btrfs(mount) {
        let fd = fd.ok_or_else(|| {
            HostError::invalid(
                "loop_devices",
                &shown,
                "a btrfs's member devices are read from a descriptor of it",
            )
        })?;
        let info = btrfs_info(fd, &shown)?;
        let names = btrfs_member_names(
            info.max_id,
            |devid| btrfs_dev_name(fd, devid, &shown),
            &shown,
        )?;
        for name in names {
            devices.push(member_device(&name, &shown)?);
        }
    }
    let mut loops = Vec::new();
    for dev in devices {
        loops.extend(loop_of(dev)?);
    }
    loops.sort();
    loops.dedup();
    Ok(loops)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_by_uuid_directory_or_device_is_no_uuid() {
        // The test sandbox may or may not have udev links; a device that
        // cannot exist never has one, and an anonymous device none.
        let dir = rustix::fs::open("/", OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
            .expect("open /");
        let mount = |major, minor| MountEntry {
            id: 1,
            parent_id: 1,
            dev: Dev { major, minor },
            fs_root: PathBuf::from("/"),
            mount_point: PathBuf::from("/"),
            fs_type: "ext4".to_owned(),
            source: "/dev/diskgc-no-such-device".to_owned(),
            read_only: false,
            idmapped: false,
            upper_dir: None,
        };
        assert_eq!(
            fs_uuid(dir.as_fd(), &mount(4095, 1_048_575)).expect("listing"),
            None
        );
        assert_eq!(
            fs_uuid(dir.as_fd(), &mount(0, 42)).expect("anonymous"),
            None
        );
        assert_eq!(
            loop_devices(&mount(4095, 1_048_575), None).expect("sysfs"),
            []
        );
        assert_eq!(
            loop_devices(&mount(0, 42), Some(dir.as_fd())).expect("tmpfs"),
            []
        );
    }

    /// `BTRFS_IOC_FS_INFO` on a directory of another filesystem fails
    /// (`ENOTTY`): its FSID is never made up.
    #[test]
    fn a_btrfs_fsid_is_read_only_from_btrfs() {
        let dir = rustix::fs::open("/proc", OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
            .expect("open /proc");
        assert!(btrfs_info(dir.as_fd(), "/proc").is_err());
    }

    /// TASK-299 review (Codex 5): every id up to `max_id` is asked, id 0
    /// (a replace target) and gaps (`ENODEV`) included; a nameless member
    /// (missing) is left out; an error of any one fails the whole list; an
    /// absurd `max_id` is refused before anything is asked.
    #[test]
    fn btrfs_members_are_every_id_asked() {
        let mut asked = Vec::new();
        let names = btrfs_member_names(
            4,
            |devid| {
                asked.push(devid);
                Ok(match devid {
                    0 => Some(b"/dev/loop9".to_vec()),
                    1 => Some(b"/dev/sda2".to_vec()),
                    2 => None,
                    3 => Some(Vec::new()),
                    _ => Some(b"/dev/loop3".to_vec()),
                })
            },
            "/data",
        )
        .expect("members");
        assert_eq!(asked, [0, 1, 2, 3, 4]);
        assert_eq!(
            names,
            [
                b"/dev/loop9".to_vec(),
                b"/dev/sda2".to_vec(),
                b"/dev/loop3".to_vec()
            ]
        );
        let failed = btrfs_member_names(
            2,
            |devid| {
                if devid == 1 {
                    Err(HostError::errno(
                        "ioctl",
                        "x",
                        diskgc_core::host::Errno::EIO,
                    ))
                } else {
                    Ok(Some(b"/dev/loop1".to_vec()))
                }
            },
            "/data",
        );
        assert!(failed.is_err());
        let mut called = false;
        let absurd = btrfs_member_names(
            u64::MAX,
            |_| {
                // Asked at all: refused too late. Stop a loop to u64::MAX.
                called = true;
                Err(HostError::errno(
                    "ioctl",
                    "x",
                    diskgc_core::host::Errno::EIO,
                ))
            },
            "/data",
        );
        assert!(absurd.is_err() && !called);
    }

    /// A member's name is the block device it stands for here; anything
    /// else (missing, a regular file) cannot be told.
    #[test]
    fn a_member_name_must_be_a_block_device_here() {
        assert!(member_device(b"/diskgc-no-such-device", "/data").is_err());
        assert!(member_device(b"/proc/self/status", "/data").is_err());
    }

    /// TASK-299: a btrfs's members are never answered without a
    /// descriptor of it (mountinfo's source names one member).
    #[test]
    fn a_btrfs_without_a_descriptor_has_no_members_to_tell() {
        let mount = MountEntry {
            id: 1,
            parent_id: 1,
            dev: Dev {
                major: 0,
                minor: 42,
            },
            fs_root: PathBuf::from("/"),
            mount_point: PathBuf::from("/data"),
            fs_type: "btrfs".to_owned(),
            source: "/dev/loop0".to_owned(),
            read_only: false,
            idmapped: false,
            upper_dir: None,
        };
        let error = loop_devices(&mount, None).expect_err("no descriptor");
        assert!(error.to_string().contains("descriptor"), "{error}");
    }
}
