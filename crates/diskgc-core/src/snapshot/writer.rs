//! `watch`'s snapshot writer (docs/architecture.md, Snapshot: Single
//! writer): the single-writer lock `index-<fs>.lock`, held for the life of
//! the process, and the writes under it every 5 minutes and at the stop.
//!
//! Without the lock (the state directory or the lock file could not be
//! made), nothing is loaded or written, and every write tries to take it
//! again without waiting; a lock then found held is one log line, never
//! an exit.
//!
//! A write is two steps so that the live index is held only for the
//! first: [`Writer::encode`] (with the index) and [`Writer::write`] (the
//! file, without it).

use std::path::Path;
use std::time::Duration;

use super::{Guard, Held, WriteChecks, encode, write_held};
use crate::host::{FsKey, Host, LockMode, Locked, MountEntry, TryLock, WaitLock};
use crate::tally::TallyState;

/// How long `watch` waits at start for the single-writer lock: an `ensure`
/// holds it only while writing.
pub const START_LOCK_WAIT: Duration = Duration::from_secs(10);

/// Why the single-writer lock was not taken at start.
#[derive(Debug)]
pub enum StartLock {
    /// Still held after [`START_LOCK_WAIT`]: another `watch` (exit 2).
    Held(String),
    /// A signal during the wait (exit 0, nothing written).
    Interrupted,
}

/// An encoded snapshot, ready to write.
#[derive(Debug)]
pub struct Encoded {
    bytes: Vec<u8>,
    units: usize,
}

/// The single-writer lock and the writes under it (see the module doc).
#[derive(Debug)]
pub struct Writer<F> {
    key: FsKey,
    /// The lock, held, and the state directory's descriptor and mount
    /// ([`crate::host::StateLock`]).
    held: Option<(Locked<F>, F, MountEntry)>,
    guard: Guard,
    /// Whether a write found the lock held by someone else (said once).
    said_held: bool,
    /// How often a retry is due, for the log.
    period: Duration,
}

impl<F> Writer<F> {
    pub fn has_lock(&self) -> bool {
        self.held.is_some()
    }

    pub fn key(&self) -> &FsKey {
        &self.key
    }
}

impl<F: std::fmt::Debug> Writer<F> {
    /// Takes the lock of `key`, waiting at most [`START_LOCK_WAIT`].
    /// `state_dir` names the state directory in the refusal; `period` is
    /// how often a writer without the lock tries again.
    pub fn start<H: Host<Fd = F>>(
        host: &H,
        key: FsKey,
        guard: Guard,
        state_dir: Option<&Path>,
        period: Duration,
        log: &mut dyn FnMut(&str),
    ) -> Result<Self, StartLock> {
        let mut writer = Self {
            key,
            held: None,
            guard,
            said_held: false,
            period,
        };
        let retry = format!("trying again every {} min", period.as_secs() / 60);
        let lock = match host.state_lock(&writer.key) {
            Ok(lock) => lock,
            Err(e) => {
                log(&format!(
                    "index: no state directory or lock file ({e}): no snapshot is loaded or \
                     written; {retry}"
                ));
                return Ok(writer);
            }
        };
        let until = host.now().checked_add(START_LOCK_WAIT);
        match host.flock_wait(lock.fd, LockMode::Exclusive, until) {
            Ok(WaitLock::Acquired(locked)) => {
                writer.held = Some((locked, lock.dir, lock.dir_mount));
            }
            Ok(WaitLock::DeadlinePassed(_)) => {
                return Err(StartLock::Held(format!(
                    "another watch holds {} for this filesystem and user (state directory {}): \
                     one watch per filesystem and state directory",
                    writer.key.lock_name(),
                    state_dir.map_or_else(|| "?".to_owned(), |d| d.display().to_string())
                )));
            }
            Ok(WaitLock::Interrupted(_)) => return Err(StartLock::Interrupted),
            Err(e) => log(&format!(
                "index: the single-writer lock {}: {e}; no snapshot is loaded or written; \
                 {retry}",
                writer.key.lock_name(),
            )),
        }
        Ok(writer)
    }

    /// The snapshot of `state`, encoded: the step that needs the live
    /// index. `None` (said in one line) when it cannot be written:
    /// without the lock, after trying to take it without waiting, or when
    /// the encoding is refused ([`encode`]).
    pub fn encode<H: Host<Fd = F>>(
        &mut self,
        host: &H,
        state: &TallyState,
        log: &mut dyn FnMut(&str),
    ) -> Option<Encoded> {
        if self.held.is_none() && !self.try_lock(host, log) {
            return None;
        }
        match encode(host, state, &self.key) {
            Ok(bytes) => {
                let stats = state.index.stats();
                Some(Encoded {
                    bytes,
                    units: stats.singles + stats.blocks,
                })
            }
            Err(why) => {
                log(&format!(
                    "index: snapshot {} not written: {why}",
                    self.key.snapshot_name()
                ));
                None
            }
        }
    }

    /// Writes what [`Self::encode`] made, under the held lock and the space
    /// guard ([`write_held`]); the live index is not needed. A second
    /// signal ([`Host::second_signal`]: docs/reference.md, Watch signals)
    /// arriving during the write cancels it in the port, before the link;
    /// one before the stop's write is [`crate::watch::run`]'s to skip.
    /// `path` is `PATH`'s held descriptor, which the guard samples too.
    pub fn write<H: Host<Fd = F>>(
        &self,
        host: &H,
        encoded: &Encoded,
        path: &F,
        log: &mut dyn FnMut(&str),
    ) {
        let Some((locked, dir, dir_mount)) = &self.held else {
            unreachable!("encoded only under the lock")
        };
        let held = Held {
            key: &self.key,
            locked,
            dir,
            dir_mount,
        };
        write_held(
            host,
            encoded.units,
            held,
            &self.guard,
            path,
            &encoded.bytes,
            WriteChecks {
                last_check: &|_| None,
                cancel: &|host| host.second_signal(),
            },
            log,
        );
    }

    fn try_lock<H: Host<Fd = F>>(&mut self, host: &H, log: &mut dyn FnMut(&str)) -> bool {
        let lock = match host.state_lock(&self.key) {
            Ok(lock) => lock,
            Err(_) => return false, // said at start
        };
        match host.flock_try(lock.fd, LockMode::Exclusive) {
            Ok(TryLock::Acquired(locked)) => {
                log(&format!(
                    "index: single-writer lock {} taken; snapshots are written from now on",
                    self.key.lock_name()
                ));
                self.held = Some((locked, lock.dir, lock.dir_mount));
                true
            }
            Ok(TryLock::Busy(_)) => {
                if !std::mem::replace(&mut self.said_held, true) {
                    log(&format!(
                        "index: {} is held by another writer; this watch writes no snapshot \
                         while it is (tried again every {} min)",
                        self.key.lock_name(),
                        self.period.as_secs() / 60
                    ));
                }
                false
            }
            Err(e) => {
                log(&format!(
                    "index: the single-writer lock {}: {e}",
                    self.key.lock_name()
                ));
                false
            }
        }
    }
}
