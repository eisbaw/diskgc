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
//! `watch` (TASK-32, [`crate::watch`]) seeds its live index with
//! [`RunIndex::seed`] once at start and writes it through
//! [`writer::Writer`] ([`encode`] and [`write_held`]) every 5 minutes under
//! the single-writer lock it holds for its lifetime. Not here: the
//! report's `index` block (TASK-244).

pub mod format;
pub mod restore;
pub mod writer;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use crate::host::{Dev, FileType, FsKey, Host, LockMode, Locked, MonoTime, WaitLock};
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
    /// `watch`'s live index for one reclaim cycle: the background tally
    /// keeps it and the watcher writes it, so the cycle neither seeds,
    /// tallies nor writes ([`RunIndex::live`]).
    Live,
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
    /// Which tally builds the index when the seed is missing, for the log:
    /// `synchronous` (`ensure`) or `background` (`watch`).
    tally: &'static str,
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
            tally: "synchronous",
        }
    }

    /// Starts from the snapshot when valid, and writes it after the run.
    pub fn snapshot() -> Self {
        Self {
            persist: Persist::Snapshot,
            ..Self::ephemeral()
        }
    }

    /// `watch`'s live index `state` for one reclaim cycle: the reclaim
    /// walk takes its queues from it and updates it, without a seed, a
    /// synchronous tally or a write (the background tally and the
    /// watcher's own writes do those: [`crate::watch`]).
    pub fn live(state: TallyState) -> Self {
        Self {
            state,
            persist: Persist::Live,
            pending: None,
            tally: "background",
        }
    }

    /// `watch`'s seed at start: [`Self::snapshot`], whose log lines name
    /// the background tally. `watch` takes [`Self::state`] after
    /// [`Self::seed`] and writes the snapshot itself; it never calls
    /// [`Self::finish`].
    pub fn watch_seed() -> Self {
        Self {
            tally: "background",
            ..Self::snapshot()
        }
    }

    /// Whether this is `watch`'s live index ([`Self::live`]).
    pub fn is_live(&self) -> bool {
        self.persist == Persist::Live
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
            Persist::No | Persist::Live => return,
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
        let tally = self.tally;
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
                    "index: no snapshot ({e}); the {tally} tally starts from nothing and \
                     nothing is written"
                ));
            }
        }
        let name = key.snapshot_name();
        let ignored = |why: String| {
            format!("index: snapshot {name} ignored: {why}; the {tally} tally starts from nothing")
        };
        let file = match host.state_read(&key, MAX_SNAPSHOT_BYTES) {
            Ok(Some(file)) => file,
            Ok(None) => {
                log(&format!(
                    "index: no snapshot {name} yet; the {tally} tally starts from nothing"
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
        let bytes = match encode(host, &self.state, &key) {
            Ok(bytes) => bytes,
            Err(why) => return log(&skipped(&why)),
        };
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
        let held = Held {
            key: &key,
            locked: &locked,
            dir_fs,
        };
        let guard = Guard {
            fs: run.fs,
            min_free: Some(run.min_free),
        };
        let last_check = |host: &H| {
            if host.interrupted().is_some() {
                Some("the run was interrupted")
            } else if past(host) {
                Some("the run's deadline has passed")
            } else {
                None
            }
        };
        let stats = self.state.index.stats();
        let units = stats.singles + stats.blocks;
        write_held(host, units, held, guard, &bytes, &last_check, log);
    }

    /// The run that seeded this index ends without a snapshot, for `why`:
    /// one log line, no state operation. Nothing when the run did not
    /// tally (nothing pending).
    pub fn decline(&mut self, why: &str, log: &mut dyn FnMut(&str)) {
        if let Some(Pending { key, .. }) = self.pending.take() {
            log(&format!(
                "index: snapshot {} not written: {why}",
                key.snapshot_name()
            ));
        }
    }
}

/// The single-writer lock of `key`, held exclusively, and the device of
/// the state directory it is in.
#[derive(Debug, Clone, Copy)]
pub struct Held<'a, F> {
    pub key: &'a FsKey,
    pub locked: &'a Locked<F>,
    pub dir_fs: Dev,
}

/// What a snapshot write must not take: when the state directory is on
/// `fs` (the filesystem a run reclaims, or `watch` watches), available
/// space must stay at or above `min_free` after the write (`None`: no
/// guard, only a write that fails is skipped; `watch --tally-only`
/// without `--min-free`).
#[derive(Debug, Clone, Copy)]
pub struct Guard {
    pub fs: Dev,
    pub min_free: Option<u64>,
}

/// The snapshot of `state` for `key`, encoded, or why it is not written
/// (the boot id cannot be read, or it is larger than any snapshot may be,
/// [`MAX_SNAPSHOT_BYTES`]: every later run would refuse it).
pub fn encode<H: Host>(host: &H, state: &TallyState, key: &FsKey) -> Result<Vec<u8>, String> {
    let boot_id = host.boot_id().map_err(|e| format!("the boot id: {e}"))?;
    let clocks = Clocks::now(host);
    let header = Header {
        diskgc_version: DISKGC_VERSION.to_owned(),
        key: key.clone(),
        uid: host.effective_uid(),
        boot_id,
        written: clocks.wall,
    };
    let bytes = format::encode(&restore::capture(state, clocks, header));
    let size = u64::try_from(bytes.len()).expect("fits u64");
    if size > MAX_SNAPSHOT_BYTES {
        return Err(format!(
            "{size} bytes, more than the {MAX_SNAPSHOT_BYTES} a snapshot may be"
        ));
    }
    Ok(bytes)
}

/// Writes `bytes` (an index of `units` units, [`encode`]d) as the snapshot under the held
/// single-writer lock, unless the space [`Guard`] says no or `last_check`
/// names a reason right before the write. Every skip or failure is one log
/// line; nothing else changes (docs/architecture.md, Snapshot). What the
/// file takes is its size rounded up to whole blocks, and one more block
/// for the new directory entry.
pub fn write_held<H: Host>(
    host: &H,
    units: usize,
    held: Held<'_, H::Fd>,
    guard: Guard,
    bytes: &[u8],
    last_check: &dyn Fn(&H) -> Option<&'static str>,
    log: &mut dyn FnMut(&str),
) {
    let name = held.key.snapshot_name();
    let skipped = |why: &str| format!("index: snapshot {name} not written: {why}");
    let size = u64::try_from(bytes.len()).expect("fits u64");
    if let (true, Some(min_free)) = (held.dir_fs == guard.fs, guard.min_free) {
        match host.statvfs(held.locked.fd()) {
            Ok(space) => {
                let block = space.block_bytes.max(1);
                let takes = size.div_ceil(block).saturating_add(1).saturating_mul(block);
                if space.avail_bytes.saturating_sub(takes) < min_free {
                    return log(&skipped(&format!(
                        "the state directory is on the reclaimed filesystem, and the {} it \
                         takes would bring its {} available below --min-free {}",
                        format_bytes(takes),
                        format_bytes(space.avail_bytes),
                        format_bytes(min_free)
                    )));
                }
            }
            Err(e) => return log(&skipped(&e.to_string())),
        }
    }
    if let Some(why) = last_check(host) {
        return log(&skipped(why));
    }
    match host.state_write(held.locked, held.key, bytes) {
        Ok(()) => log(&format!(
            "index: snapshot {name} written: {units} unit(s), {}",
            format_bytes(size)
        )),
        Err(e) => log(&skipped(&e.to_string())),
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
