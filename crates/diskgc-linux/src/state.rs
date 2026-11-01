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
    Dev, FileType, FsKey, HostError, MakeUserLock, StateFile, StateWritten, UserLockError,
};
use diskgc_core::hostlock::USER_LOCK_NAME;
use rustix::fs::{AtFlags, Mode, OFlags};

use crate::host::{errno_of, io_error};

/// Mode of the state directory diskgc creates.
const DIR_MODE: u32 = 0o700;

/// Mode of the files diskgc creates there (the umask can only narrow it).
const FILE_MODE: u32 = 0o600;

fn dev_of(st_dev: u64) -> Dev {
    Dev {
        major: rustix::fs::major(st_dev),
        minor: rustix::fs::minor(st_dev),
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
pub(crate) fn open_state_dir(dir: &Path) -> Result<(OwnedFd, Dev), HostError> {
    let shown = dir.display().to_string();
    let fd = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| HostError::errno("open", &shown, errno_of(e)))?;
    let st = rustix::fs::fstat(&fd).map_err(|e| HostError::errno("fstat", &shown, errno_of(e)))?;
    let uid = rustix::process::geteuid().as_raw();
    if st.st_uid != uid || st.st_mode & 0o022 != 0 {
        return Err(HostError::invalid(
            "open",
            &shown,
            format!(
                "the state directory must be owned by uid {uid} and writable by no one else \
                 (owner {}, mode {:04o})",
                st.st_uid,
                st.st_mode & 0o7777
            ),
        ));
    }
    Ok((fd, dev_of(st.st_dev)))
}

/// Opens (creating it, and the state directory, if absent) the
/// single-writer lock file of `key`, read-only, for `flock` only: nothing
/// is ever written to it or renamed onto it (docs/architecture.md, Single
/// writer). The directory is created mode 0700 with its missing parents;
/// the file mode 0600, `O_NOFOLLOW` (a symlink there is refused), and it
/// must be a regular file of this user. Returns the fd and the device of
/// the state directory's filesystem.
pub(crate) fn open_lock_file_ungated(
    dir: &Path,
    key: &FsKey,
) -> Result<(OwnedFd, PathBuf, Dev), HostError> {
    let (dir_fd, dir_dev) = match open_state_dir(dir) {
        Err(e) if e.is(diskgc_core::host::Errno::ENOENT) => {
            make_dirs_ungated(dir)?;
            open_state_dir(dir)?
        }
        other => other?,
    };
    let name = key.lock_name();
    let path = dir.join(&name);
    let shown = path.display().to_string();
    let fd = rustix::fs::openat(
        &dir_fd,
        name.as_str(),
        OFlags::RDONLY
            | OFlags::CREATE
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::NOCTTY
            | OFlags::CLOEXEC,
        Mode::from_raw_mode(FILE_MODE),
    )
    .map_err(|e| HostError::errno("open", &shown, errno_of(e)))?;
    let st = rustix::fs::fstat(&fd).map_err(|e| HostError::errno("fstat", &shown, errno_of(e)))?;
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
    Ok((fd, path, dir_dev))
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
fn inspect(dir: BorrowedFd<'_>, name: &OsStr, shown: &Path) -> Result<Inspected, UserLockError> {
    let failed = |op, e| UserLockError::Failed {
        path: shown.to_path_buf(),
        error: HostError::errno(op, shown.display(), errno_of(e)),
    };
    match rustix::fs::openat(
        dir,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let st = rustix::fs::fstat(&fd).map_err(|e| failed("fstat", e))?;
            Ok(Inspected::Present(fd, st))
        }
        Err(rustix::io::Errno::NOENT) => Ok(Inspected::Absent),
        Err(e) => Err(failed("open", e)),
    }
}

/// Opens the per-user reclaim lock `dir/lock` (docs/safety.md, Host lock),
/// read-only, for `flock` only: nothing is ever written to it or renamed
/// onto it. `dir` is the per-user lock directory `LinuxHost` found
/// (`/run/user/<uid>/diskgc` or `<home>/.local/state/diskgc`). Its parent
/// is reached following symlinks (made, mode 0700 with its missing
/// parents, when `make` allows). The directory and the file are inspected
/// before anything opens them for reading ([`inspect`]): the directory must
/// be a directory of this user that no one else may write to, the file a
/// regular file of this user that no one else may open (`mode & 0o077`:
/// they could hold the lock); anything else, whoever owns it, is
/// [`UserLockError::Unsafe`]. Then the checked file is reopened read-only
/// through `/proc/self/fd` and checked to be the same object. What is
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
    // Reopened for reading through the checked object, never by name again.
    let readable = rustix::fs::openat(
        proc,
        format!("self/fd/{}", file.as_raw_fd()).as_str(),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| failed("open", &path, e))?;
    let opened = rustix::fs::fstat(&readable).map_err(|e| failed("fstat", &path, e))?;
    if (opened.st_dev, opened.st_ino) != (st.st_dev, st.st_ino) {
        return Err(unsafe_(
            &path,
            "the per-user lock changed between checking it and opening it".into(),
        ));
    }
    Ok(Some((readable, path)))
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
        Ok((fd, _)) => fd,
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
fn is_lock_of(dir_fd: &OwnedFd, key: &FsKey, lock: BorrowedFd<'_>) -> Result<bool, HostError> {
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
/// [`StateWritten::Cancelled`], the old snapshot unchanged.
pub(crate) fn write_snapshot_ungated(
    dir: &Path,
    key: &FsKey,
    lock: BorrowedFd<'_>,
    bytes: &[u8],
    proc: BorrowedFd<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<StateWritten, HostError> {
    let (dir_fd, _) = open_state_dir(dir)?;
    let target = key.snapshot_name();
    let shown = dir.join(&target).display().to_string();
    if !is_lock_of(&dir_fd, key, lock)? {
        return Err(HostError::invalid(
            "write",
            &shown,
            "the lock given is not this snapshot's single-writer lock",
        ));
    }
    let temp = rustix::fs::openat(
        &dir_fd,
        ".",
        OFlags::TMPFILE | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::from_raw_mode(FILE_MODE),
    )
    .map_err(|e| HostError::errno("open O_TMPFILE", dir.display(), errno_of(e)))?;
    // `cancelled` (a second signal) is asked between chunks and right
    // before the link: until then the file has no name, and dropping it
    // leaves nothing behind.
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
    let link = |dir_fd: &OwnedFd| {
        rustix::fs::linkat(
            proc,
            format!("self/fd/{}", temp.as_fd().as_raw_fd()).as_str(),
            dir_fd,
            new.as_str(),
            AtFlags::SYMLINK_FOLLOW,
        )
    };
    let promote = |dir_fd: &OwnedFd| {
        rustix::fs::renameat(dir_fd, new.as_str(), dir_fd, target.as_str())
            .map_err(|e| HostError::errno("renameat", &shown, errno_of(e)))
    };
    if cancelled() {
        return Ok(StateWritten::Cancelled);
    }
    match link(&dir_fd) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST) => {
            // A complete `.new` a crashed writer left: promote it, then link.
            promote(&dir_fd)?;
            link(&dir_fd).map_err(|e| HostError::errno("linkat", &new, errno_of(e)))?;
        }
        Err(e) => return Err(HostError::errno("linkat", &new, errno_of(e))),
    }
    promote(&dir_fd).map(|()| StateWritten::Replaced)
}

/// The UUID of the filesystem whose files report `dev`, from the udev
/// links `/dev/disk/by-uuid/<uuid>` (the link whose block device has that
/// number); `None` without such a link, or without the directory (a
/// container, a system without udev). Read-only: names are listed and
/// their targets `stat`ed, nothing is opened.
pub(crate) fn fs_uuid(dev: Dev) -> Result<Option<String>, HostError> {
    const BY_UUID: &str = "/dev/disk/by-uuid";
    let entries = match std::fs::read_dir(BY_UUID) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error("read_dir", BY_UUID, &e)),
    };
    let want = rustix::fs::makedev(dev.major, dev.minor);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_by_uuid_directory_or_device_is_no_uuid() {
        // The test sandbox may or may not have udev links; a device that
        // cannot exist never has one.
        assert_eq!(
            fs_uuid(Dev {
                major: 4095,
                minor: 1_048_575
            })
            .expect("listing"),
            None
        );
    }
}
