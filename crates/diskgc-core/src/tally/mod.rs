//! The tally (docs/architecture.md, The tally): it updates the index; the
//! reclaim walk enacts it. Read-only: it reaches the filesystem only
//! through [`Host`] (never [`crate::host::Delete`]), takes no lock but a
//! momentary shared test of the host lock, and decides nothing about
//! deletion (the index is a cache, never an authority).
//!
//! One engine, two modes that differ only in pacing and in the order of
//! work:
//!
//! - [`background`] (in `watch`): must not load the system. Mini-batches
//!   bounded by [`budget::Budget::batch`], the duty cycle and the pressure
//!   brake ([`pace`]), the most overdue job first ([`schedule`]), a pause
//!   while the host lock is held, idle priorities.
//! - [`sync`] (in `ensure`): may load the machine. Full speed within its
//!   deadline (`--tally-budget`), in two passes: discovery of every
//!   selected tier, then sizing of the first tier's stale units.
//!
//! Composed bottom-up:
//!
//! - [`budget`], [`pace`], [`schedule`]: the bounds and the pacing, pure;
//! - [`size`]: sizing one unit's tree through a resumable [`size::Cursor`];
//! - [`listing`]: one source's discovery, by its kind's own code;
//! - [`recount`]: what complete listings mean for the index;
//! - this module: [`TallyState`], the index with when each source was last
//!   listed and the live cursors, and the work items: a whole listing
//!   ([`TallyState::list_source`], the synchronous tally's), a resumable
//!   listing round ([`TallyState::list_round`], the background's) and
//!   sizing ([`TallyState::size_unit`]);
//! - [`sync`] and [`background`]: the two modes.
//!
//! Not here: running the background tally in `watch` (TASK-32); the
//! snapshot that seeds and saves [`TallyState`] is [`crate::snapshot`].

pub mod background;
pub mod budget;
pub mod listing;
pub mod pace;
pub mod recount;
pub mod schedule;
pub mod size;
pub mod sync;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::engine::{DEFAULT_MAX_ENTRIES, Reason};
use crate::host::{Host, HostError, MonoTime};
use crate::index::{Finder, Index, Inserted, MountKey, Refusal, Tally};

use budget::Budget;
use listing::{Listing, ListingCursor, TallySource};
use recount::{Complete, Recount};
use size::{Cursor, Step};

/// The index and what the tally keeps beside it.
#[derive(Debug)]
pub struct TallyState {
    pub index: Index,
    /// When each source was last listed completely (`discovered_at`,
    /// docs/architecture.md, Trie and queue): what pass 1 skips when recent,
    /// and what the snapshot keeps (TASK-30).
    discovered: BTreeMap<Finder, MonoTime>,
    /// When each source was last listed at all: the background schedule
    /// (a listing that is never complete must not run every batch).
    listed: BTreeMap<Finder, MonoTime>,
    /// The live sizing walk, if one is paused between batches (at most
    /// one; counted against the index's budget).
    cursor: Option<Cursor>,
    /// The live background listing round, if one is paused between batches
    /// (at most one; counted against the index's budget with `cursor`),
    /// with its block member counts and the collapse count at its start.
    round: Option<Round>,
    /// Collapses run so far by this state's insertions and cursors: a
    /// round whose blocks changed under it does not recount them.
    collapses: u64,
    /// Singles whose last background job (re-check or sizing) failed, and
    /// when: they wait [`schedule::RECHECK_PERIOD`] before their next job,
    /// so a failing unit is not retried every batch. Pruned to the index's
    /// singles ([`Self::prune_failed`]).
    failed: BTreeMap<(MountKey, PathBuf), MonoTime>,
}

/// What listing one source did to the index.
#[derive(Debug, Default)]
pub struct Listed {
    pub listing: Listing,
    pub new: usize,
    pub refreshed: usize,
    pub replaced: usize,
    pub covered: usize,
    pub lumped: usize,
    /// Insertions refused: the budget is below the floor (a test budget).
    pub no_room: usize,
    /// Collapses the insertions needed.
    pub collapses: usize,
}

/// A live background listing round.
#[derive(Debug)]
struct Round {
    cursor: ListingCursor,
    members: BTreeMap<recount::BlockKey, u64>,
    collapses_at_start: u64,
}

/// What a step of a background listing round came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounded {
    /// Paused; the round goes on next time.
    Paused,
    /// The round ended: complete (evictions and recounts applied) or not.
    Done { complete: bool, recount: Recount },
    /// Its cursor does not fit the index's budget even after a collapse:
    /// dropped; the source waits its period.
    NoRoom,
}

/// What sizing a unit came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sizing {
    /// Its tally is recorded.
    Done,
    /// The budget ended first; the cursor is kept to resume.
    Paused,
    /// Gone or replaced: evicted.
    Gone,
    /// Refused (too many entries), recorded as a hint.
    Refused(Reason),
    /// The unit is not in the index (any more) as a single.
    Missing,
    /// The cursor does not fit the index's budget even after a collapse:
    /// left untallied.
    NoRoom,
}

impl TallyState {
    pub fn new(index: Index) -> Self {
        Self {
            index,
            discovered: BTreeMap::new(),
            listed: BTreeMap::new(),
            cursor: None,
            round: None,
            collapses: 0,
            failed: BTreeMap::new(),
        }
    }

    /// When `finder`'s source was last listed completely.
    pub fn discovered_at(&self, finder: &Finder) -> Option<MonoTime> {
        self.discovered.get(finder).copied()
    }

    /// Every source listed completely, and when.
    pub fn discovered(&self) -> impl Iterator<Item = (&Finder, MonoTime)> {
        self.discovered.iter().map(|(f, at)| (f, *at))
    }

    /// Records a complete listing of `finder`'s source at `at` (a seed
    /// from the snapshot).
    pub fn set_discovered_at(&mut self, finder: Finder, at: MonoTime) {
        self.discovered.insert(finder, at);
    }

    /// When `finder`'s source was last listed at all (this process).
    pub fn listed_at(&self, finder: &Finder) -> Option<MonoTime> {
        self.listed.get(finder).copied()
    }

    /// The unit the live cursor sizes, if any.
    pub fn cursor_unit(&self) -> Option<(&MountKey, &std::path::Path)> {
        self.cursor.as_ref().map(|c| (c.mount(), c.root()))
    }

    /// Lists `source` and inserts what it found (see [`listing`]). What a
    /// complete listing means beyond that (evictions, recounts) is the
    /// caller's round ([`Self::recount`]): the background recounts after
    /// each listing, the synchronous tally after pass 1. `deadline` bounds
    /// the discovery (the synchronous tally's).
    pub fn list_source<H: Host>(
        &mut self,
        host: &H,
        source: &TallySource,
        deadline: Option<MonoTime>,
    ) -> Result<Listed, HostError> {
        // Stamped first: a listing that fails is not retried before its
        // period (the background schedule).
        self.listed.insert(source.finder.clone(), host.now());
        let listing = listing::list(host, source, deadline)?;
        let mut done = Listed::default();
        for item in &listing.listed {
            let inserted =
                self.index
                    .insert(host, &item.mount, &item.path, &source.finder, item.found)?;
            done.collapses += usize::from(inserted.collapse.is_some());
            self.collapses += u64::from(inserted.collapse.is_some());
            match inserted.outcome {
                Inserted::New => done.new += 1,
                Inserted::Refreshed => done.refreshed += 1,
                Inserted::Replaced => done.replaced += 1,
                Inserted::Covered => done.covered += 1,
                Inserted::Lumped => done.lumped += 1,
                Inserted::NoRoom => done.no_room += 1,
            }
        }
        let now = host.now();
        if listing.complete {
            self.discovered.insert(source.finder.clone(), now);
        }
        // The cursor's unit may have been evicted or replaced meanwhile.
        self.drop_stale_cursor();
        done.listing = listing;
        Ok(done)
    }

    /// Applies the complete listings of a round ([`recount`]).
    pub fn recount(&mut self, round: &[Complete<'_>]) -> Recount {
        let done = recount::apply(&mut self.index, round);
        self.drop_stale_cursor();
        done
    }

    /// Sizes the single at `path` (on `mount`) within `budget`, resuming
    /// the live cursor if it is this unit's, else starting one (a cursor of
    /// another unit is dropped: one live cursor). Records the tally, the
    /// refusal hint or the eviction in the index.
    pub fn size_unit<H: Host>(
        &mut self,
        host: &H,
        mounts: &[crate::host::MountEntry],
        mount: &MountKey,
        path: &std::path::Path,
        budget: &mut Budget,
    ) -> Result<Sizing, HostError> {
        let Some(identity) = self.index.single(mount, path).map(|s| s.identity) else {
            return Ok(Sizing::Missing);
        };
        let mut cursor = match self.cursor.take() {
            Some(c) if c.mount() == mount && c.root() == path && c.identity() == identity => c,
            _ => Cursor::new(mount.clone(), path.to_owned(), identity),
        };
        let step = cursor.step(host, mounts, budget, DEFAULT_MAX_ENTRIES);
        let step = match step {
            Ok(step) => step,
            Err(e) => {
                self.account_cursors();
                return Err(e);
            }
        };
        let now = host.now();
        let outcome = match step {
            Step::Paused => {
                if !self.keep_cursor(host, cursor)? {
                    return Ok(Sizing::NoRoom);
                }
                return Ok(Sizing::Paused);
            }
            Step::Done(sized) => {
                let single = self.index.single_mut(mount, path).expect("read above");
                single.tally = Some(Tally {
                    est_bytes: sized.est_bytes,
                    files: sized.files,
                    tallied_at: now,
                });
                if let Some(newest) = sized.newest_change {
                    single.newest_change = newest;
                }
                single.refusal = None;
                single.checked_at = now;
                Sizing::Done
            }
            Step::Gone => {
                self.index.evict_single(mount, path);
                Sizing::Gone
            }
            Step::Refused(reason) => {
                let single = self.index.single_mut(mount, path).expect("read above");
                single.refusal = Some(Refusal { reason, at: now });
                Sizing::Refused(reason)
            }
        };
        self.account_cursors();
        Ok(outcome)
    }

    /// Keeps `cursor` live, counted against the budget (collapsing first if
    /// it must). `false`: it does not fit; dropped.
    fn keep_cursor<H: Host>(&mut self, host: &H, cursor: Cursor) -> Result<bool, HostError> {
        self.cursor = Some(cursor);
        if self.fit_cursors(host)? {
            return Ok(true);
        }
        self.cursor = None;
        self.account_cursors();
        Ok(false)
    }

    /// The live cursors' bytes: the sizing walk's and the listing round's.
    fn cursor_bytes(&self) -> usize {
        self.cursor.as_ref().map_or(0, Cursor::counted_bytes)
            + self.round.as_ref().map_or(0, |r| r.cursor.counted_bytes())
    }

    /// Counts the live cursors against the budget, collapsing first if
    /// they do not fit. `false`: they still do not (nothing counted for
    /// them changed: the caller drops one and calls [`Self::account_cursors`]).
    fn fit_cursors<H: Host>(&mut self, host: &H) -> Result<bool, HostError> {
        let bytes = self.cursor_bytes();
        if self.index.set_cursor_bytes(bytes) {
            return Ok(true);
        }
        assert!(self.index.set_cursor_bytes(0), "zero always fits");
        self.index.collapse_to_low_water(host)?;
        self.collapses += 1;
        Ok(self.index.set_cursor_bytes(bytes))
    }

    /// Counts the live cursors (smaller than before: always fits).
    fn account_cursors(&mut self) {
        let bytes = self.cursor_bytes();
        assert!(
            self.index.set_cursor_bytes(bytes),
            "the live cursors shrank yet do not fit"
        );
    }

    /// The source of the live listing round, if any.
    pub fn round_finder(&self) -> Option<&Finder> {
        self.round.as_ref().map(|r| r.cursor.finder())
    }

    /// Drops the live listing round (its source is gone from the run).
    pub fn drop_round(&mut self) {
        self.round = None;
        self.account_cursors();
    }

    /// One step of the background listing round of `source` within
    /// `budget`, starting a round if none is live (a round of another
    /// source is the caller's to finish first). What it finds is inserted
    /// at once; when the round ends complete, singles of the source it did
    /// not find lose the finder and its blocks are recounted
    /// ([`recount::apply_round`]). The source is stamped listed when the
    /// round ends or fails, so a failing source waits its period.
    pub fn list_round<H: Host>(
        &mut self,
        host: &H,
        source: &TallySource,
        budget: &mut Budget,
    ) -> Result<Rounded, HostError> {
        let mut round = match self.round.take() {
            Some(r) if *r.cursor.finder() == source.finder => r,
            Some(other) => panic!(
                "a listing round of {} is live: finish it first",
                other.cursor.finder().root.display()
            ),
            None => Round {
                cursor: ListingCursor::new(source, host.now()),
                members: BTreeMap::new(),
                collapses_at_start: self.collapses,
            },
        };
        let step = match round.cursor.step(host, source, budget) {
            Ok(step) => step,
            Err(e) => {
                self.listed.insert(source.finder.clone(), host.now());
                self.account_cursors();
                return Err(e);
            }
        };
        for item in &step.listed {
            let inserted =
                match self
                    .index
                    .insert(host, &item.mount, &item.path, &source.finder, item.found)
                {
                    Ok(inserted) => inserted,
                    Err(e) => {
                        // The round is lost: the source waits its period.
                        self.listed.insert(source.finder.clone(), host.now());
                        self.account_cursors();
                        return Err(e);
                    }
                };
            self.collapses += u64::from(inserted.collapse.is_some());
            if inserted.outcome == Inserted::Covered
                && let Some(key) = recount::covering_block(&self.index, &source.finder, item)
            {
                *round.members.entry(key).or_default() += 1;
            }
        }
        // Found but not indexable: a single there is still found.
        let now = host.now();
        for path in &step.unindexed {
            let mount = self.index_mount_of(path);
            if let Some(single) = mount.and_then(|m| self.index.single_mut(&m, path)) {
                single.checked_at = now;
            }
        }
        self.drop_stale_cursor();
        if !step.done {
            self.round = Some(round);
            // Never collapsed for: a round's listings must not make the
            // index coarser. One that does not fit is dropped (partial)
            // and its source waits its period.
            if !self.index.set_cursor_bytes(self.cursor_bytes()) {
                self.round = None;
                self.account_cursors();
                self.listed.insert(source.finder.clone(), now);
                return Ok(Rounded::NoRoom);
            }
            return Ok(Rounded::Paused);
        }
        self.listed.insert(source.finder.clone(), now);
        self.account_cursors();
        let mut recount = Recount::default();
        if step.complete {
            self.discovered
                .insert(source.finder.clone(), round.cursor.started());
            recount = recount::apply_round(
                &mut self.index,
                &source.finder,
                round.cursor.started(),
                &round.members,
                self.collapses == round.collapses_at_start,
            );
            self.drop_stale_cursor();
        }
        Ok(Rounded::Done {
            complete: step.complete,
            recount,
        })
    }

    /// The index's mount holding `path`, by the longest mount point.
    fn index_mount_of(&self, path: &Path) -> Option<MountKey> {
        let mut found: Option<MountKey> = None;
        self.index.for_each_mount(|key| {
            if path.starts_with(&key.mount_point)
                && found.as_ref().is_none_or(|f| {
                    key.mount_point.as_os_str().len() > f.mount_point.as_os_str().len()
                })
            {
                found = Some(key.clone());
            }
        });
        found
    }

    /// Evicts the single at `path` (a failed re-check), and the live cursor
    /// with it if it sized that unit.
    pub fn evict_single(&mut self, mount: &MountKey, path: &Path) -> bool {
        let evicted = self.index.evict_single(mount, path);
        self.drop_stale_cursor();
        evicted
    }

    /// Drops the live cursor (its unit is no longer due).
    pub fn drop_cursor(&mut self) {
        self.cursor = None;
        self.account_cursors();
    }

    /// Records that a job of the single at `path` failed at `at`.
    pub fn mark_failed(&mut self, mount: &MountKey, path: &Path, at: MonoTime) {
        self.failed.insert((mount.clone(), path.to_owned()), at);
    }

    /// When a job of the single at `path` last failed.
    pub fn failed_at(&self, mount: &MountKey, path: &Path) -> Option<MonoTime> {
        self.failed.get(&(mount.clone(), path.to_owned())).copied()
    }

    /// Forgets failures of units no longer in the index as singles, and
    /// ones older than `older_than`.
    pub fn prune_failed(&mut self, older_than: MonoTime) {
        let index = &self.index;
        self.failed
            .retain(|(mount, path), at| *at >= older_than && index.single(mount, path).is_some());
    }

    /// Drops the live cursor if its unit is no longer the single it sizes.
    fn drop_stale_cursor(&mut self) {
        let stale = self.cursor.as_ref().is_some_and(|c| {
            self.index.single(c.mount(), c.root()).map(|s| s.identity) != Some(c.identity())
        });
        if stale {
            self.cursor = None;
            self.account_cursors();
        }
    }
}
