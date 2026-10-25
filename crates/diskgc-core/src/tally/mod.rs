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
//!   listed and the live cursor, and the work items both modes share
//!   ([`TallyState::list_source`], [`TallyState::size_unit`]);
//! - [`sync`] and [`background`]: the two modes.
//!
//! Not here: running the background tally in `watch` (TASK-32), the
//! snapshot that seeds and saves [`TallyState`] (TASK-30), and the reclaim
//! walk's use of the synchronous tally (TASK-31).

pub mod background;
pub mod budget;
pub mod listing;
pub mod pace;
pub mod recount;
pub mod schedule;
pub mod size;
pub mod sync;

use std::collections::BTreeMap;

use crate::engine::{DEFAULT_MAX_ENTRIES, Reason};
use crate::host::{Host, HostError, MonoTime};
use crate::index::{Finder, Index, Inserted, MountKey, Refusal, Tally};

use budget::Budget;
use listing::{Listing, TallySource};
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
        }
    }

    /// When `finder`'s source was last listed completely.
    pub fn discovered_at(&self, finder: &Finder) -> Option<MonoTime> {
        self.discovered.get(finder).copied()
    }

    /// Records a complete listing of `finder`'s source at `at` (a seed
    /// from the snapshot, TASK-30).
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
        let listing = listing::list(host, source, deadline)?;
        let mut done = Listed::default();
        for item in &listing.listed {
            let inserted =
                self.index
                    .insert(host, &item.mount, &item.path, &source.finder, item.found)?;
            done.collapses += usize::from(inserted.collapse.is_some());
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
        self.listed.insert(source.finder.clone(), now);
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
                assert!(self.index.set_cursor_bytes(0), "zero always fits");
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
            Step::TooMany => {
                let single = self.index.single_mut(mount, path).expect("read above");
                single.refusal = Some(Refusal {
                    reason: Reason::TooManyEntries,
                    at: now,
                });
                Sizing::Refused(Reason::TooManyEntries)
            }
        };
        assert!(self.index.set_cursor_bytes(0), "zero always fits");
        Ok(outcome)
    }

    /// Keeps `cursor` live, counted against the budget (collapsing first if
    /// it must). `false`: it does not fit; dropped.
    fn keep_cursor<H: Host>(&mut self, host: &H, cursor: Cursor) -> Result<bool, HostError> {
        let bytes = cursor.counted_bytes();
        if !self.index.set_cursor_bytes(bytes) {
            assert!(self.index.set_cursor_bytes(0), "zero always fits");
            self.index.collapse_to_low_water(host)?;
            if !self.index.set_cursor_bytes(bytes) {
                return Ok(false);
            }
        }
        self.cursor = Some(cursor);
        Ok(true)
    }

    /// Drops the live cursor if its unit is no longer the single it sizes.
    fn drop_stale_cursor(&mut self) {
        let stale = self.cursor.as_ref().is_some_and(|c| {
            self.index.single(c.mount(), c.root()).map(|s| s.identity) != Some(c.identity())
        });
        if stale {
            self.cursor = None;
            assert!(self.index.set_cursor_bytes(0), "zero always fits");
        }
    }
}
