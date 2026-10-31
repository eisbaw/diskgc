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

/// The version a snapshot must have been written by: this build's.
pub const DISKGC_VERSION: &str = env!("CARGO_PKG_VERSION");

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
/// otherwise its device number. A UUID that cannot be read or used (an
/// `EACCES` on the udev links, an escaped name) falls back to the device
/// number with the reason, for the log: only warmth is at stake.
pub fn fs_key<H: Host>(host: &H, dev: Dev) -> (FsKey, Option<String>) {
    match host.fs_uuid(dev) {
        Ok(Some(uuid)) => match FsKey::uuid(&uuid) {
            Some(key) => (key, None),
            None => (
                FsKey::dev(dev),
                Some(format!("its UUID {uuid:?} is not hex digits and dashes")),
            ),
        },
        Ok(None) => (FsKey::dev(dev), None),
        Err(e) => (FsKey::dev(dev), Some(format!("its UUID: {e}"))),
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

/// The filesystem a run reclaims, as the snapshot needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reclaimed {
    /// `st_dev` of `PATH`: what the key ([`fs_key`]) is made from.
    pub dev: Dev,
    /// The device mountinfo names for `PATH`'s mount (the superblock's:
    /// one for every btrfs subvolume of a filesystem, where `st_dev`
    /// differs per subvolume): what the space guard compares with the
    /// state directory's ([`crate::host::StateLock::dir_dev`]).
    pub fs: Dev,
    /// `--min-free` in bytes: the write never takes the filesystem below it.
    pub min_free: u64,
    pub deadline: Option<MonoTime>,
}

/// What [`RunIndex::seed`] keeps for [`RunIndex::finish`].
#[derive(Debug, Clone)]
struct Pending {
    key: FsKey,
    run: Reclaimed,
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

    /// Seeds the index from the snapshot of the reclaimed filesystem,
    /// right before the synchronous tally of a run that tallies (and so
    /// will write it, [`Self::finish`]). `bound` holds the (profile, hash)
    /// of every source this run tallies: a snapshot finder of one of these
    /// profiles under another hash (the profile was edited) is dropped;
    /// finders of other profiles stay for the runs that use them. Anything
    /// wrong with the snapshot is one log line, and the run starts from
    /// nothing; a state directory that cannot be used at all is said once
    /// and nothing is written either. At or past the deadline, nothing.
    /// Under `--no-index`, one line.
    pub fn seed<H: Host>(
        &mut self,
        host: &H,
        run: Reclaimed,
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
        if run.deadline.is_some_and(|d| host.now() >= d) {
            return; // the tally is skipped too; nothing to seed or write
        }
        let (key, fallback) = fs_key(host, run.dev);
        if let Some(why) = fallback {
            log(&format!(
                "index: filesystem {}: {why}; its snapshot is keyed by device number",
                run.dev
            ));
        }
        // The state directory and the lock file are made now, before the
        // reclaim walk, if they are missing: what they take is then on the
        // filesystem before the walk samples it, so a state directory on
        // the reclaimed filesystem never costs the goal behind the space
        // guard's back ([`Self::finish`] creates nothing but the file).
        // A state directory diskgc may not use is said once, here.
        match host.state_lock(&key) {
            Ok(lock) => drop(lock),
            Err(e) => {
                return log(&format!(
                    "index: no snapshot ({e}); the synchronous tally starts from nothing and \
                     nothing is written"
                ));
            }
        }
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
                self.pending = Some(Pending { key, run });
                return;
            }
            Err(e) => {
                self.pending = Some(Pending { key, run });
                return log(&ignored(e.to_string()));
            }
        };
        self.pending = Some(Pending {
            key: key.clone(),
            run,
        });
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
        let decoded = format::decode(&bytes);
        drop(bytes); // up to twice the budget: not held through the run
        let snapshot = match decoded.and_then(|s| fits(host, &key, s)) {
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

    /// Writes the snapshot after the reclaim walk of a run that seeded it
    /// ([`Self::seed`]), before the run's final space sample, so that the
    /// sample, the exit code and the report's duration include the write
    /// (TASK-30 review). Not after a terminating signal, not at or past
    /// the run's deadline, not while another writer holds the single-writer
    /// lock, not when the encoded index is larger than any snapshot may be
    /// ([`MAX_SNAPSHOT_BYTES`]: every later run would refuse it), and not
    /// when the state directory is on the reclaimed filesystem and the
    /// space the file takes (its size rounded up to whole blocks, and one
    /// more block for the new directory entry; the state directory and the
    /// lock file were made by [`Self::seed`], before the walk) would take
    /// that filesystem
    /// below `--min-free`. Each skip or failure is one log line and changes
    /// nothing else (docs/architecture.md, Snapshot).
    pub fn finish<H: Host>(&mut self, host: &H, log: &mut dyn FnMut(&str)) {
        let Some(Pending { key, run }) = self.pending.take() else {
            return;
        };
        let name = key.snapshot_name();
        let skipped = |why: &str| format!("index: snapshot {name} not written: {why}");
        let past = |host: &H| run.deadline.is_some_and(|d| host.now() >= d);
        if host.interrupted().is_some() {
            return log(&skipped("the run was interrupted"));
        }
        if past(host) {
            return log(&skipped("the run's deadline has passed"));
        }
        let boot_id = match host.boot_id() {
            Ok(id) => id,
            Err(e) => return log(&skipped(&format!("the boot id: {e}"))),
        };
        let clocks = Clocks::now(host);
        let header = Header {
            diskgc_version: DISKGC_VERSION.to_owned(),
            key: key.clone(),
            uid: host.effective_uid(),
            boot_id,
            written: clocks.wall,
        };
        let bytes = format::encode(&restore::capture(&self.state, clocks, header));
        let size = u64::try_from(bytes.len()).expect("fits u64");
        if size > MAX_SNAPSHOT_BYTES {
            return log(&skipped(&format!(
                "{size} bytes, more than the {MAX_SNAPSHOT_BYTES} a snapshot may be"
            )));
        }
        let lock = match host.state_lock(&key) {
            Ok(lock) => lock,
            Err(e) => return log(&skipped(&e.to_string())),
        };
        let dir_fs = lock.dir_dev;
        let wait_until = host.now().checked_add(LOCK_WAIT);
        let until = match (run.deadline, wait_until) {
            (Some(d), Some(w)) => Some(d.min(w)),
            (d, w) => d.or(w),
        };
        let locked = match host.flock_wait(lock.fd, LockMode::Exclusive, until) {
            Ok(WaitLock::Acquired(locked)) => locked,
            Ok(WaitLock::DeadlinePassed(_)) => {
                return log(&skipped(&format!(
                    "{} is held by another writer (a watch, or an ensure writing)",
                    key.lock_name()
                )));
            }
            Ok(WaitLock::Interrupted(_)) => return log(&skipped("the run was interrupted")),
            Err(e) => return log(&skipped(&e.to_string())),
        };
        if dir_fs == run.fs {
            match host.statvfs(locked.fd()) {
                Ok(space) => {
                    let block = space.block_bytes.max(1);
                    let takes = size.div_ceil(block).saturating_add(1).saturating_mul(block);
                    if space.avail_bytes.saturating_sub(takes) < run.min_free {
                        return log(&skipped(&format!(
                            "the state directory is on the reclaimed filesystem, and the {} it \
                             takes would bring its {} available below --min-free {}",
                            format_bytes(takes),
                            format_bytes(space.avail_bytes),
                            format_bytes(run.min_free)
                        )));
                    }
                }
                Err(e) => return log(&skipped(&e.to_string())),
            }
        }
        if host.interrupted().is_some() {
            return log(&skipped("the run was interrupted"));
        }
        if past(host) {
            return log(&skipped("the run's deadline has passed"));
        }
        match host.state_write(&locked, &key, &bytes) {
            Ok(()) => {
                let stats = self.state.index.stats();
                log(&format!(
                    "index: snapshot {name} written: {} unit(s), {}",
                    stats.singles + stats.blocks,
                    format_bytes(size)
                ));
            }
            Err(e) => log(&skipped(&e.to_string())),
        }
    }
}

/// `snapshot` if this run may use it (docs/architecture.md, Snapshot:
/// Validity): written by this build, for this filesystem, by this user,
/// and for a filesystem without a UUID in this boot.
pub fn fits<H: Host>(host: &H, key: &FsKey, snapshot: Snapshot) -> Result<Snapshot, String> {
    let h = &snapshot.header;
    let version = DISKGC_VERSION;
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
