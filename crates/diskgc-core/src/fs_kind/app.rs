//! Chromium-family SingletonLock guard. Only a missing lock in a safely
//! reached config directory permits a cache candidate. A lock PID is relative
//! to the browser's PID namespace, which may differ from ours even when both
//! processes share the same home directory. We therefore never infer that a
//! present lock is stale by looking up its PID in our process namespace.

use std::path::Path;

use super::judge::{mountinfo, skip_or_stop};
use crate::engine::{Reason, Skip};
use crate::host::{Base, Errno, Host, MonoTime};
use crate::outcome::{Judged, Stop};
use crate::resolve::{Want, resolve_in};

/// Checks one application lock. Called for each candidate during planning,
/// tallying, and reclaim, including immediately before deletion. A present
/// lock always blocks: a PID that appears gone here may still be live in a
/// private PID namespace with this same home directory mounted into it. A
/// directory or malformed lock entry blocks too; its contents are irrelevant.
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
    match host.statx(Base::Dir(&dir.fd), Path::new(name)) {
        Ok(_) => Ok(Err(refuse(
            lock,
            Reason::InUse,
            "SingletonLock is present; its owner cannot be proved stopped across PID namespaces",
        ))),
        Err(e) if e.is(Errno::ENOENT) => Ok(Ok(())),
        Err(e) => Ok(Err(skip_or_stop(lock.to_owned(), "stat", &e)?)),
    }
}

fn refuse(lock: &Path, reason: Reason, detail: impl Into<String>) -> Skip {
    Skip::new(reason, lock.to_owned(), detail.into())
}
