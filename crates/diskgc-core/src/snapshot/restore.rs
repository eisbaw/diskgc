//! Between the in-memory index ([`TallyState`], monotonic times) and a
//! [`Snapshot`] (wall-clock times): [`capture`] for a write, [`restore`]
//! for a seed (docs/architecture.md, The tally: Snapshot).
//!
//! **Times.** A monotonic time means nothing in another process's boot,
//! so none is written (TASK-31 review (a)): each is written as the wall
//! clock it corresponds to at the write (`wall now - (mono now - t)`), and
//! read back as `mono now - age`, where `age` is the wall clock's distance
//! from it at the load, at least one nanosecond, so a loaded time is
//! always before the loading run started. A time that cannot be read back
//! that way is unknown: one in the future (the wall clock was set back),
//! or older than the monotonic clock's origin (before this boot). Unknown
//! means as if never: no `discovered_at` (the source is listed again), no
//! tally (sized again), no refusal hint; a check time becomes the clock's
//! origin.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use super::format::{BlockRec, FinderRec, Header, SingleRec, Snapshot, TallyRec};
use crate::host::{Host, HostError, MonoTime, Timestamp};
use crate::index::{
    BlockParts, Discovered, Finder, Index, Inserted, MountKey, Refusal, Tally, UnitRef,
};
use crate::tally::TallyState;

/// The two clocks at one instant: a write's or a load's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clocks {
    pub mono: MonoTime,
    pub wall: Timestamp,
}

impl Clocks {
    pub fn now<H: Host>(host: &H) -> Self {
        Self {
            mono: host.now(),
            wall: host.wall_clock(),
        }
    }

    /// The wall-clock time of the monotonic `t` (at or before now).
    fn wall_of(self, t: MonoTime) -> Timestamp {
        minus(self.wall, self.mono.saturating_since(t))
    }

    /// The monotonic time of the wall-clock `t`, strictly before now;
    /// `None` when unknown (see the module doc).
    fn mono_of(self, t: Timestamp) -> Option<MonoTime> {
        let age = since(self.wall, t)?.max(Duration::from_nanos(1));
        self.mono.0.checked_sub(age).map(MonoTime)
    }
}

/// `t - d`, saturating at the representable limit.
fn minus(t: Timestamp, d: Duration) -> Timestamp {
    let total = i128::from(t.secs) * 1_000_000_000 + i128::from(t.nanos)
        - i128::try_from(d.as_nanos()).unwrap_or(i128::MAX);
    let secs = total.div_euclid(1_000_000_000);
    let nanos = u32::try_from(total.rem_euclid(1_000_000_000)).expect("below one second");
    match i64::try_from(secs) {
        Ok(secs) => Timestamp { secs, nanos },
        Err(_) => Timestamp {
            secs: i64::MIN,
            nanos: 0,
        },
    }
}

/// `later - earlier`; `None` if `earlier` is later.
fn since(later: Timestamp, earlier: Timestamp) -> Option<Duration> {
    let nanos = |t: Timestamp| i128::from(t.secs) * 1_000_000_000 + i128::from(t.nanos);
    let d = nanos(later) - nanos(earlier);
    let d = u128::try_from(d).ok()?;
    Some(Duration::new(
        u64::try_from(d / 1_000_000_000).ok()?,
        u32::try_from(d % 1_000_000_000).expect("below one second"),
    ))
}

/// The snapshot of `state` at `clocks`, under `header`.
pub fn capture(state: &TallyState, clocks: Clocks, header: Header) -> Snapshot {
    let index = &state.index;
    // Every finder a unit uses, and every source with a discovery time.
    let mut finders: BTreeMap<Finder, Option<Timestamp>> = BTreeMap::new();
    for (finder, at) in state.discovered() {
        finders.insert(finder.clone(), Some(clocks.wall_of(at)));
    }
    let mut mounts: BTreeMap<MountKey, ()> = BTreeMap::new();
    index.for_each_unit(|mount, _, unit| {
        mounts.insert(mount.clone(), ());
        let set = match unit {
            UnitRef::Single(s) => s.finders(),
            UnitRef::Block(b) => b.finders(),
        };
        for f in index.finder_set(set) {
            finders.entry(f.clone()).or_insert(None);
        }
    });
    let finder_at: BTreeMap<&Finder, usize> =
        finders.keys().enumerate().map(|(i, f)| (f, i)).collect();
    let mount_at: BTreeMap<&MountKey, usize> =
        mounts.keys().enumerate().map(|(i, m)| (m, i)).collect();
    let mut singles = Vec::new();
    let mut blocks = Vec::new();
    index.for_each_unit(|mount, path, unit| {
        let ids = |set| {
            let mut ids: Vec<usize> = index.finder_set(set).iter().map(|f| finder_at[f]).collect();
            ids.sort_unstable();
            ids
        };
        match unit {
            UnitRef::Single(s) => singles.push(SingleRec {
                mount: mount_at[mount],
                path: path.to_path_buf(),
                identity: s.identity,
                finders: ids(s.finders()),
                newest_change: s.newest_change,
                tally: s.tally.map(|t| TallyRec {
                    est_bytes: t.est_bytes,
                    files: t.files,
                    tallied_at: clocks.wall_of(t.tallied_at),
                }),
                checked_at: clocks.wall_of(s.checked_at),
                refusal: s.refusal.map(|r| (r.reason, clocks.wall_of(r.at))),
            }),
            UnitRef::Block(b) => blocks.push(BlockRec {
                mount: mount_at[mount],
                site: path.to_path_buf(),
                site_identity: b.site,
                finders: ids(b.finders()),
                family: b.family.clone(),
                members: b.members,
                est_bytes: b.est_bytes,
                files: b.files,
                oldest_change: b.oldest_change,
                newest_change: b.newest_change,
                tallied_at: b.tallied_at.map(|t| clocks.wall_of(t)),
            }),
        }
    });
    Snapshot {
        header,
        finders: finders
            .into_iter()
            .map(|(finder, discovered_at)| FinderRec {
                finder,
                discovered_at,
            })
            .collect(),
        mounts: mounts.into_keys().collect(),
        singles,
        blocks,
    }
}

/// What [`restore`] put back.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Restored {
    pub singles: usize,
    pub blocks: usize,
    /// Finders whose profile this run binds under another definition
    /// (another profile hash): dropped, with their discovery times.
    pub finders_dropped: usize,
    /// Units left with no finder by that: not restored.
    pub units_dropped: usize,
    /// Units the index had no room for (a smaller budget than the
    /// writer's): left out, at the cost of warmth only.
    pub no_room: usize,
}

/// `snapshot` (structurally checked by its decode) as a fresh index of
/// `budget` bytes, at `clocks`. A finder `keep` refuses is dropped
/// (docs/architecture.md, The index: Finders: a profile edited since
/// drops its units, nothing else does); a unit left without one is not
/// restored. An error is a failed host call of a collapse the insertions
/// needed (a snapshot written with a larger budget).
pub fn restore<H: Host>(
    host: &H,
    snapshot: &Snapshot,
    clocks: Clocks,
    budget: usize,
    keep: impl Fn(&Finder) -> bool,
) -> Result<(TallyState, Restored), HostError> {
    let mut state = TallyState::new(Index::new(budget));
    let mut restored = Restored::default();
    let kept: Vec<Option<&FinderRec>> = snapshot
        .finders
        .iter()
        .map(|f| {
            let keep = keep(&f.finder);
            restored.finders_dropped += usize::from(!keep);
            keep.then_some(f)
        })
        .collect();
    for f in kept.iter().flatten() {
        if let Some(at) = f.discovered_at.and_then(|t| clocks.mono_of(t)) {
            state.set_discovered_at(f.finder.clone(), at);
        }
    }
    let finders_of = |ids: &[usize]| -> Vec<&Finder> {
        ids.iter()
            .filter_map(|&i| kept[i].map(|f| &f.finder))
            .collect()
    };
    for s in &snapshot.singles {
        let mount = &snapshot.mounts[s.mount];
        let finders = finders_of(&s.finders);
        if finders.is_empty() {
            restored.units_dropped += 1;
            continue;
        }
        if !restore_single(host, &mut state, mount, s, &finders, clocks)? {
            restored.no_room += 1;
            continue;
        }
        restored.singles += 1;
    }
    for b in &snapshot.blocks {
        let mount = &snapshot.mounts[b.mount];
        let finders: Vec<Finder> = finders_of(&b.finders).into_iter().cloned().collect();
        if finders.is_empty() {
            restored.units_dropped += 1;
            continue;
        }
        let parts = BlockParts {
            site: b.site_identity,
            family: b.family.clone(),
            members: b.members,
            est_bytes: b.est_bytes,
            files: b.files,
            oldest_change: b.oldest_change,
            newest_change: b.newest_change,
            tallied_at: b.tallied_at.and_then(|t| clocks.mono_of(t)),
        };
        if state.index.restore_block(mount, &b.site, finders, parts) {
            restored.blocks += 1;
        } else {
            restored.no_room += 1;
        }
    }
    Ok((state, restored))
}

/// Puts back one single, found by each of `finders`; `false` if it did
/// not go in as a single (no room for it).
fn restore_single<H: Host>(
    host: &H,
    state: &mut TallyState,
    mount: &MountKey,
    s: &SingleRec,
    finders: &[&Finder],
    clocks: Clocks,
) -> Result<bool, HostError> {
    let path: &Path = &s.path;
    let checked_at = clocks
        .mono_of(s.checked_at)
        .unwrap_or(MonoTime(Duration::ZERO));
    for finder in finders {
        let found = Discovered {
            identity: s.identity,
            newest_change: s.newest_change,
            checked_at,
        };
        let inserted = state.index.insert(host, mount, path, finder, found)?;
        if !matches!(inserted.outcome, Inserted::New | Inserted::Refreshed) {
            return Ok(false);
        }
    }
    let Some(single) = state.index.single_mut(mount, path) else {
        return Ok(false); // a collapse for a later finder absorbed it
    };
    single.newest_change = s.newest_change;
    single.tally = s.tally.and_then(|t| {
        Some(Tally {
            est_bytes: t.est_bytes,
            files: t.files,
            tallied_at: clocks.mono_of(t.tallied_at)?,
        })
    });
    single.refusal = s.refusal.and_then(|(reason, at)| {
        Some(Refusal {
            reason,
            at: clocks.mono_of(at)?,
        })
    });
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64, nanos: u32) -> Timestamp {
        Timestamp { secs, nanos }
    }

    #[test]
    fn a_time_written_and_read_back_is_its_age_before_the_load() {
        let write = Clocks {
            mono: MonoTime(Duration::from_secs(5000)),
            wall: ts(1_000_000, 0),
        };
        let wall = write.wall_of(MonoTime(Duration::from_millis(4_000_500)));
        assert_eq!(wall, ts(999_000, 500_000_000));
        // The next run, 10 s later on both clocks: the same instant.
        let load = Clocks {
            mono: MonoTime(Duration::from_secs(5010)),
            wall: ts(1_000_010, 0),
        };
        assert_eq!(
            load.mono_of(wall),
            Some(MonoTime(Duration::from_millis(4_000_500)))
        );
    }

    #[test]
    fn a_time_never_reads_back_as_now_or_later() {
        let clocks = Clocks {
            mono: MonoTime(Duration::from_secs(100)),
            wall: ts(1_000, 0),
        };
        // Written at this very instant: one nanosecond before it.
        assert_eq!(
            clocks.mono_of(ts(1_000, 0)),
            Some(MonoTime(Duration::from_secs(100) - Duration::from_nanos(1)))
        );
        // The wall clock went back: unknown, never "this run".
        assert_eq!(clocks.mono_of(ts(1_000, 1)), None);
        // Older than this boot's monotonic origin: unknown.
        assert_eq!(clocks.mono_of(ts(899, 999_999_999)), None);
        assert_eq!(clocks.mono_of(ts(900, 0)), Some(MonoTime(Duration::ZERO)));
    }

    #[test]
    fn wall_arithmetic_crosses_second_and_sign_boundaries() {
        assert_eq!(
            minus(ts(0, 100), Duration::from_nanos(200)),
            ts(-1, 999_999_900)
        );
        assert_eq!(
            since(ts(0, 100), ts(-1, 999_999_900)),
            Some(Duration::from_nanos(200))
        );
        assert_eq!(since(ts(-1, 0), ts(0, 0)), None);
        assert_eq!(
            minus(ts(i64::MIN, 0), Duration::from_secs(1)).secs,
            i64::MIN
        );
    }
}
