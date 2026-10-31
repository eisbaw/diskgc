//! The lock a reclaim takes (docs/safety.md, Host lock, the rule's one
//! home). With the provisioned host lock, an exclusive `flock` on the
//! fixed path `/run/diskgc/lock` that whatever installs diskgc provisions
//! (the tmpfiles lines in [`TMPFILES_LINES`]), one reclaim runs per host at
//! a time. A non-root run on a host without it (the file or `/run/diskgc`
//! missing) takes a per-user lock instead ([`Host::user_lock`], at
//! [`Host::user_lock_path`]), which serialises only that user's reclaims;
//! where that lock cannot be made for a reason of the environment (no
//! place for it, a full or read-only filesystem, no permission on the way:
//! [`environmental`]) the reclaim runs without a lock, saying so
//! ([`ReclaimLock::Unlocked`]). The per-user lock only serialises; it is
//! not a safety boundary. Something unsafe at its path is refused
//! ([`LockError::UserLockRefused`]). Root (and so `--allow-root`, which is
//! for root) never falls back. Anything else at the host lock's path (not a
//! regular file, reached through a symlink) is refused for everyone, never
//! fallen back from. Which lock it is, is decided afresh at every use.
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

use std::fmt;
use std::path::{Path, PathBuf};

use crate::host::{
    Access, Errno, FileType, Host, HostError, LockMode, Locked, MakeUserLock, MonoTime, TryLock,
    UserLockError, WaitLock,
};
use crate::report::Signal;
use crate::resolve::{ResolveError, Want, open_beneath, resolve};

/// The directory of the host lock. Fixed, not configurable: two configs
/// naming two lock files would silently break mutual exclusion. Reclaims
/// are serialised host-wide only with the host lock provisioned; without
/// it each user's only among themselves (docs/safety.md, Host lock).
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

/// The per-user lock's file name in its directory.
pub const USER_LOCK_NAME: &str = "lock";

/// Where the per-user lock is, when the user has a runtime directory
/// (`/run/user/<uid>`, the user's own, mode 0700: the host decides that):
/// `/run/user/<uid>/diskgc/lock` (docs/safety.md, Host lock).
pub fn runtime_user_lock(uid: u32) -> PathBuf {
    Path::new("/run/user")
        .join(uid.to_string())
        .join("diskgc")
        .join(USER_LOCK_NAME)
}

/// Where the per-user lock is otherwise: `<home>/.local/state/diskgc/lock`,
/// the home being the user's account entry's (`$HOME` only when that
/// lookup fails; the host decides).
pub fn home_user_lock(home: &Path) -> PathBuf {
    home.join(".local/state/diskgc").join(USER_LOCK_NAME)
}

/// Why a reclaim runs without a lock: the per-user lock at `path` (`None`:
/// no place for one) cannot be made, for a reason of the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unlocked {
    pub path: Option<PathBuf>,
    pub why: String,
}

/// Which lock a reclaim takes: the provisioned host lock, this user's
/// per-user lock at its path, or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimLock {
    Host,
    User(PathBuf),
    /// The host lock is not provisioned and the per-user lock cannot be
    /// made here: the reclaim runs unserialised (TASK-277: a user with a
    /// full disk must still be able to reclaim).
    Unlocked(Unlocked),
}

impl ReclaimLock {
    /// The lock's path, for an overrun entry: the host lock's for
    /// `Unlocked` without a place.
    pub fn path(&self) -> PathBuf {
        match self {
            Self::Host => host_lock_path(),
            Self::User(path)
            | Self::Unlocked(Unlocked {
                path: Some(path), ..
            }) => path.clone(),
            Self::Unlocked(Unlocked { path: None, .. }) => host_lock_path(),
        }
    }
}

impl fmt::Display for ReclaimLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host => write!(f, "host lock {}", host_lock_path().display()),
            Self::User(path) => write!(f, "per-user lock {}", path.display()),
            Self::Unlocked(Unlocked { path, why }) => match path {
                Some(path) => write!(f, "no lock (the per-user lock {}: {why})", path.display()),
                None => write!(f, "no lock ({why})"),
            },
        }
    }
}

/// What a reclaim would lock, as [`readiness`] finds it without taking or
/// making anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// The provisioned host lock is there and usable.
    Host,
    /// It is not provisioned, and this non-root run falls back to its
    /// per-user lock at this path (made, if need be, by the reclaim);
    /// `None`: no place for one, so the reclaim would run without a lock.
    /// Whether the per-user lock is usable is not checked.
    FallsBack(Option<PathBuf>),
}

/// Whether a failure making the per-user lock is the environment's (no
/// space or quota, a read-only filesystem, no permission or a non-directory
/// on the way): the reclaim then runs without a lock. Any other errno (`EIO`,
/// `ENOMEM`, `EMFILE`, ...) is a failure of the host.
pub fn environmental(error: &HostError) -> bool {
    error.kernel_errno().is_some_and(|errno| {
        [
            Errno::ENOSPC,
            Errno::EDQUOT,
            Errno::EROFS,
            Errno::EACCES,
            Errno::EPERM,
            Errno::ENOTDIR,
        ]
        .contains(&errno)
    })
}

/// The held lock. Dropping it closes the fd, which releases the lock once
/// every child that inherited a copy has closed its copy too. Holds no
/// lock when [`ReclaimLock::Unlocked`].
#[derive(Debug)]
pub struct HeldLock<F> {
    locked: Option<Locked<F>>,
    /// Which lock it is.
    pub lock: ReclaimLock,
    /// How long [`acquire`] waited for it.
    pub waited: std::time::Duration,
}

impl<F> HeldLock<F> {
    /// The held fd (for the record only: it must never be `flock`ed again).
    pub fn locked(&self) -> Option<&Locked<F>> {
        self.locked.as_ref()
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
    /// The host lock is not provisioned and something unsafe stands at
    /// the per-user lock's path (a symlink, another type, another owner,
    /// open to others): a provisioning error (exit 2).
    UserLockRefused { path: PathBuf, error: HostError },
    /// Another reclaim held this lock until the deadline.
    DeadlinePassed(ReclaimLock),
    /// SIGINT or SIGTERM arrived before the lock could be used.
    Interrupted(Signal),
    /// The host failed on this lock (opening, making, reopening, locking,
    /// `fcntl`): exit 3, cause `host`.
    Failed(ReclaimLock, HostError),
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
            Self::UserLockRefused { path: user, error } => write!(
                f,
                "the host lock {} is not provisioned, and the per-user lock {} that a reclaim \
                 by this user takes instead is unsafe: {error}. Make {} a directory of yours \
                 writable by no one else, holding `{USER_LOCK_NAME}` as a regular file of \
                 yours that no one else may open (mode 0600), neither a symlink, or remove \
                 them and diskgc makes them (docs/safety.md, Host lock); or have root \
                 provision the host lock, which serialises every user's reclaims. {}",
                path.display(),
                user.display(),
                user.parent().unwrap_or(user).display(),
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
            Self::Failed(lock, e) => write!(f, "taking the {lock}: {e}"),
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
/// missing a non-root run falls back ([`Readiness::FallsBack`], with
/// [`Host::user_lock_path`]) without making or checking its per-user lock.
/// An already-met `ensure` and a dry run use this to report a broken
/// installation early.
pub fn readiness<H: Host>(host: &H) -> Result<Readiness, LockError> {
    match open_host_lock(host) {
        Ok(fd) => {
            drop(fd);
            Ok(Readiness::Host)
        }
        Err(LockError::Missing { .. }) if may_fall_back(host) => {
            Ok(Readiness::FallsBack(host.user_lock_path()))
        }
        Err(e) => Err(e),
    }
}

/// Opens the lock a reclaim takes, making the per-user lock if this run
/// falls back to it, without taking it: a reclaiming `watch` checks at
/// start that its cycles can lock, so an unsafe per-user lock stops it
/// there (exit 2). `Unlocked`: its cycles will run without a lock.
pub fn prepare<H: Host>(host: &H) -> Result<ReclaimLock, LockError> {
    Ok(match open_lock(host)? {
        Opened::Lock(fd, lock) => {
            drop(fd);
            lock
        }
        Opened::Unlocked(unlocked) => ReclaimLock::Unlocked(unlocked),
    })
}

/// What [`open_lock`] found.
enum Opened<F> {
    /// The lock, open and readable for `flock`.
    Lock(F, ReclaimLock),
    /// No lock can be had: the reclaim runs without one.
    Unlocked(Unlocked),
}

/// The lock a reclaim takes: the host lock ([`open_host_lock`]), or, when
/// it is missing and this run is not root, the per-user lock
/// ([`Host::user_lock`], made if absent); when that cannot be made for a
/// reason of the environment ([`environmental`], or no place for it), none.
fn open_lock<H: Host>(host: &H) -> Result<Opened<H::Fd>, LockError> {
    match open_host_lock(host) {
        Ok(fd) => Ok(Opened::Lock(fd, ReclaimLock::Host)),
        Err(LockError::Missing { .. }) if may_fall_back(host) => {
            match host.user_lock(MakeUserLock::IfAbsent) {
                Ok(Some(opened)) => Ok(Opened::Lock(opened.fd, ReclaimLock::User(opened.path))),
                Ok(None) => Err(LockError::Failed(
                    ReclaimLock::Host,
                    HostError::invalid("user_lock", "the per-user lock", "made nothing"),
                )),
                Err(UserLockError::NoPlace) => Ok(Opened::Unlocked(Unlocked {
                    path: None,
                    why: UserLockError::NoPlace.to_string(),
                })),
                Err(UserLockError::Unsafe { path, error }) => {
                    Err(LockError::UserLockRefused { path, error })
                }
                Err(UserLockError::Failed { path, error }) if environmental(&error) => {
                    Ok(Opened::Unlocked(Unlocked {
                        path: Some(path),
                        why: error.to_string(),
                    }))
                }
                Err(UserLockError::Failed { path, error }) => {
                    Err(LockError::Failed(ReclaimLock::User(path), error))
                }
            }
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
    let readable = host
        .reopen_regular(&fd)
        .map_err(|e| LockError::Failed(ReclaimLock::Host, e))?;
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
    let (readable, lock) = match open_lock(host)? {
        Opened::Lock(fd, lock) => (fd, lock),
        Opened::Unlocked(unlocked) => {
            return Ok(HeldLock {
                locked: None,
                lock: ReclaimLock::Unlocked(unlocked),
                waited: host.now().saturating_since(started),
            });
        }
    };
    if let Some(signal) = host.interrupted() {
        return Err(LockError::Interrupted(signal));
    }
    // Read-only is enough for flock, and is what the host lock's mode
    // (0644 root) gives every user (docs/safety.md, Host lock).
    let locked = match host
        .flock_wait(readable, LockMode::Exclusive, deadline)
        .map_err(|e| LockError::Failed(lock.clone(), e))?
    {
        WaitLock::Acquired(locked) => locked,
        WaitLock::DeadlinePassed(_) => return Err(LockError::DeadlinePassed(lock)),
        WaitLock::Interrupted(signal) => return Err(LockError::Interrupted(signal)),
    };
    if let Some(signal) = host.interrupted() {
        return Err(LockError::Interrupted(signal));
    }
    host.inherit_lock(&locked)
        .map_err(|e| LockError::Failed(lock.clone(), e))?;
    Ok(HeldLock {
        locked: Some(locked),
        lock,
        waited: host.now().saturating_since(started),
    })
}

/// Whether another process (or another part of this one) holds the lock a
/// reclaim of this run's user takes (the host lock, or for a non-root run
/// without it the per-user lock), tested without waiting, without taking it
/// for longer than the test and without making anything: a shared `flock`
/// attempt on a fresh open of the lock file, released at once by closing
/// it. Busy means a reclaim holds it (they take it exclusively): which lock
/// is returned. A per-user lock that does not exist (or has no place) is
/// held by no one. For the background tally's pause (docs/architecture.md,
/// The tally: Pausing), which polls: a lock released by its holder can
/// still look held for a moment (docs/safety.md, Host lock). A reclaim
/// trying the lock in that instant finds it busy once and tries again (it
/// waits by polling, [`Host::flock_wait`]).
pub fn held_by_another<H: Host>(host: &H) -> Result<Option<ReclaimLock>, LockError> {
    let (readable, lock) = match open_host_lock(host) {
        Ok(fd) => (fd, ReclaimLock::Host),
        Err(LockError::Missing { .. }) if may_fall_back(host) => {
            match host.user_lock(MakeUserLock::Never) {
                Ok(Some(opened)) => (opened.fd, ReclaimLock::User(opened.path)),
                Ok(None) | Err(UserLockError::NoPlace) => return Ok(None),
                Err(UserLockError::Unsafe { path, error }) => {
                    return Err(LockError::UserLockRefused { path, error });
                }
                Err(UserLockError::Failed { path, error }) => {
                    return Err(LockError::Failed(ReclaimLock::User(path), error));
                }
            }
        }
        Err(e) => return Err(e),
    };
    match host
        .flock_try(readable, LockMode::Shared)
        .map_err(|e| LockError::Failed(lock.clone(), e))?
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
        ResolveError::Host(error) => LockError::Failed(ReclaimLock::Host, error),
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

    /// TASK-277: the per-user lock's two places, from the uid and the home
    /// only (never the environment's `XDG_*`).
    #[test]
    fn the_per_user_lock_paths() {
        assert_eq!(
            runtime_user_lock(1000),
            Path::new("/run/user/1000/diskgc/lock")
        );
        assert_eq!(
            home_user_lock(Path::new("/home/u")),
            Path::new("/home/u/.local/state/diskgc/lock")
        );
    }

    /// The environment's failures run the reclaim unlocked; the host's
    /// are failures.
    #[test]
    fn environmental_errnos() {
        for errno in [
            Errno::ENOSPC,
            Errno::EDQUOT,
            Errno::EROFS,
            Errno::EACCES,
            Errno::EPERM,
            Errno::ENOTDIR,
        ] {
            assert!(
                environmental(&HostError::errno("mkdir", "x", errno)),
                "{errno}"
            );
        }
        for errno in [Errno::EIO, Errno::ENOMEM, Errno::EMFILE, Errno::ELOOP] {
            assert!(
                !environmental(&HostError::errno("mkdir", "x", errno)),
                "{errno}"
            );
        }
        assert!(!environmental(&HostError::invalid(
            "open",
            "x",
            "a symlink"
        )));
    }

    /// The refusal of a per-user lock names it and how to fix it, and
    /// still offers the provisioning commands, for whoever is root.
    #[test]
    fn a_refused_per_user_lock_says_how_to_fix_it() {
        let text = LockError::UserLockRefused {
            path: PathBuf::from("/run/user/1000/diskgc/lock"),
            error: HostError::invalid("open", "/run/user/1000/diskgc", "x"),
        }
        .to_string();
        assert!(text.contains("not provisioned"), "{text}");
        assert!(
            text.contains("per-user lock /run/user/1000/diskgc/lock"),
            "{text}"
        );
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
