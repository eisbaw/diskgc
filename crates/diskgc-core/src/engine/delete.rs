//! The delete step (docs/safety.md, Deletion, step 5): removes exactly the
//! entries of a [`WalkRecord`], depth first, in the record's (name) order.
//!
//! - Every directory is reopened below its parent by its recorded name
//!   (`open_beneath`: no symlink, no mount crossing) and matched against its
//!   recorded identity, after a deadline check; see [`super::frames`].
//! - Every entry is removed by [`checked_unlink`]: the deadline is checked,
//!   then `statx` of the name, its identity (device, inode, owner, type,
//!   birth time) compared with the record, the deadline again (the `statx`
//!   can block, the machine can sleep), then `unlinkat`. No filesystem call
//!   runs between the `statx` and the `unlinkat`, only that clock read;
//!   what can still happen there (the name replaced by another process) is
//!   the window docs/safety.md accepts, since Linux has no unlink-by-inode.
//! - With `make_writable`, a directory without owner write permission gets
//!   it right after it is opened and matched (a readable fd is prepared
//!   before the final deadline read and `fchmod`), before anything in it is unlinked (after every
//!   check: the walk and the caller's in-use check ran before this step).
//!
//! The first mismatch stops the delete: the candidate is `changed`, and
//! nothing more is unlinked (a concurrent writer or remover is at work; the
//! rest is left to a later run, which walks the candidate afresh). Entries
//! the walk did not see (created afterwards) are never unlinked; the
//! directory holding one then fails with `ENOTEMPTY`, reported as an error.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::host::{
    Access, Base, Cause, Delete, Errno, FileType, HostError, MonoTime, Stat, Unlink,
};
use crate::resolve::{ResolveError, Want, open_beneath};

use super::frames::{OpenFailed, OpenFailure, Stack};
use super::{Candidate, Identity, WalkRecord, deadline_passed};

/// How [`delete_unchecked`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeleteOptions {
    /// Add owner write permission to directories that lack it (the
    /// profile's `make_writable`, docs/reference.md, Actions).
    pub make_writable: bool,
    /// Start no unlink after this point.
    pub deadline: Option<MonoTime>,
}

/// What [`delete_unchecked`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deletion {
    /// Entries unlinked, the candidate included if it was.
    pub removed_entries: usize,
    /// Their allocated bytes as the walk recorded them (an estimate; the
    /// measure is the observed change in available space).
    pub removed_bytes: u64,
    pub end: End,
}

/// How the delete ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// Every recorded entry was unlinked, the candidate last.
    Complete,
    /// An entry or directory was not the one the walk recorded (replaced,
    /// gone, a mount on it): nothing more was unlinked (`changed`).
    Changed { path: PathBuf, detail: String },
    /// The deadline passed before `path` was unlinked.
    DeadlinePassed { path: PathBuf },
    /// The host failed an operation (`ENOTEMPTY` when something was created
    /// in a directory after the walk, `EACCES` in a read-only directory
    /// without `make_writable`, ...): an error for the candidate.
    Failed { path: PathBuf, error: HostError },
    /// The pre-release sandbox gate refused (docs/safety.md), at `path`: an
    /// error of the whole run, never a skip; every later deletion of the
    /// process is refused too.
    GateRefused { path: PathBuf, error: HostError },
}

/// Deletes the entries of `record` without proving the in-use check ran.
/// Production protocols must use [`crate::inuse::clear_and_delete`]; this
/// entry point exists for isolated deletion-engine and host tests.
pub fn delete_unchecked<H: Delete>(
    host: &H,
    candidate: &Candidate<'_, H::Fd>,
    record: &WalkRecord,
    options: DeleteOptions,
) -> Deletion {
    let mut deleter = Deleter {
        host,
        options,
        removed_entries: 0,
        removed_bytes: 0,
    };
    let end = match deleter.run(candidate, record) {
        Ok(()) => End::Complete,
        Err(end) => end,
    };
    Deletion {
        removed_entries: deleter.removed_entries,
        removed_bytes: deleter.removed_bytes,
        end,
    }
}

struct Deleter<'d, H> {
    host: &'d H,
    options: DeleteOptions,
    removed_entries: usize,
    removed_bytes: u64,
}

impl<H: Delete> Deleter<'_, H> {
    fn run(&mut self, candidate: &Candidate<'_, H::Fd>, record: &WalkRecord) -> Result<(), End> {
        let root = record.root();
        let Some(listing) = root.listing else {
            return self.remove(
                candidate.parent,
                candidate.name,
                &root.identity,
                root.bytes,
                || candidate.path.to_path_buf(),
            );
        };
        let mut stack = Stack::new(self.host, candidate, self.options.deadline);
        stack.push(listing, root.name.clone(), root.identity, root.bytes);
        while let Some(i) = stack.top() {
            // Opening checks the deadline first, so nothing is made
            // writable once it passed.
            match stack.open(i) {
                Ok(Some(fresh)) => {
                    self.make_writable(stack.fd(i), &fresh, || stack.path(i, None))?;
                }
                Ok(None) => {}
                Err(failed) => {
                    return Err(open_failure(stack.path(failed.frame, None), failed));
                }
            }
            let frame = stack.frame(i);
            if let Some(entry) = record.listing(frame.listing).get(frame.cursor) {
                stack.frame_mut(i).cursor += 1;
                match entry.listing {
                    Some(sub) => stack.push(sub, entry.name.clone(), entry.identity, entry.bytes),
                    None => self.remove(
                        stack.fd(i),
                        &entry.name,
                        &entry.identity,
                        entry.bytes,
                        || stack.path(i, Some(&entry.name)),
                    )?,
                }
                continue;
            }
            // Everything in frame i is gone: remove the directory itself
            // from its parent.
            let done = stack.pop().expect("frame i is the top");
            let (parent, up) = match i.checked_sub(1) {
                None => (candidate.parent, None),
                Some(up) => {
                    if let Err(failed) = stack.open(up) {
                        return Err(open_failure(stack.path(failed.frame, None), failed));
                    }
                    (stack.fd(up), Some(up))
                }
            };
            let path = || match up {
                None => candidate.path.to_path_buf(),
                Some(up) => stack.path(up, Some(&done.name)),
            };
            self.remove(parent, &done.name, &done.identity, done.bytes, path)?;
        }
        Ok(())
    }

    /// Unlinks the recorded entry `name` of `dir` after the deadline check,
    /// through [`checked_unlink`].
    fn remove(
        &mut self,
        dir: &H::Fd,
        name: &OsStr,
        expected: &Identity,
        bytes: u64,
        path: impl FnOnce() -> PathBuf,
    ) -> Result<(), End> {
        if deadline_passed(self.host, self.options.deadline) {
            return Err(End::DeadlinePassed { path: path() });
        }
        match checked_unlink(self.host, dir, name, expected, self.options.deadline) {
            Ok(Unlinked::DeadlinePassed) => Err(End::DeadlinePassed { path: path() }),
            Ok(Unlinked::Removed) => {
                self.removed_entries += 1;
                self.removed_bytes = self.removed_bytes.saturating_add(bytes);
                Ok(())
            }
            Ok(Unlinked::Changed(detail)) => Err(End::Changed {
                path: path(),
                detail,
            }),
            Err(error) => Err(host_failure(path(), error)),
        }
    }

    /// Adds owner write permission to the directory `dir` (just opened and
    /// matched, `stat` its fresh stat) if the options ask for it and it
    /// lacks it. The engine's only caller of `Delete::fchmod`.
    fn make_writable(
        &self,
        dir: &H::Fd,
        stat: &Stat,
        path: impl FnOnce() -> PathBuf,
    ) -> Result<(), End> {
        // A mode the kernel did not report is left alone: the unlinks
        // below then fail on their own if it is read-only.
        match stat.perm {
            Some(perm) if self.options.make_writable && perm & 0o200 == 0 => {
                // Linux fchmod cannot act on O_PATH. Prepare its readable fd
                // before the last clock read; reopening may block on a sick
                // filesystem. Confirm it is still the directory the walk
                // checked before changing its permissions.
                let Some(mount_id) = stat.mnt_id else {
                    return Err(End::Changed {
                        path: path(),
                        detail: "the kernel did not report the directory's mount id".into(),
                    });
                };
                let (readable, reopened) = match open_beneath(
                    self.host,
                    dir,
                    mount_id,
                    Path::new(""),
                    Access::Read,
                    Want::Directory,
                ) {
                    Ok(opened) => opened,
                    Err(ResolveError::Host(error)) => return Err(host_failure(path(), error)),
                    Err(error) => {
                        return Err(End::Changed {
                            path: path(),
                            detail: format!("could not reopen the same directory: {error}"),
                        });
                    }
                };
                if Identity::of(&reopened).is_none()
                    || Identity::of(&reopened) != Identity::of(stat)
                {
                    return Err(End::Changed {
                        path: path(),
                        detail: "readable fd differs from the directory the walk checked".into(),
                    });
                }
                match self.host.fchmod(&readable, self.options.deadline) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(End::DeadlinePassed { path: path() }),
                    Err(error) => Err(host_failure(path(), error)),
                }
            }
            _ => Ok(()),
        }
    }
}

/// What [`checked_unlink`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unlinked {
    Removed,
    /// Not unlinked: the deadline passed while the name was checked.
    DeadlinePassed,
    /// Not unlinked: the name is not (or no longer) the recorded object.
    Changed(String),
}

/// The identity-checked unlink (docs/safety.md, Deletion): `statx` of
/// `name` in `dir`, its (device, inode, owner, type, birth time) compared with
/// `expected`, then the deadline (the `statx` can block, the machine can
/// sleep), then `unlinkat` as the very next port call (no filesystem call
/// between: only that clock read). The only caller of `Delete::unlinkat`
/// in production code (a source-scan allowlist entry enforces it), so no
/// unlink can skip the check.
fn checked_unlink<H: Delete>(
    host: &H,
    dir: &H::Fd,
    name: &OsStr,
    expected: &Identity,
    deadline: Option<MonoTime>,
) -> Result<Unlinked, HostError> {
    let found = match host.statx(Base::Dir(dir), Path::new(name)) {
        Ok(stat) => stat,
        Err(e) if e.is(Errno::ENOENT) => return Ok(Unlinked::Changed(format!("gone: {e}"))),
        Err(e) => return Err(e),
    };
    let now = Identity::of(&found);
    if now != Some(*expected) {
        return Ok(Unlinked::Changed(format!(
            "not the object the walk checked: found {now:?}, recorded {expected:?}"
        )));
    }
    if deadline_passed(host, deadline) {
        return Ok(Unlinked::DeadlinePassed);
    }
    let what = if expected.file_type == FileType::Directory {
        Unlink::Dir
    } else {
        Unlink::NonDir
    };
    match host.unlinkat(dir, name, what) {
        Ok(()) => Ok(Unlinked::Removed),
        // Gone, a mount put on it, or another type put in its place, after
        // the check (the window docs/safety.md accepts).
        Err(e)
            if [Errno::ENOENT, Errno::EBUSY, Errno::EISDIR, Errno::ENOTDIR]
                .iter()
                .any(|errno| e.is(*errno)) =>
        {
            Ok(Unlinked::Changed(format!("after its check: {e}")))
        }
        Err(e) => Err(e),
    }
}

/// A host error as the end of the delete.
fn host_failure(path: PathBuf, error: HostError) -> End {
    match error.cause {
        Cause::SandboxGate => End::GateRefused { path, error },
        Cause::Errno(_) | Cause::Invalid => End::Failed { path, error },
    }
}

/// Why a directory could not be reopened, as the end of the delete: an
/// errno the host returned is an error; anything that means "not the
/// recorded directory any more" (gone, replaced, a symlink or a mount now,
/// `EXDEV` from `RESOLVE_NO_XDEV`) is `changed`.
fn open_failure(path: PathBuf, failed: OpenFailed) -> End {
    match failed.why {
        OpenFailure::DeadlinePassed => End::DeadlinePassed { path },
        OpenFailure::Resolve(ResolveError::Host(error)) => host_failure(path, error),
        OpenFailure::Resolve(e) => End::Changed {
            path,
            detail: e.to_string(),
        },
        OpenFailure::NotSame { found } => End::Changed {
            path,
            detail: format!("not the directory the walk checked (now {found:?})"),
        },
    }
}
