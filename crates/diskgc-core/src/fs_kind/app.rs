//! Chromium-family SingletonLock guard. A lock is a symlink whose relative
//! text is `hostname-pid`; it is never followed. Only a missing lock in a
//! safely reached config directory, or a lock naming a process proved gone
//! by the host's pidfd-backed process reader, permits a cache candidate.

use std::path::{Path, PathBuf};

use super::judge::{mountinfo, skip_or_stop};
use crate::engine::{Identity, Reason, Skip};
use crate::host::{Base, Errno, FileType, Host, MonoTime, ProcError};
use crate::outcome::{Judged, Stop};
use crate::resolve::{Want, resolve_in};

/// Checks one application lock. Called for each candidate during planning,
/// tallying, and reclaim, including immediately before deletion. A PID that
/// was reused by another process still blocks: that is the safe answer.
pub(super) fn check<H: Host>(
    host: &H,
    root: &Path,
    lock: &Path,
    deadline: Option<MonoTime>,
) -> Judged<()> {
    let lock_path = if lock.is_absolute() {
        lock.to_owned()
    } else {
        root.join(lock)
    };
    let lock = lock_path.as_path();
    if deadline.is_some_and(|end| host.now() >= end) {
        return Err(Stop::DeadlinePassed {
            path: lock.to_owned(),
        });
    }
    let Some(parent) = lock.parent() else {
        return Ok(Err(refuse(lock, Reason::UnsafePath, "lock has no parent")));
    };
    let Some(name) = lock.file_name() else {
        return Ok(Err(refuse(lock, Reason::UnsafePath, "lock has no name")));
    };
    let mounts = mountinfo(host)?;
    let dir = match resolve_in(host, &mounts, parent, Want::Directory) {
        Ok(dir) => dir,
        Err(e) => {
            return Ok(Err(refuse(
                lock,
                Reason::UnsafePath,
                format!("cannot safely reach lock parent: {e}"),
            )));
        }
    };
    let first = match host.statx(Base::Dir(&dir.fd), Path::new(name)) {
        Ok(stat) => stat,
        Err(e) if e.is(Errno::ENOENT) => return Ok(Ok(())),
        Err(e) => return Ok(Err(skip_or_stop(lock.to_owned(), "stat", &e)?)),
    };
    if first.file_type != Some(FileType::Symlink) {
        return Ok(Err(refuse(
            lock,
            Reason::InUse,
            "SingletonLock is not a symlink",
        )));
    }
    let target = match host.read_link(&dir.fd, name) {
        Ok(target) => target,
        Err(e) => return Ok(Err(skip_or_stop(lock.to_owned(), "readlink", &e)?)),
    };
    let second = match host.statx(Base::Dir(&dir.fd), Path::new(name)) {
        Ok(stat) => stat,
        Err(e) => return Ok(Err(skip_or_stop(lock.to_owned(), "stat", &e)?)),
    };
    if Identity::of(&first).is_none() || Identity::of(&first) != Identity::of(&second) {
        return Ok(Err(refuse(
            lock,
            Reason::Changed,
            "SingletonLock changed while read",
        )));
    }
    let Some(text) = target.to_str() else {
        return Ok(Err(refuse(
            lock,
            Reason::InUse,
            "SingletonLock target is not UTF-8",
        )));
    };
    // The lock target is text, often relative. Never resolve or follow it.
    let Some((host_name, pid_text)) = text.rsplit_once('-') else {
        return Ok(Err(refuse(
            lock,
            Reason::InUse,
            "SingletonLock target has no hostname-pid",
        )));
    };
    if host_name.is_empty() || pid_text.is_empty() || !pid_text.bytes().all(|b| b.is_ascii_digit())
    {
        return Ok(Err(refuse(
            lock,
            Reason::InUse,
            "SingletonLock target has an invalid hostname-pid",
        )));
    }
    let Ok(pid) = pid_text.parse::<u32>() else {
        return Ok(Err(refuse(
            lock,
            Reason::InUse,
            "SingletonLock PID is out of range",
        )));
    };
    if pid == 0 {
        return Ok(Err(refuse(
            lock,
            Reason::InUse,
            "SingletonLock PID is zero",
        )));
    }
    let hostname = host.hostname().map_err(|error| Stop::Failed {
        path: PathBuf::from("uname -n"),
        error,
    })?;
    if hostname != host_name {
        return Ok(Err(refuse(
            lock,
            Reason::InUse,
            format!("SingletonLock names host {host_name:?}, local host is {hostname:?}"),
        )));
    }
    match host.proc_status(crate::host::Pid(pid)) {
        Err(ProcError::Gone) => Ok(Ok(())),
        Ok(_) => Ok(Err(refuse(
            lock,
            Reason::InUse,
            format!("SingletonLock names live PID {pid}"),
        ))),
        Err(error) => Ok(Err(refuse(
            lock,
            Reason::InUse,
            format!("cannot prove SingletonLock PID {pid} exited: {error}"),
        ))),
    }
}

fn refuse(lock: &Path, reason: Reason, detail: impl Into<String>) -> Skip {
    Skip::new(reason, lock.to_owned(), detail.into())
}
