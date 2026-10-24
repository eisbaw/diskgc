//! The host lock (docs/safety.md, Host lock): one reclaim per host at a
//! time, by an exclusive `flock` on the fixed path `/run/diskgc/lock`,
//! which whatever installs diskgc provisions (the tmpfiles lines in
//! [`TMPFILES_LINES`]).
//!
//! [`acquire`] reaches the file without following a symlink or crossing a
//! mount ([`resolve`] of `/run/diskgc`, then the name below it), checks
//! that it is a regular file on the `O_PATH` fd and reopens that object
//! read-only through the fd ([`Host::reopen_regular`]), so a FIFO or a
//! device standing at the path is never opened. It then waits for the lock
//! until the deadline ([`Host::flock_wait`]: non-blocking attempts, so the
//! deadline and an observed interruption both stop the wait) and makes the held fd inheritable by
//! every child ([`Host::inherit_lock`]), so the lock stays held until the
//! last child exits. It is released only by dropping the [`HostLock`]
//! (closing the fd); the port has no unlock.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::host::{Access, FileType, Host, HostError, LockMode, Locked, MonoTime, WaitLock};
use crate::report::Signal;
use crate::resolve::{ResolveError, Want, open_beneath, resolve};

/// The directory of the host lock. Fixed, not configurable: two configs
/// naming two lock files would silently break mutual exclusion.
pub const HOST_LOCK_DIR: &str = "/run/diskgc";

/// The host lock file's name in [`HOST_LOCK_DIR`].
pub const HOST_LOCK_NAME: &str = "lock";

/// The host lock's path.
pub fn host_lock_path() -> PathBuf {
    Path::new(HOST_LOCK_DIR).join(HOST_LOCK_NAME)
}

/// The tmpfiles.d lines that provision the host lock (docs/safety.md,
/// Host lock), printed when it is missing.
pub const TMPFILES_LINES: [&str; 2] = [
    "d /run/diskgc      0755 root root -",
    "f /run/diskgc/lock 0644 root root -",
];

/// The commands that provision the host lock, for the refusal (TASK-151):
/// the package's tmpfiles.d drop-in (`lib/tmpfiles.d/diskgc.conf`, the
/// [`TMPFILES_LINES`]) written out in full, so the command is exact
/// wherever the package lives, installed in `/etc/tmpfiles.d` so that the
/// next boot recreates `/run/diskgc`, and applied now; then the plain
/// `mkdir`/`touch`/`chmod` for a host without systemd-tmpfiles.
struct FixIt;

impl fmt::Display for FixIt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "To provision it now and at every boot, install the tmpfiles.d drop-in the \
             package ships (lib/tmpfiles.d/diskgc.conf) and apply it:\n  \
             printf '%s\\n' '{}' '{}' | sudo tee /etc/tmpfiles.d/diskgc.conf\n  \
             sudo systemd-tmpfiles --create /etc/tmpfiles.d/diskgc.conf\n\
             Without systemd-tmpfiles (until the next boot, /run is emptied then):\n  \
             sudo mkdir -p -m 0755 {HOST_LOCK_DIR} && sudo touch {lock} && sudo chmod 0644 {lock}",
            TMPFILES_LINES[0],
            TMPFILES_LINES[1],
            lock = host_lock_path().display(),
        )
    }
}

/// The held host lock. Dropping it closes the fd, which releases the lock
/// once every child that inherited a copy has closed its copy too.
#[derive(Debug)]
pub struct HostLock<F> {
    locked: Locked<F>,
    /// How long [`acquire`] waited for it.
    pub waited: std::time::Duration,
}

impl<F> HostLock<F> {
    /// The held fd (for the record only: it must never be `flock`ed again).
    pub fn locked(&self) -> &Locked<F> {
        &self.locked
    }
}

/// Why the host lock was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    /// `/run/diskgc/lock` (or its directory) does not exist: not
    /// provisioned (exit 2, printing [`TMPFILES_LINES`]).
    Missing { detail: String },
    /// Something other than a regular file stands at the path, or it cannot
    /// be reached safely (a symlink, a mount crossing): a provisioning
    /// error too (exit 2).
    Unusable { detail: String },
    /// Another reclaim held it until the deadline.
    DeadlinePassed,
    /// SIGINT or SIGTERM arrived before the lock could be used.
    Interrupted(Signal),
    /// The host failed (opening, reopening, locking, `fcntl`).
    Failed(HostError),
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = host_lock_path();
        match self {
            Self::Missing { detail } => write!(
                f,
                "the host lock {} is missing ({detail}): this host is not provisioned for \
                 diskgc. {}",
                path.display(),
                FixIt
            ),
            Self::Unusable { detail } => write!(
                f,
                "the host lock {path} is unusable: {detail}. As root, remove what stands at \
                 {path}. {}",
                FixIt,
                path = path.display()
            ),
            Self::DeadlinePassed => write!(
                f,
                "the host lock {} was still held by another reclaim at the deadline",
                path.display()
            ),
            Self::Interrupted(signal) => write!(
                f,
                "interrupted by {signal:?} while waiting for the host lock"
            ),
            Self::Failed(e) => write!(f, "the host lock {}: {e}", path.display()),
        }
    }
}

impl std::error::Error for LockError {}

/// Check that the provisioned lock can be opened without taking it. An
/// already-met `ensure` uses this to report a broken installation early.
pub fn readiness<H: Host>(host: &H) -> Result<(), LockError> {
    drop(open_lock(host)?);
    Ok(())
}

/// Reach and reopen exactly the lock object that [`acquire`] will flock.
fn open_lock<H: Host>(host: &H) -> Result<H::Fd, LockError> {
    let dir = resolve(host, Path::new(HOST_LOCK_DIR), Want::Directory).map_err(unreachable)?;
    let (fd, stat) = open_beneath(
        host,
        &dir.fd,
        dir.mount.id,
        Path::new(HOST_LOCK_NAME),
        Access::Path,
        Want::Any,
    )
    .map_err(unreachable)?;
    match stat.file_type {
        Some(FileType::Regular) => {}
        other => {
            return Err(LockError::Unusable {
                detail: format!(
                    "not a regular file ({}); it must be the file the tmpfiles line `{}` makes",
                    other.map_or_else(|| "type not reported".to_owned(), |t| format!("{t:?}")),
                    TMPFILES_LINES[1]
                ),
            });
        }
    }
    let readable = host.reopen_regular(&fd).map_err(LockError::Failed)?;
    drop(fd);
    Ok(readable)
}

/// Takes the host lock, waiting for it until `deadline` (`None`: no
/// deadline). See the module docs.
pub fn acquire<H: Host>(
    host: &H,
    deadline: Option<MonoTime>,
) -> Result<HostLock<H::Fd>, LockError> {
    if let Some(signal) = host.interrupted() {
        return Err(LockError::Interrupted(signal));
    }
    let started = host.now();
    let readable = open_lock(host)?;
    if let Some(signal) = host.interrupted() {
        return Err(LockError::Interrupted(signal));
    }
    // Read-only is enough for flock, and is what the file's mode (0644
    // root) gives every user (docs/safety.md, Host lock).
    let locked = match host
        .flock_wait(readable, LockMode::Exclusive, deadline)
        .map_err(LockError::Failed)?
    {
        WaitLock::Acquired(locked) => locked,
        WaitLock::DeadlinePassed(_) => return Err(LockError::DeadlinePassed),
        WaitLock::Interrupted(signal) => return Err(LockError::Interrupted(signal)),
    };
    if let Some(signal) = host.interrupted() {
        return Err(LockError::Interrupted(signal));
    }
    host.inherit_lock(&locked).map_err(LockError::Failed)?;
    Ok(HostLock {
        locked,
        waited: host.now().saturating_since(started),
    })
}

/// A resolve failure on the way to the lock file: missing is its own error
/// (the tmpfiles hint), unsafe or unsupported is a provisioning error, the
/// rest a host failure.
fn unreachable(e: ResolveError) -> LockError {
    match e {
        ResolveError::Missing { .. } => LockError::Missing {
            detail: e.to_string(),
        },
        ResolveError::UnsafePath { .. } | ResolveError::UnsupportedFs { .. } => {
            LockError::Unusable {
                detail: e.to_string(),
            }
        }
        ResolveError::Host(error) => LockError::Failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines are docs/safety.md's (Host lock), which is normative.
    #[test]
    fn the_tmpfiles_lines_are_the_docs() {
        let safety = crate::doc::safety_md();
        let section = crate::doc::section(&safety, "## Host lock");
        let block: Vec<String> = section
            .lines()
            .skip_while(|l| l.trim() != "```")
            .skip(1)
            .take_while(|l| l.trim() != "```")
            .map(|l| l.trim().to_owned())
            .collect();
        assert_eq!(block, TMPFILES_LINES);
    }

    #[test]
    fn the_missing_message_carries_both_tmpfiles_lines() {
        let text = LockError::Missing { detail: "x".into() }.to_string();
        for line in TMPFILES_LINES {
            assert!(text.contains(line), "{text}");
        }
        assert_eq!(host_lock_path(), Path::new("/run/diskgc/lock"));
    }

    /// TASK-151: the refusal prints commands an operator can paste, and
    /// they make exactly what [`TMPFILES_LINES`] say (run through `sh`, the
    /// printf writes the two lines; the manual form makes the same modes).
    #[test]
    fn missing_and_unusable_print_the_commands_that_fix_them() {
        for error in [
            LockError::Missing { detail: "x".into() },
            LockError::Unusable { detail: "x".into() },
        ] {
            let text = error.to_string();
            let commands: Vec<&str> = text
                .lines()
                .filter_map(|line| line.strip_prefix("  "))
                .collect();
            assert_eq!(
                commands,
                [
                    "printf '%s\\n' 'd /run/diskgc      0755 root root -' \
                     'f /run/diskgc/lock 0644 root root -' | sudo tee /etc/tmpfiles.d/diskgc.conf",
                    "sudo systemd-tmpfiles --create /etc/tmpfiles.d/diskgc.conf",
                    "sudo mkdir -p -m 0755 /run/diskgc && sudo touch /run/diskgc/lock \
                     && sudo chmod 0644 /run/diskgc/lock",
                ],
                "{text}"
            );
        }
    }
}
