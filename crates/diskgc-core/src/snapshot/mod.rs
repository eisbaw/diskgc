//! The index snapshot (docs/architecture.md, The tally: Snapshot): the
//! index saved by one run for the next to start from. A cache: a missing,
//! corrupt, mismatched or foreign snapshot is ignored with one log line and
//! costs only warmth; a loaded one only orders work (every candidate is
//! found again and checked live).
//!
//! Composed bottom-up:
//!
//! - [`format`]: the bytes, pure, with every structural check on the way in;
//! - [`restore`]: between the in-memory index (monotonic times) and a
//!   snapshot (wall-clock times);
//! - this module: [`RunIndex`], a run's index and whether it reads and
//!   writes the snapshot, with the two steps a reclaiming `ensure` takes:
//!   [`RunIndex::seed`] right before its synchronous tally and
//!   [`RunIndex::finish`] after the run, both through the [`Host`] port's
//!   state operations only.
//!
//! Not here: `watch`'s live index and its writes every 5 minutes
//! (TASK-32), the report's `index` block (TASK-244).

pub mod format;
pub mod restore;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use crate::host::{Dev, FileType, FsKey, Host, LockMode, MonoTime, WaitLock};
use crate::index::{BUDGET_BYTES, Finder, Index};
use crate::tally::TallyState;
use crate::units::format_bytes;

use format::{Header, Snapshot};
use restore::Clocks;

/// The largest snapshot file read: twice the index's budget
/// (docs/architecture.md, Snapshot: Format). A larger one is refused unread.
pub const MAX_SNAPSHOT_BYTES: u64 = 2 * BUDGET_BYTES as u64;

/// How long a writer waits for the single-writer lock: a lock its last
/// holder just released can look held for the length of another process's
/// `/proc` read (docs/safety.md, Host lock; up to 0.5 ms measured), so one
/// attempt is not enough; a lock really held (a `watch`, another `ensure`
/// writing) is not waited for longer than this.
pub const LOCK_WAIT: Duration = Duration::from_millis(100);

/// diskgc's state directory (docs/architecture.md, Snapshot):
/// `$XDG_STATE_HOME/diskgc`, or `$HOME/.local/state/diskgc` when that is
/// unset or not absolute (the XDG rule); `None` when neither gives an
/// absolute path. Pure: the binary passes its environment.
pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let absolute = |v: Option<&OsStr>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    absolute(xdg_state_home)
        .or_else(|| absolute(home).map(|h| h.join(".local/state")))
        .map(|base| base.join("diskgc"))
}

/// The key of the filesystem `dev`: its UUID where the host knows one,
/// otherwise its device number.
pub fn fs_key<H: Host>(host: &H, dev: Dev) -> Result<FsKey, String> {
    match host.fs_uuid(dev) {
        Ok(Some(uuid)) => FsKey::uuid(&uuid)
            .ok_or_else(|| format!("filesystem {dev} has an unusable UUID {uuid:?}")),
        Ok(None) => Ok(FsKey::dev(dev)),
        Err(e) => Err(format!("filesystem {dev}: its UUID: {e}")),
    }
}

/// Whether a run reads and writes the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Persist {
    /// Neither: an index a caller (a test) supplies, or none.
    No,
    /// Neither, said once: `--no-index`.
    NoIndex,
    /// Seeded from a valid snapshot; written after the run.
    Snapshot,
}

/// What [`RunIndex::seed`] keeps for [`RunIndex::finish`].
#[derive(Debug, Clone)]
struct Pending {
    key: FsKey,
    /// The filesystem the run reclaims, and its `--min-free` in bytes:
    /// the write never takes it below that.
    dev: Dev,
    min_free: u64,
    deadline: Option<MonoTime>,
}

/// A run's index (docs/architecture.md, The tally: Sources of a run's
/// index) and whether it reads and writes the snapshot.
#[derive(Debug)]
pub struct RunIndex {
    pub state: TallyState,
    persist: Persist,
    /// Set by [`Self::seed`] when the snapshot is to be written.
    pending: Option<Pending>,
}

impl RunIndex {
    /// An index of the run alone, from nothing. Neither reads nor writes
    /// the snapshot.
    pub fn ephemeral() -> Self {
        Self::of(TallyState::new(Index::new(BUDGET_BYTES)))
    }

    /// `--no-index`: [`Self::ephemeral`], and a run that tallies says so.
    pub fn no_index() -> Self {
        Self {
            persist: Persist::NoIndex,
            ..Self::ephemeral()
        }
    }

    /// `state` as the run's index (a test's seed); neither reads nor writes
    /// the snapshot.
    pub fn of(state: TallyState) -> Self {
        Self {
            state,
            persist: Persist::No,
            pending: None,
        }
    }

    /// Starts from the snapshot when valid, and writes it after the run.
    pub fn snapshot() -> Self {
        Self {
            persist: Persist::Snapshot,
            ..Self::ephemeral()
        }
    }

    /// Seeds the index from the snapshot of the filesystem `dev`, right
    /// before the synchronous tally of a run that tallies (and so will
    /// write it, [`Self::finish`]). `bound` holds the (profile, hash) of
    /// every source this run tallies: a snapshot finder of one of these
    /// profiles under another hash (the profile was edited) is dropped;
    /// finders of other profiles stay for the runs that use them.
    /// Anything wrong with the snapshot is one log line, and the run
    /// starts from nothing. Under `--no-index`, nothing.
    pub fn seed<H: Host>(
        &mut self,
        host: &H,
        dev: Dev,
        min_free: u64,
        deadline: Option<MonoTime>,
        bound: &BTreeMap<&str, u64>,
        log: &mut dyn FnMut(&str),
    ) {
        match self.persist {
            Persist::No => return,
            Persist::NoIndex => {
                return log(
                    "index: --no-index: the snapshot is neither read nor written; the \
                     synchronous tally starts from nothing",
                );
            }
            Persist::Snapshot => {}
        }
        let key = match fs_key(host, dev) {
            Ok(key) => key,
            Err(why) => {
                log(&format!(
                    "index: no snapshot ({why}); the synchronous tally starts from nothing and \
                     nothing is written"
                ));
                return;
            }
        };
        self.pending = Some(Pending {
            key: key.clone(),
            dev,
            min_free,
            deadline,
        });
        let name = key.snapshot_name();
        let ignored = |why: String| {
            format!(
                "index: snapshot {name} ignored: {why}; the synchronous tally starts from nothing"
            )
        };
        let file = match host.state_read(&key, MAX_SNAPSHOT_BYTES) {
            Ok(Some(file)) => file,
            Ok(None) => {
                log(&format!(
                    "index: no snapshot {name} yet; the synchronous tally starts from nothing"
                ));
                return;
            }
            Err(e) => return log(&ignored(e.to_string())),
        };
        let uid = host.effective_uid();
        let bytes = match (file.file_type, file.bytes) {
            (FileType::Regular, _) if file.uid != uid => {
                return log(&ignored(format!(
                    "owned by uid {}, not by this user ({uid})",
                    file.uid
                )));
            }
            (FileType::Regular, _) if file.mode & 0o077 != 0 => {
                return log(&ignored(format!(
                    "mode {:04o}: others may read or write it (diskgc writes 0600)",
                    file.mode
                )));
            }
            (FileType::Regular, Some(bytes)) => bytes,
            (FileType::Regular, None) => {
                return log(&ignored(format!(
                    "{} bytes, more than the {MAX_SNAPSHOT_BYTES} a snapshot can be (twice the \
                     index's budget); not read",
                    file.size
                )));
            }
            (other, _) => return log(&ignored(format!("a {other}, not a regular file"))),
        };
        let snapshot = match format::decode(&bytes).and_then(|s| fits(host, &key, s)) {
            Ok(snapshot) => snapshot,
            Err(why) => return log(&ignored(why)),
        };
        let clocks = Clocks::now(host);
        let keep = |f: &Finder| bound.get(&*f.profile).is_none_or(|h| *h == f.profile_hash);
        match restore::restore(
            host,
            &snapshot,
            clocks,
            self.state.index.budget_bytes(),
            keep,
        ) {
            Ok((state, restored)) => {
                self.state = state;
                let age = restore_age(clocks, snapshot.header.written);
                let stats = self.state.index.stats();
                let mut line = format!(
                    "index: started from snapshot {name} written {age} ago: {} unit(s), {} of \
                     them blocks",
                    stats.singles + stats.blocks,
                    stats.blocks,
                );
                if restored.finders_dropped > 0 {
                    line += &format!(
                        "; {} finder(s) of edited profiles dropped with {} unit(s)",
                        restored.finders_dropped, restored.units_dropped
                    );
                }
                if restored.no_room > 0 {
                    line += &format!("; {} unit(s) left out (no room)", restored.no_room);
                }
                log(&line);
            }
            Err(e) => {
                self.state = TallyState::new(Index::new(self.state.index.budget_bytes()));
                log(&ignored(format!("restoring it failed: {e}")));
            }
        }
    }

    /// Writes the snapshot after a run that seeded it ([`Self::seed`]):
    /// not after a terminating signal, not at or past the run's deadline,
    /// not while another writer holds the single-writer lock, and not when
    /// the state directory is on the reclaimed filesystem and the write
    /// would take it below `--min-free`. Each skip or failure is one log
    /// line and changes nothing else (docs/architecture.md, Snapshot).
    pub fn finish<H: Host>(&mut self, host: &H, log: &mut dyn FnMut(&str)) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let name = pending.key.snapshot_name();
        let skipped = |why: &str| format!("index: snapshot {name} not written: {why}");
        if host.interrupted().is_some() {
            return log(&skipped("the run was interrupted"));
        }
        let past = |host: &H| pending.deadline.is_some_and(|d| host.now() >= d);
        if past(host) {
            return log(&skipped("the run's deadline has passed"));
        }
        let header = Header {
            diskgc_version: env!("CARGO_PKG_VERSION").to_owned(),
            key: pending.key.clone(),
            uid: host.effective_uid(),
            boot_id: match host.boot_id() {
                Ok(id) => id,
                Err(e) => return log(&skipped(&format!("the boot id: {e}"))),
            },
            written: host.wall_clock(),
        };
        let clocks = Clocks::now(host);
        let snapshot = restore::capture(
            &self.state,
            clocks,
            Header {
                written: clocks.wall,
                ..header
            },
        );
        let bytes = format::encode(&snapshot);
        let lock = match host.state_lock(&pending.key) {
            Ok(lock) => lock,
            Err(e) => return log(&skipped(&e.to_string())),
        };
        let dir_dev = lock.dir_dev;
        let wait_until = host.now().checked_add(LOCK_WAIT);
        let until = match (pending.deadline, wait_until) {
            (Some(d), Some(w)) => Some(d.min(w)),
            (d, w) => d.or(w),
        };
        let locked = match host.flock_wait(lock.fd, LockMode::Exclusive, until) {
            Ok(WaitLock::Acquired(locked)) => locked,
            Ok(WaitLock::DeadlinePassed(_)) => {
                return log(&skipped(&format!(
                    "{} is held by another writer (a watch, or an ensure writing)",
                    pending.key.lock_name()
                )));
            }
            Ok(WaitLock::Interrupted(_)) => return log(&skipped("the run was interrupted")),
            Err(e) => return log(&skipped(&e.to_string())),
        };
        if dir_dev == pending.dev {
            let size = u64::try_from(bytes.len()).expect("fits u64");
            match host.statvfs(locked.fd()) {
                Ok(space) if space.avail_bytes.saturating_sub(size) >= pending.min_free => {}
                Ok(space) => {
                    return log(&skipped(&format!(
                        "the state directory is on the reclaimed filesystem, and {} would take \
                         its {} available below --min-free {}",
                        format_bytes(size),
                        format_bytes(space.avail_bytes),
                        format_bytes(pending.min_free)
                    )));
                }
                Err(e) => return log(&skipped(&e.to_string())),
            }
        }
        if past(host) {
            return log(&skipped("the run's deadline has passed"));
        }
        match host.state_write(&locked, &pending.key, &bytes) {
            Ok(()) => {
                let stats = self.state.index.stats();
                log(&format!(
                    "index: snapshot {name} written: {} unit(s), {}",
                    stats.singles + stats.blocks,
                    format_bytes(u64::try_from(bytes.len()).expect("fits u64"))
                ));
            }
            Err(e) => log(&skipped(&e.to_string())),
        }
    }
}

/// `snapshot` if this run may use it (docs/architecture.md, Snapshot:
/// Validity): written by this build, for this filesystem, by this user,
/// and for a filesystem without a UUID in this boot.
fn fits<H: Host>(host: &H, key: &FsKey, snapshot: Snapshot) -> Result<Snapshot, String> {
    let h = &snapshot.header;
    let version = env!("CARGO_PKG_VERSION");
    if h.diskgc_version != version {
        return Err(format!(
            "written by diskgc {}, this is {version}",
            h.diskgc_version
        ));
    }
    if h.key != *key {
        return Err(format!("written for filesystem {}", h.key));
    }
    let uid = host.effective_uid();
    if h.uid != uid {
        return Err(format!("written by uid {}, not this user ({uid})", h.uid));
    }
    if !key.has_uuid() {
        let boot = host.boot_id().map_err(|e| format!("the boot id: {e}"))?;
        if h.boot_id != boot {
            return Err(
                "written in another boot, for a filesystem without a UUID (its device and \
                 inode numbers repeat after a reboot)"
                    .to_owned(),
            );
        }
    }
    Ok(snapshot)
}

/// How long before the load the snapshot was written, for the log.
fn restore_age(clocks: Clocks, written: crate::host::Timestamp) -> String {
    let secs = clocks.wall.secs.saturating_sub(written.secs).max(0);
    match secs {
        s if s < 120 => format!("{s} s"),
        s if s < 7200 => format!("{} min", s / 60),
        s => format!("{} h", s / 3600),
    }
}
