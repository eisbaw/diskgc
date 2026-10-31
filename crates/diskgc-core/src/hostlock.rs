//! The lock a reclaim takes (docs/safety.md, Host lock). The provisioned
//! host lock, an exclusive `flock` on the fixed path `/run/diskgc/lock`
//! that whatever installs diskgc provisions (the tmpfiles lines in
//! [`TMPFILES_LINES`]), lets one reclaim run per host at a time. A non-root
//! run on a host without it (the file or `/run/diskgc` missing) takes a
//! per-user lock instead, which diskgc makes itself ([`Host::user_lock`],
//! in [`user_lock_dir`]): one reclaim per user at a time, and reclaims of
//! different users are not serialised. Root (and so `--allow-root`, which
//! is for root) never falls back. Anything else at the host lock's path
//! (not a regular file, reached through a symlink) is refused for everyone,
//! never fallen back from. Which lock it is, is decided afresh at every
//! use ([`ReclaimLock`]).
//!
//! [`acquire`] reaches the host lock without following a symlink or
//! crossing a mount ([`resolve`] of `/run/diskgc`, then the name below
//! it), checks that it is a regular file on the `O_PATH` fd and reopens
//! that object read-only through the fd ([`Host::reopen_regular`]), so a
//! FIFO or a device standing at the path is never opened. It then waits
//! for the lock until the deadline ([`Host::flock_wait`]: non-blocking
//! attempts, so the deadline and an observed interruption both stop the
//! wait) and makes the held fd inheritable by every child
//! ([`Host::inherit_lock`]), so the lock stays held until the last child
//! exits. It is released only by dropping the [`HeldLock`] (closing the
//! fd); the port has no unlock. The per-user lock is held the same way.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::host::{
    Access, Cause, FileType, Host, HostError, LockMode, Locked, MonoTime, TryLock, WaitLock,
};
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
/// next boot recreates `/run/diskgc`, and applied now. `install -D`, not
/// `tee`: a minimal system has no `/etc/tmpfiles.d` yet (TASK-215, a clean
/// Debian container), and `tee` cannot create it. Then the plain
/// `mkdir`/`touch`/`chmod` for a host without systemd-tmpfiles.
struct FixIt;

impl fmt::Display for FixIt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "To provision it now and at every boot, install the tmpfiles.d drop-in the \
             package ships (lib/tmpfiles.d/diskgc.conf) and apply it:\n  \
             printf '%s\\n' '{}' '{}' | sudo install -Dm0644 /dev/stdin /etc/tmpfiles.d/diskgc.conf\n  \
             sudo systemd-tmpfiles --create /etc/tmpfiles.d/diskgc.conf\n\
             Without systemd-tmpfiles (until the next boot, /run is emptied then):\n  \
             sudo mkdir -p -m 0755 {HOST_LOCK_DIR} && sudo touch {lock} && sudo chmod 0644 {lock}",
            TMPFILES_LINES[0],
            TMPFILES_LINES[1],
            lock = host_lock_path().display(),
        )
    }
}

/// The per-user lock's file name in [`user_lock_dir`].
pub const USER_LOCK_NAME: &str = "lock";

/// The directory of the per-user lock (docs/safety.md, Host lock):
/// `$XDG_RUNTIME_DIR/diskgc` when `runtime_dir` (`$XDG_RUNTIME_DIR`) is
/// absolute, otherwise `state_dir`, diskgc's state directory; `None`
/// without either. The binary decides it from its environment and gives
/// it to `LinuxHost`; the model derives it the same way.
pub fn user_lock_dir(runtime_dir: Option<&OsStr>, state_dir: Option<&Path>) -> Option<PathBuf> {
    match runtime_dir.map(Path::new) {
        Some(dir) if dir.is_absolute() => Some(dir.join("diskgc")),
        _ => state_dir.map(Path::to_path_buf),
    }
}

/// Which lock a reclaim takes: the provisioned host lock, or this user's
/// per-user lock at its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimLock {
    Host,
    User(PathBuf),
}

impl ReclaimLock {
    pub fn path(&self) -> PathBuf {
        match self {
            Self::Host => host_lock_path(),
            Self::User(path) => path.clone(),
        }
    }
}

impl fmt::Display for ReclaimLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host => write!(f, "host lock {}", host_lock_path().display()),
            Self::User(path) => write!(f, "per-user lock {}", path.display()),
        }
    }
}

/// What a reclaim would lock, as [`readiness`] finds it without taking or
/// making anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// The provisioned host lock is there and usable.
    Host,
    /// It is not provisioned, and this non-root run falls back to its
    /// per-user lock (made, if need be, by the reclaim).
    FallsBack,
}

/// The held lock. Dropping it closes the fd, which releases the lock once
/// every child that inherited a copy has closed its copy too.
#[derive(Debug)]
pub struct HeldLock<F> {
    locked: Locked<F>,
    /// Which lock it is.
    pub lock: ReclaimLock,
    /// How long [`acquire`] waited for it.
    pub waited: std::time::Duration,
}

impl<F> HeldLock<F> {
    /// The held fd (for the record only: it must never be `flock`ed again).
    pub fn locked(&self) -> &Locked<F> {
        &self.locked
    }
}

/// Why the lock was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    /// `/run/diskgc/lock` (or its directory) does not exist: not
    /// provisioned, and this run is root, which never falls back (exit 2,
    /// printing [`TMPFILES_LINES`]).
    Missing { detail: String },
    /// Something other than a regular file stands at the path, or it cannot
    /// be reached safely (a symlink, a mount crossing): a provisioning
    /// error too (exit 2), for every user.
    Unusable { detail: String },
    /// The host lock is not provisioned and the per-user lock was refused
    /// ([`Host::user_lock`]'s [`Cause::Invalid`]: a symlink, another
    /// user's, open to others, or no place for it): a provisioning error
    /// (exit 2).
    UserLockRefused(HostError),
    /// Another reclaim held this lock until the deadline.
    DeadlinePassed(ReclaimLock),
    /// SIGINT or SIGTERM arrived before the lock could be used.
    Interrupted(Signal),
    /// The host failed (opening, making, reopening, locking, `fcntl`).
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
            Self::UserLockRefused(e) => write!(
                f,
                "the host lock {} is not provisioned, and the per-user lock a reclaim by \
                 this user takes instead is unusable: {e}. Fix the per-user lock (a directory \
                 of yours, mode 0700, holding the regular file `{USER_LOCK_NAME}` of yours, \
                 mode 0600, neither a symlink; in $XDG_RUNTIME_DIR/diskgc, otherwise in the \
                 state directory $XDG_STATE_HOME/diskgc or ~/.local/state/diskgc), or have \
                 root provision the host lock, which serialises every user's reclaims. {}",
                path.display(),
                FixIt
            ),
            Self::DeadlinePassed(lock) => write!(
                f,
                "the {lock} was still held by another reclaim at the deadline"
            ),
            Self::Interrupted(signal) => write!(
                f,
                "interrupted by {} while waiting for the reclaim lock",
                signal.name()
            ),
            Self::Failed(e) => write!(f, "taking the reclaim lock: {e}"),
        }
    }
}

impl std::error::Error for LockError {}

/// Whether a missing host lock falls back to the per-user lock: only for a
/// run that is not root (`--allow-root` is for root, docs/safety.md,
/// Privileges, so it never falls back either).
fn may_fall_back<H: Host>(host: &H) -> bool {
    host.effective_uid() != 0
}

/// What a reclaim would lock, checked without taking or making anything:
/// the provisioned host lock is opened as [`acquire`] opens it; when it is
/// missing a non-root run falls back ([`Readiness::FallsBack`]) without
/// making its per-user lock (a state write). An already-met `ensure` and
/// a dry run use this to report a broken installation early.
pub fn readiness<H: Host>(host: &H) -> Result<Readiness, LockError> {
    match open_host_lock(host) {
        Ok(fd) => {
            drop(fd);
            Ok(Readiness::Host)
        }
        Err(LockError::Missing { .. }) if may_fall_back(host) => Ok(Readiness::FallsBack),
        Err(e) => Err(e),
    }
}

/// Opens the lock a reclaim takes, making the per-user lock if this run
/// falls back to it, without taking it: `watch` checks at start that its
/// cycles can lock, so a refused per-user lock stops it there (exit 2).
pub fn prepare<H: Host>(host: &H) -> Result<ReclaimLock, LockError> {
    open_lock(host).map(|(fd, lock)| {
        drop(fd);
        lock
    })
}

/// The lock a reclaim takes, open and readable for `flock`: the host lock
/// ([`open_host_lock`]), or, when it is missing and this run is not root,
/// the per-user lock ([`Host::user_lock`], made if absent).
fn open_lock<H: Host>(host: &H) -> Result<(H::Fd, ReclaimLock), LockError> {
    match open_host_lock(host) {
        Ok(fd) => Ok((fd, ReclaimLock::Host)),
        Err(LockError::Missing { .. }) if may_fall_back(host) => {
            let opened = host.user_lock().map_err(|e| match e.cause {
                Cause::Invalid => LockError::UserLockRefused(e),
                Cause::Errno(_) | Cause::SandboxGate => LockError::Failed(e),
            })?;
            Ok((opened.fd, ReclaimLock::User(opened.path)))
        }
        Err(e) => Err(e),
    }
}

/// Reach and reopen exactly the host lock object that [`acquire`] will
/// flock.
fn open_host_lock<H: Host>(host: &H) -> Result<H::Fd, LockError> {
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
                    other.map_or_else(|| "type not reported".to_owned(), |t| t.to_string()),
                    TMPFILES_LINES[1]
                ),
            });
        }
    }
    let readable = host.reopen_regular(&fd).map_err(LockError::Failed)?;
    drop(fd);
    Ok(readable)
}

/// Takes the lock a reclaim takes, waiting for it until `deadline`
/// (`None`: no deadline). See the module docs.
pub fn acquire<H: Host>(
    host: &H,
    deadline: Option<MonoTime>,
) -> Result<HeldLock<H::Fd>, LockError> {
    if let Some(signal) = host.interrupted() {
        return Err(LockError::Interrupted(signal));
    }
    let started = host.now();
    let (readable, lock) = open_lock(host)?;
    if let Some(signal) = host.interrupted() {
        return Err(LockError::Interrupted(signal));
    }
    // Read-only is enough for flock, and is what the host lock's mode
    // (0644 root) gives every user (docs/safety.md, Host lock).
    let locked = match host
        .flock_wait(readable, LockMode::Exclusive, deadline)
        .map_err(LockError::Failed)?
    {
        WaitLock::Acquired(locked) => locked,
        WaitLock::DeadlinePassed(_) => return Err(LockError::DeadlinePassed(lock)),
        WaitLock::Interrupted(signal) => return Err(LockError::Interrupted(signal)),
    };
    if let Some(signal) = host.interrupted() {
        return Err(LockError::Interrupted(signal));
    }
    host.inherit_lock(&locked).map_err(LockError::Failed)?;
    Ok(HeldLock {
        locked,
        lock,
        waited: host.now().saturating_since(started),
    })
}

/// Whether another process (or another part of this one) holds the lock a
/// reclaim of this run's user takes ([`open_lock`]: the host lock, or the
/// per-user lock, made if absent), tested without waiting and without
/// taking it for longer than the test: a shared `flock` attempt on a fresh
/// open of the lock file, released at once by closing it. Busy means a
/// reclaim holds it (they take it exclusively): which lock is returned. For the background tally's pause (docs/architecture.md,
/// The tally: Pausing), which polls: a lock released by its holder can
/// still look held for a moment (docs/safety.md, Host lock). A reclaim
/// trying the lock in that instant finds it busy once and tries again (it
/// waits by polling, [`Host::flock_wait`]).
pub fn held_by_another<H: Host>(host: &H) -> Result<Option<ReclaimLock>, LockError> {
    let (readable, lock) = open_lock(host)?;
    match host
        .flock_try(readable, LockMode::Shared)
        .map_err(LockError::Failed)?
    {
        TryLock::Acquired(probe) => {
            drop(probe);
            Ok(None)
        }
        TryLock::Busy(_) => Ok(Some(lock)),
    }
}

/// A resolve failure on the way to the lock file: missing is its own error
/// (the tmpfiles hint), unsafe or unsupported is a provisioning error, the
/// rest a host failure.
fn unreachable(e: ResolveError) -> LockError {
    match e {
        ResolveError::Missing { .. } => LockError::Missing {
            detail: e.to_string(),
        },
        ResolveError::UnsafePath { .. }
        | ResolveError::UnsupportedFs { .. }
        | ResolveError::Screened { .. } => LockError::Unusable {
            detail: e.to_string(),
        },
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

    /// TASK-277: `$XDG_RUNTIME_DIR/diskgc` when it is absolute, otherwise
    /// the state directory; an empty or relative value counts as unset.
    #[test]
    fn the_per_user_lock_directory_prefers_an_absolute_runtime_dir() {
        let state = Path::new("/home/u/.local/state/diskgc");
        let runtime = |v: &str| user_lock_dir(Some(OsStr::new(v)), Some(state));
        assert_eq!(
            runtime("/run/user/1000"),
            Some(PathBuf::from("/run/user/1000/diskgc"))
        );
        for unusable in ["", "run/user/1000"] {
            assert_eq!(runtime(unusable), Some(state.to_path_buf()), "{unusable:?}");
        }
        assert_eq!(user_lock_dir(None, Some(state)), Some(state.to_path_buf()));
        assert_eq!(user_lock_dir(Some(OsStr::new("rel")), None), None);
        assert_eq!(user_lock_dir(None, None), None);
    }

    /// The refusal of a per-user lock names it and how to fix it, and
    /// still offers the provisioning commands, for whoever is root.
    #[test]
    fn a_refused_per_user_lock_says_how_to_fix_it() {
        let text =
            LockError::UserLockRefused(HostError::invalid("open", "/run/user/1000/diskgc", "x"))
                .to_string();
        assert!(text.contains("not provisioned"), "{text}");
        assert!(text.contains("per-user lock"), "{text}");
        assert!(text.contains("open /run/user/1000/diskgc: x"), "{text}");
        assert!(text.contains(TMPFILES_LINES[1]), "{text}");
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
                     'f /run/diskgc/lock 0644 root root -' \
                     | sudo install -Dm0644 /dev/stdin /etc/tmpfiles.d/diskgc.conf",
                    "sudo systemd-tmpfiles --create /etc/tmpfiles.d/diskgc.conf",
                    "sudo mkdir -p -m 0755 /run/diskgc && sudo touch /run/diskgc/lock \
                     && sudo chmod 0644 /run/diskgc/lock",
                ],
                "{text}"
            );
        }
    }
}
