//! The reclaim walk on the index (docs/architecture.md, The reclaim walk;
//! docs/safety.md, Deletion: Discovery). The index orders and narrows
//! work; it decides nothing about safety:
//!
//! 1. **Before the host lock**, the synchronous tally ([`sync_tally`],
//!    [`crate::tally::sync`]), seeded from the snapshot when there is a
//!    valid one ([`RunIndex::seed`]), lists every indexed tier's sources
//!    not listed in the last 5 minutes and sizes the first tier's stale
//!    units, within `--tally-budget`; under `oldest` that sizing is
//!    deferred to the queue below, which sizes each unit as it reaches it
//!    ([`DeferredSizing`]), in the same order.
//! 2. **Per tier, the queue** ([`Reclaim::walk_fs_index`]): the tier's units
//!    in its order. Each is a candidate only as discovery would make it: a
//!    single is found again by the tier's own discovery restricted to its
//!    path ([`fs_kind::discover_beneath`], [`Beneath::Exact`]), and must
//!    have the stored identity (`stale` otherwise, never walked); a block
//!    is expanded by the same discovery restricted to its site, or from the
//!    tier's bound root where a lump's site lies above it
//!    ([`Beneath::Subtree`]), and its members join the queue. The identity
//!    that re-discovery saw is then the one every later check compares
//!    against; each candidate is tallied read-only and reclaimed by the
//!    same code as a discovered one ([`Reclaim::act_fs`]).
//! 3. **Then the tier's own discovery**, always: what it finds that the
//!    queue did not take ([`Handled`]) is judged as before the index, so
//!    candidates created after the tally, or missing from a stale, partial
//!    or corrupt index, are reclaimed before any later tier runs.
//!
//! Which tiers take the index ([`takes_index`]): `fs` tiers of one source
//! without `!profile:`. Cargo, filesystem unions and Docker discover only
//! (their units are not tallied either); for them the index changes
//! nothing.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Acted, Protect, ProtectAt, Reclaim, SourceKind, Tier, sample};
use crate::config::Order;
use crate::engine::{Reason, Skip};
use crate::fs_kind::{self, Beneath, Find, Found};
use crate::host::{Delete, DevIno, Host, MonoTime, MountEntry};
use crate::index::queue::{self, AgeFilter, QueueEntry, Queued, Source, TierQuery};
use crate::index::{Family, Finder, FinderSetId, MountKey, name_family};
use crate::inuse::Checker;
use crate::outcome::Stop;
use crate::places::{self, mount_for};
use crate::report::Report;
use crate::snapshot::{Reclaimed, RunIndex};
use crate::tally::budget::Budget;
use crate::tally::listing::{SourceFind, TallySource, index_identity};
use crate::tally::schedule::needs_sizing;
use crate::tally::sync::{self, SyncTier};
use crate::tally::{Sizing, size};

mod tier_queue;

use tier_queue::{Item, Settle, TierQueue};

/// The first found-ahead batch (docs/architecture.md, Found ahead): what
/// the first deletion waits for.
const FOUND_AHEAD_FIRST: usize = 64;

/// The largest found-ahead batch, of a liveness tier ordered `oldest`
/// (docs/architecture.md, Found ahead).
const FOUND_AHEAD_MAX: usize = 512;

/// The sizes of a tier's successive found-ahead batches
/// (docs/architecture.md, Found ahead): one at a time under `best-fit`;
/// under `oldest`, [`FOUND_AHEAD_FIRST`] each time, except that a tier
/// whose source has a liveness contract doubles them up to
/// [`FOUND_AHEAD_MAX`] (no scan started before its candidates were found
/// may serve them, [`crate::inuse::FreshAttempts::scan_not_before`], so
/// each of its batches costs a scan).
fn found_ahead_batches(order: Order, has_contract: bool) -> impl Iterator<Item = usize> {
    let (first, grows) = match order {
        Order::BestFit => (1, false),
        Order::Oldest => (FOUND_AHEAD_FIRST, has_contract),
    };
    std::iter::successors(Some(first), move |&batch| {
        Some(if grows {
            (batch * 2).min(FOUND_AHEAD_MAX)
        } else {
            batch
        })
    })
}

/// A tier's found-ahead batch: the candidates found again and not yet
/// judged, and the sizes of the batches to come ([`found_ahead_batches`]).
struct FoundAhead<I> {
    ready: VecDeque<(Found, Origin)>,
    sizes: I,
}

/// Whether the reclaim walk takes `tier`'s candidates from the index: an
/// `fs` tier of one source without `!profile:` (see the module doc).
pub(super) fn takes_index(tier: &Tier) -> bool {
    matches!(tier.source, SourceKind::Fs(_)) && tier.fs_union.is_empty() && tier.negated.is_empty()
}

/// Why `roots` may reach one object under two spellings that a restricted
/// re-match cannot reconcile (TASK-252 review): two of them lie on
/// different mounts of one filesystem (a bind mount, by device as
/// mountinfo shows it). Discovery walks every root, outermost first, and
/// records an object once, under the first root that reaches it, whose
/// path then decides the rules by path (a keep marker on the way); a unit
/// re-matched from the other root alone would be judged by its own way
/// down, and unlinking it removes the same directory entry. `None`: no two
/// roots share a filesystem across mounts. A root whose mount cannot be
/// told counts as such a pair (fail closed).
fn bind_view(mounts: &[MountEntry], roots: &[PathBuf]) -> Option<String> {
    let mut placed: Vec<(&Path, &MountEntry)> = Vec::new();
    for root in roots {
        let Ok(mount) = mount_for(mounts, root) else {
            return Some(format!(
                "the mount of root {} cannot be told from mountinfo",
                root.display()
            ));
        };
        if let Some((other, _)) = placed
            .iter()
            .find(|(_, m)| m.dev == mount.dev && m.id != mount.id)
        {
            return Some(format!(
                "roots {} and {} lie on two mounts of one filesystem (device {})",
                other.display(),
                root.display(),
                mount.dev
            ));
        }
        placed.push((root, mount));
    }
    None
}

/// The finder `tier`'s source carries for `root`.
fn finder(tier: &Tier, root: &Path) -> Finder {
    Finder {
        profile: tier.profile.as_str().into(),
        profile_hash: tier.profile_hash(),
        root: root.into(),
    }
}

/// The synchronous tally's view of the indexed tiers among `tiers` (policy
/// order): each source bound to each of its roots.
fn sync_tiers<'t>(tiers: impl IntoIterator<Item = &'t Tier>) -> Vec<SyncTier> {
    tiers
        .into_iter()
        .filter(|tier| takes_index(tier))
        .map(|tier| {
            let SourceKind::Fs(find) = &tier.source else {
                unreachable!("takes_index admits fs tiers only")
            };
            SyncTier {
                sources: tier
                    .roots
                    .iter()
                    .map(|root| TallySource {
                        finder: finder(tier, root),
                        find: SourceFind::Fs(find.clone()),
                    })
                    .collect(),
                order: tier.order,
            }
        })
        .collect()
}

/// The background tally's sources for `tiers` (policy order): every
/// indexed tier's source bound to each of its roots, each once (`watch`;
/// docs/architecture.md, The tally: Background).
pub fn tally_sources<'t>(tiers: impl IntoIterator<Item = &'t Tier>) -> Vec<TallySource> {
    let mut sources: Vec<TallySource> = Vec::new();
    for source in sync_tiers(tiers).into_iter().flat_map(|t| t.sources) {
        if !sources.iter().any(|s| s.finder == source.finder) {
            sources.push(source);
        }
    }
    sources
}

/// What the run's synchronous tally came to, for the reclaim walk.
#[derive(Debug, Default)]
pub(super) struct SyncTallied {
    /// Whether the reclaim walk may take queues from the index.
    pub(super) usable: bool,
    /// The candidates this invocation's own tally refused, by path: they
    /// are skipped for the run (docs/architecture.md, Refusals). Only
    /// these: a refusal hint already in the index (an earlier tally's, a
    /// snapshot's) only ranks its unit last, whatever its time says
    /// (Codex review of TASK-31: a hint timed in this run, by a clock
    /// step or a corrupt snapshot, suppressed an eligible candidate and
    /// let a pressure tier run before it). The walk's own sizing of the
    /// first tier ([`Self::deferred`]) adds what it refuses.
    pub(super) refused: BTreeMap<PathBuf, Reason>,
    /// The first tier's sizing under `oldest`, which the tally left to the
    /// walk ([`crate::tally::sync::Deferred`]).
    pub(super) deferred: Option<DeferredSizing>,
}

/// The first tier's stale singles under `oldest`, sized by the reclaim walk
/// as the tier's queue reaches them (TASK-251; docs/architecture.md, The
/// tally: Synchronous, pass 2): the synchronous tally's pass 2, deferred,
/// so the first deletion waits for the oldest units' sizing only.
///
/// **Order.** A unit's key in the queue must be a lower bound of the key
/// its sizing gives it ([`TierQueue::settle`]); then the queue gives out
/// the units in the order full sizing would. `max(mtime, ctime)` of the
/// unit's root is one, read now: the sizing walk counts the root's own
/// times, and a later read of them is not older unless timestamps go
/// back (a clock stepped back, a time set into the past whose ctime does
/// not cover it). So a stale unit whose key does not come from this
/// run's listing of its root (a key from an earlier tally, or from a
/// snapshot's listing this run did not repeat; Codex review of TASK-251)
/// has its root read again before the queue is ordered
/// ([`Reclaim::refresh_bounds`]). A root whose times are unknown is keyed
/// as just changed (as discovery keys it), which is no lower bound: its
/// place is approximate.
///
/// **Budget.** The re-reads and the sizing spend what pass 1 left of
/// `--tally-budget` (`None`: no limit), each stretch charged from before
/// it reads the mount table, so the tally's work stays within its budget;
/// the wait for the host lock and the steps between stretches are not
/// charged. Past it, the rest of the tier is taken by the keys it has and
/// the run's `tally_cut_short` is set. Every stretch is also bounded by
/// the tier's deadline (the run's, or the tier's own `timeout`).
///
/// **Who holds it.** The run's [`SyncTallied`] until a tier takes it
/// ([`Reclaim::walk_fs_index`]: an `oldest` tier whose sources are among
/// the deferred tier's, so a root preflight dropped does not lose it);
/// that tier gives it back when its queue ends, for a later tier with the
/// same sources. Not sized: blocks (as in pass 2) and units with a refusal
/// hint (they come after the tier's others anyway, Refusals).
#[derive(Debug)]
pub(super) struct DeferredSizing {
    /// The tier's sources, as the tally bound them (before preflight).
    finders: Vec<Finder>,
    /// Its stale singles when the tally ended.
    stale: usize,
    /// When the tally started: a never-tallied unit checked (listed) since
    /// carries its root's times from this run's listing.
    listed_since: MonoTime,
    /// What is left of `--tally-budget` for this work (`None`: no limit).
    left: Option<Duration>,
    /// The budget ended: the rest goes by the keys it has.
    spent: bool,
    /// A tier took it.
    used: bool,
    /// Roots read again for their keys; singles sized, refused; the time.
    refreshed: usize,
    sized: usize,
    refused: usize,
    took: Duration,
    /// The mount table, read once at the first stretch.
    mounts: Option<Vec<MountEntry>>,
}

impl SyncTallied {
    /// One log line if the first tier's deferred sizing was never taken by
    /// a tier (the run ended first, or no tier's sources matched): its
    /// units' order then came from their stored keys alone.
    pub(super) fn log_unused(&self, log: &mut dyn FnMut(&str)) {
        if let Some(d) = self.deferred.as_ref().filter(|d| !d.used) {
            log(&format!(
                "index: no tier took the first tier's deferred sizing ({} stale unit(s) of {} \
                 not sized in this run)",
                d.stale,
                d.finders
                    .iter()
                    .map(|f| format!("{} at {}", f.profile, f.root.display()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
}

/// A stretch of the deferred tally work ([`DeferredSizing`], Budget).
enum Window {
    Open {
        started: MonoTime,
        until: Option<MonoTime>,
    },
    Spent,
}

/// Runs the synchronous tally over `tiers` on the filesystem `run.mount`, for at
/// most `budget` (`None`: to completion) and never past `deadline`, before
/// the host lock (docs/architecture.md, The tally: Synchronous). Returns
/// whether the reclaim walk may take queues from the index, and what this
/// tally refused.
///
/// A failure of the host (`EIO`, `EMFILE`, `ENOMEM`: no skip, an error of
/// the run, docs/safety.md, Fail closed) ends the tally with one log line
/// and returns `false`: no tier then takes a queue, so each tier's own
/// discovery meets the failure first, and a tier whose discovery stops
/// reclaims nothing, as without the index. Otherwise a queued unit could
/// be deleted before discovery of the same root stops on another branch.
/// Coarse on purpose: any tier, not only the one whose roots failed (the
/// run's other tiers lose only speed).
#[allow(clippy::too_many_arguments)]
pub(super) fn sync_tally<'t, H: Host>(
    host: &H,
    index: &mut RunIndex,
    tiers: impl IntoIterator<Item = &'t Tier>,
    run: Reclaimed,
    budget: Option<Duration>,
    report: &mut Report,
    log: &mut dyn FnMut(&str),
) -> SyncTallied {
    let (space, deadline) = (places::space_of(&run.mount), run.deadline);
    let tiers = sync_tiers(tiers);
    // `watch`'s live index: the background tally keeps it, so a cycle
    // takes its queues from it as it is (docs/reference.md, `watch`: the
    // live index replaces the synchronous tally), unless the tally's last
    // listing of a selected source failed with a failure of the host: then
    // no tier takes a queue, as after a synchronous tally that failed.
    if index.is_live() {
        let failed = tiers
            .iter()
            .flat_map(|t| &t.sources)
            .find_map(|s| Some((&s.finder, index.state.source_failure(&s.finder)?)));
        if let Some((finder, why)) = failed {
            log(&format!(
                "index: the background tally's last listing of {} (profile {}) failed: {why}; \
                 no tier takes candidates from the index in this cycle (each tier's own \
                 discovery decides)",
                finder.root.display(),
                finder.profile
            ));
            return SyncTallied::default();
        }
        return SyncTallied {
            usable: true,
            ..SyncTallied::default()
        };
    }
    if tiers.is_empty() {
        return SyncTallied {
            usable: true,
            ..SyncTallied::default()
        };
    }
    // The run tallies: seed from the snapshot (and write it after the run).
    let bound: BTreeMap<&str, u64> = tiers
        .iter()
        .flat_map(|t| &t.sources)
        .map(|s| (&*s.finder.profile, s.finder.profile_hash))
        .collect();
    index.seed(host, run, &bound, log);
    let state = &mut index.state;
    let started = host.now();
    let budget_end = budget.and_then(|b| started.checked_add(b));
    let until = match (deadline, budget_end) {
        (Some(d), Some(b)) => Some(d.min(b)),
        (d, None) => d,
        (None, b) => b,
    };
    let done = sync::synchronous(host, state, &tiers, space, until);
    let took = host.now().saturating_since(started).as_millis();
    let stats = state.index.stats();
    let sync = match done {
        Ok(sync) if !matches!(sync.stopped, Some(Stop::Failed { .. })) => sync,
        failed => {
            let why = match failed {
                Ok(sync) => sync
                    .stopped
                    .map(|stop| stop.to_string())
                    .unwrap_or_default(),
                Err(error) => error.to_string(),
            };
            log(&format!(
                "index: the synchronous tally failed after {took} ms: {why}; no tier takes \
                 candidates from the index in this run (each tier's own discovery decides)"
            ));
            return SyncTallied::default();
        }
    };
    // Cut short by `--tally-budget`, not by the run's deadline, a signal or
    // a failure of the host.
    let by_budget = matches!(sync.stopped, Some(Stop::DeadlinePassed { .. }))
        && budget_end.is_some_and(|b| host.now() >= b && deadline.is_none_or(|d| b < d));
    if by_budget && (sync.untallied > 0 || !sync.pass1_done) {
        report.tally_cut_short = true;
    }
    let deferred = sync.deferred.map(|d| DeferredSizing {
        finders: tiers[d.tier]
            .sources
            .iter()
            .map(|s| s.finder.clone())
            .collect(),
        stale: d.stale,
        listed_since: started,
        left: budget_end.map(|end| end.saturating_since(host.now())),
        spent: false,
        used: false,
        refreshed: 0,
        sized: 0,
        refused: 0,
        took: Duration::ZERO,
        mounts: None,
    });
    log(&format!(
        "index: synchronous tally of {} tier(s) in {took} ms (discovery {} ms): {} source(s) \
         listed, {} recent, {} candidate(s) inserted, {}{}; the index holds {} unit(s), {} of \
         them blocks",
        tiers.len(),
        sync.pass1_took.as_millis(),
        sync.listed.len(),
        sync.skipped_recent.len(),
        sync.inserted,
        match &deferred {
            Some(d) => format!(
                "sizing of the first tier's {} stale unit(s) left to the reclaim walk (oldest)",
                d.stale
            ),
            None => format!("{} sized, {} left untallied", sync.sized, sync.untallied),
        },
        match &sync.stopped {
            Some(stop) => format!(", stopped: {stop}"),
            None => String::new(),
        },
        stats.singles + stats.blocks,
        stats.blocks,
    ));
    SyncTallied {
        usable: true,
        refused: sync.refused.into_iter().collect(),
        deferred,
    }
}

/// What the index's queue of a tier took, so that the tier's own discovery
/// afterwards does not judge it again.
#[derive(Debug, Default)]
pub(super) struct Handled {
    /// Candidates the queue judged, by the identity re-discovery saw (an
    /// object judged under one name is not judged again under another).
    ids: HashSet<DevIno>,
    /// Paths the queue gave a verdict: those it judged, and those this
    /// run's own tally refused (docs/architecture.md, Refusals). Whatever is
    /// there now waits for the next run, as it did before the index (one
    /// verdict per path and run). A `stale` unit's path is not here: the
    /// object there now was never judged, and discovery judges it.
    paths: HashSet<PathBuf>,
}

impl Handled {
    /// Whether discovery's `candidate` at `path` was handled by the queue.
    pub(super) fn covers(&self, path: &Path, candidate: &Result<Found, Skip>) -> bool {
        self.paths.contains(path)
            || candidate
                .as_ref()
                .is_ok_and(|found| self.ids.contains(&found.identity.id))
    }
}

/// How a tier's queue ended.
pub(super) enum Walked {
    /// The tier ends here: the goal was observed, or the run stops.
    TierDone,
    /// On to the tier's own discovery.
    Discover,
}

/// A block a member was expanded from, to update the index after an action.
#[derive(Debug, Clone)]
struct BlockOf {
    mount: MountKey,
    site: PathBuf,
    finders: FinderSetId,
    family: Family,
}

/// Where a candidate the queue took stands in the index.
enum Origin {
    Single(MountKey, PathBuf),
    Member(BlockOf),
}

/// A member of an expanded block in the tier's queue: found now, and the
/// block it came from.
type Member = Box<(Found, BlockOf)>;

/// Whether `path` is a member of a block of `family` sited at `site`
/// (docs/architecture.md, The index: Granularity).
fn member_of(site: &Path, family: &Family, path: &Path) -> bool {
    match family {
        Family::Any => path != site && path.starts_with(site),
        Family::Name(family) => {
            path.parent() == Some(site)
                && path
                    .file_name()
                    .is_some_and(|name| *name_family(name.as_bytes()) == **family)
        }
    }
}

impl<H: Delete> Reclaim<'_, '_, H> {
    /// The tier's queue from the index (see the module doc). Every
    /// candidate it takes is recorded in `handled`. The first tier's
    /// deferred sizing ([`DeferredSizing`]) is this tier's when its sources
    /// are that tier's and it is ordered `oldest`.
    pub(super) fn walk_fs_index(
        &mut self,
        find: &Find,
        judge: &fs_kind::Judge<'_>,
        checker: &mut Checker<'_>,
        handled: &mut Handled,
    ) -> Walked {
        let finders: Vec<Finder> = self
            .tier
            .roots
            .iter()
            .map(|root| finder(self.tier, root))
            .collect();
        // Taken out of the run's state for this tier's queue, given back
        // below (one owner at a time, DeferredSizing: Who holds it). The
        // walk's sources are the tally's less what preflight dropped (a root
        // on another filesystem): a subset, in any order.
        let mut deferred = self.tallied.deferred.take_if(|d| {
            self.tier.order == Order::Oldest
                && !d.spent
                && !finders.is_empty()
                && finders.iter().all(|f| d.finders.contains(f))
        });
        if let Some(d) = deferred.as_mut() {
            d.used = true;
        }
        let before = deferred
            .as_ref()
            .map(|d| (d.refreshed, d.sized, d.refused, d.took));
        let walked = self.walk_queue(find, judge, checker, handled, &finders, &mut deferred);
        if let (Some(d), Some((refreshed, sized, refused, took))) = (&deferred, before)
            && (d.refreshed + d.sized + d.refused > refreshed + sized + refused || d.spent)
        {
            (self.log)(&format!(
                "{}: {} unit(s) sized as the queue reached them, {} refused, {} root(s) read \
                 again for their keys, in {} ms ({} of the first tier's {} stale unit(s) so far){}",
                self.tier.name,
                d.sized - sized,
                d.refused - refused,
                d.refreshed - refreshed,
                d.took.saturating_sub(took).as_millis(),
                d.sized + d.refused,
                d.stale,
                if d.spent {
                    "; --tally-budget ended: the rest by stored keys"
                } else {
                    ""
                },
            ));
        }
        if deferred.is_some() {
            self.tallied.deferred = deferred;
        }
        walked
    }

    fn walk_queue(
        &mut self,
        find: &Find,
        judge: &fs_kind::Judge<'_>,
        checker: &mut Checker<'_>,
        handled: &mut Handled,
        finders: &[Finder],
        deferred: &mut Option<DeferredSizing>,
    ) -> Walked {
        let sources: Vec<Source<'_>> = finders
            .iter()
            .map(|f| Source {
                profile: &f.profile,
                profile_hash: f.profile_hash,
                root: &f.root,
            })
            .collect();
        let Some(state) = self.index.as_deref() else {
            return Walked::Discover;
        };
        if self.tier.roots.len() > 1 {
            // A bind view: one object, two spellings; only a full discovery
            // picks the one it judges (`bind_view`).
            let why = match self.host.mountinfo() {
                Ok(mounts) => bind_view(&mounts, &self.tier.roots),
                Err(e) => Some(format!("mountinfo: {e}")),
            };
            if let Some(why) = why {
                (self.log)(&format!(
                    "{}: no queue from the index: {why}; its discovery decides",
                    self.tier.name
                ));
                return Walked::Discover;
            }
        }
        // The idle filters are not applied to the queue: a unit they would
        // leave out is found by the tier's discovery and judged anyway.
        let mut derived = queue::derive(
            &state.index,
            &TierQuery {
                dev: self.measured.space,
                sources: &sources,
                age: AgeFilter::None,
            },
        );
        if derived.ready.is_empty() && derived.hinted.is_empty() {
            return Walked::Discover;
        }
        if let Some(sizing) = deferred.as_mut()
            && !self.refresh_bounds(&mut derived.ready, sizing)
        {
            return Walked::TierDone;
        }
        let blocks = derived
            .ready
            .iter()
            .filter(|e| matches!(e.unit, Queued::Block(_)))
            .count();
        (self.log)(&format!(
            "{}: {} unit(s) from the index ({blocks} block(s), {} with a refusal hint), then \
             discovery",
            self.tier.name,
            derived.ready.len() + derived.hinted.len(),
            derived.hinted.len(),
        ));
        let mut queue: TierQueue<Member> = TierQueue::new(self.tier.order, derived);
        let started = self.host.now();
        let sampled = sample(self.host, self.measured);
        self.record_overrun("statvfs", &self.measured.path, started, self.host.now());
        if self.past_deadline() {
            self.stop(&Stop::DeadlinePassed {
                path: self.measured.path.clone(),
            });
            return Walked::TierDone;
        }
        let mut avail = match sampled {
            Ok(space) => space.avail_bytes,
            Err(e) => {
                self.host_failure(e);
                return Walked::TierDone;
            }
        };
        let mut ahead = FoundAhead {
            ready: VecDeque::new(),
            sizes: found_ahead_batches(self.tier.order, find.liveness_contract.is_some()),
        };
        loop {
            if self.stop_if_interrupted() || self.goal_reached(avail) {
                return Walked::TierDone;
            }
            if ahead.ready.is_empty() {
                let need = self.measured.target.saturating_sub(avail);
                if !self.find_ahead(find, &mut queue, &mut ahead, handled, need, deferred) {
                    return Walked::TierDone;
                }
            }
            let Some((found, origin)) = ahead.ready.pop_front() else {
                return Walked::Discover;
            };
            if !handled.ids.insert(found.identity.id) {
                continue; // a single and a block member: the same object
            }
            // One verdict per path and run, as before the index: an object
            // that replaces a judged one (`changed`) waits for the next run.
            handled.paths.insert(found.path.clone());
            match self.judge_from_index(&found, &origin, judge, checker, avail) {
                Acted::Next { avail: after, .. } => avail = after,
                Acted::Stop => return Walked::TierDone,
            }
        }
    }

    /// Takes the next found-ahead batch from `queue` into `ahead.ready`,
    /// its size the next of `ahead.sizes`, each candidate found again (a
    /// single re-matched, a block expanded into the queue) before any of
    /// them is judged: then one in-use scan started after all of them can
    /// serve them all (a liveness contract takes no scan older than its
    /// candidate's finding, [`crate::inuse::FreshAttempts::scan_not_before`];
    /// TASK-203 shares a scan among consecutive candidates). `need` is the
    /// space still missing after the last action, which a `best-fit` pick
    /// needs. `false`: the run stops.
    fn find_ahead(
        &mut self,
        find: &Find,
        queue: &mut TierQueue<Member>,
        ahead: &mut FoundAhead<impl Iterator<Item = usize>>,
        handled: &mut Handled,
        need: u64,
        deferred: &mut Option<DeferredSizing>,
    ) -> bool {
        let size = ahead.sizes.next().expect("an endless iterator");
        let ready = &mut ahead.ready;
        while ready.len() < size {
            if self.stop_if_interrupted() {
                return false;
            }
            if let Some(sizing) = deferred.as_mut()
                && !queue.settle(|front| self.size_deferred(front, sizing))
            {
                return false;
            }
            let Some(item) = queue.pop(need) else {
                return true;
            };
            match item {
                Item::Member(member) => {
                    let (found, block) = *member;
                    ready.push_back((found, Origin::Member(block)));
                }
                Item::Unit(entry) => match &entry.unit {
                    Queued::Block(_) => {
                        if !self.expand(find, &entry, queue, handled) {
                            return false;
                        }
                    }
                    Queued::Single(_) => match self.rematch(find, &entry, handled) {
                        Rematched::Candidate(found) => ready.push_back((
                            *found,
                            Origin::Single(entry.mount.clone(), entry.path.clone()),
                        )),
                        Rematched::Next => {}
                        Rematched::Stop => return false,
                    },
                },
            }
        }
        true
    }

    /// Opens a stretch of the deferred tally work ([`DeferredSizing`],
    /// Budget): from now, until what is left of `--tally-budget` or the
    /// tier's deadline. `Spent` when nothing is left (the rest goes by the
    /// keys it has; `tally_cut_short`).
    fn open_window(&mut self, sizing: &mut DeferredSizing) -> Window {
        let started = self.host.now();
        if sizing.left.is_some_and(|left| left.is_zero()) {
            self.deferred_budget_spent(sizing);
            return Window::Spent;
        }
        let budget_end = sizing
            .left
            .map(|left| started.checked_add(left).expect("monotonic time overflow"));
        let until = match (budget_end, self.deadline) {
            (Some(b), Some(d)) => Some(b.min(d)),
            (b, None) => b,
            (None, d) => d,
        };
        Window::Open { started, until }
    }

    /// Charges a stretch that began at `started` (`path`: what it worked on,
    /// for an overrun of the deadline).
    fn close_window(&mut self, sizing: &mut DeferredSizing, started: MonoTime, path: &Path) {
        let ended = self.host.now();
        let took = ended.saturating_since(started);
        sizing.took += took;
        sizing.left = sizing.left.map(|left| left.saturating_sub(took));
        self.record_overrun("tally", path, started, ended);
    }

    /// `--tally-budget` ended the deferred work: the rest by the keys it has.
    fn deferred_budget_spent(&mut self, sizing: &mut DeferredSizing) {
        sizing.spent = true;
        self.report.tally_cut_short = true;
    }

    /// The mount table for the deferred work, read once, inside a stretch
    /// (its time is charged). `None`: a failure of the host, recorded; the
    /// run stops.
    fn deferred_mounts(
        &mut self,
        sizing: &mut DeferredSizing,
        path: &Path,
    ) -> Option<Vec<MountEntry>> {
        if let Some(mounts) = sizing.mounts.take() {
            return Some(mounts);
        }
        match self.host.mountinfo() {
            Ok(mounts) => Some(mounts),
            Err(e) => {
                self.host_failure(format!("mountinfo, to size {}: {e}", path.display()));
                None
            }
        }
    }

    /// Makes every stale single's key in `ready` a lower bound of its sized
    /// key before the queue is ordered ([`DeferredSizing`], Order): a unit
    /// whose key does not come from this run's listing of its root (an
    /// earlier tally's `newest_change`, or a snapshot's listing this run
    /// did not repeat) gets `max(mtime, ctime)` of its root, read now, in
    /// the queue's copy (the index keeps what it has). Then `ready` is
    /// sorted again. A unit gone or replaced keeps its key: its sizing
    /// finds it so and its re-match reports it. `false`: the run stops.
    fn refresh_bounds(&mut self, ready: &mut [QueueEntry], sizing: &mut DeferredSizing) -> bool {
        if sizing.spent {
            return true;
        }
        let now = self.host.now();
        let since = sizing.listed_since;
        let to_read: Vec<usize> = ready
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match &e.unit {
                Queued::Single(s) => (needs_sizing(s, now)
                    && (s.tally.is_some() || s.checked_at < since))
                    .then_some(i),
                Queued::Block(_) => None,
            })
            .collect();
        let Some(&first) = to_read.first() else {
            return true;
        };
        let first_path = ready[first].path.clone();
        if self.stop_if_interrupted() {
            return false;
        }
        let Window::Open { started, until } = self.open_window(sizing) else {
            return true;
        };
        let Some(mounts) = self.deferred_mounts(sizing, &first_path) else {
            self.close_window(sizing, started, &first_path);
            return false;
        };
        let now_wall = self.host.wall_clock();
        let mut budget = Budget::until(until);
        let mut going = true;
        for i in to_read {
            if self.stop_if_interrupted() {
                going = false;
                break;
            }
            if budget.spent(self.host) {
                if self.past_deadline() {
                    self.stop(&Stop::DeadlinePassed {
                        path: ready[i].path.clone(),
                    });
                    going = false;
                } else {
                    self.deferred_budget_spent(sizing);
                }
                break;
            }
            let entry = &mut ready[i];
            let Queued::Single(single) = &mut entry.unit else {
                unreachable!("only singles are read again")
            };
            match size::root_change(
                self.host,
                &mounts,
                &entry.path,
                single.identity,
                &mut budget,
            ) {
                Ok(Some(change)) => {
                    single.newest_change = change.unwrap_or(now_wall);
                    sizing.refreshed += 1;
                }
                Ok(None) => {}
                Err(e) => {
                    let path = entry.path.display().to_string();
                    self.host_failure(format!("reading {path} again for its key: {e}"));
                    going = false;
                    break;
                }
            }
        }
        sizing.mounts = Some(mounts);
        self.close_window(sizing, started, &first_path);
        ready.sort_by(QueueEntry::cmp_order);
        going
    }

    /// The deferred sizing of `front`, the next unit left in the queue
    /// ([`DeferredSizing`], [`TierQueue::settle`]): a stale single is sized
    /// now, read-only, in one go and keeping no cursor, within a stretch of
    /// the deferred budget that starts before the mount table is read. A
    /// refusal is this run's own tally's: the candidate is skipped once
    /// found again at its path ([`Self::rematch`]). A failure of the host
    /// is an error of the run, as for every check of the walk
    /// (docs/safety.md, Fail closed). A signal is not checked inside one
    /// unit's sizing walk, only between units.
    fn size_deferred(&mut self, front: &QueueEntry, sizing: &mut DeferredSizing) -> Settle {
        let Queued::Single(single) = &front.unit else {
            return Settle::Final; // blocks are not sized (pass 2's rule)
        };
        if sizing.spent || !needs_sizing(single, self.host.now()) {
            return Settle::Final;
        }
        if self.stop_if_interrupted() {
            return Settle::Stop;
        }
        if self.past_deadline() {
            self.stop(&Stop::DeadlinePassed {
                path: front.path.clone(),
            });
            return Settle::Stop;
        }
        let Window::Open { started, until } = self.open_window(sizing) else {
            return Settle::Final;
        };
        let Some(mounts) = self.deferred_mounts(sizing, &front.path) else {
            self.close_window(sizing, started, &front.path);
            return Settle::Stop;
        };
        let Some(state) = self.index.as_deref_mut() else {
            unreachable!("a queue from the index has an index")
        };
        let sized = state.size_unit_whole(
            self.host,
            &mounts,
            &front.mount,
            &front.path,
            &mut Budget::until(until),
        );
        sizing.mounts = Some(mounts);
        self.close_window(sizing, started, &front.path);
        match sized {
            Err(e) => {
                self.host_failure(format!("sizing {}: {e}", front.path.display()));
                Settle::Stop
            }
            Ok(Sizing::Done) => {
                sizing.sized += 1;
                let index = self.index.as_deref().map(|state| &state.index);
                let single = index.and_then(|index| index.single(&front.mount, &front.path));
                Settle::Ready(QueueEntry {
                    mount: front.mount.clone(),
                    path: front.path.clone(),
                    unit: Queued::Single(single.expect("a sized single").clone()),
                })
            }
            Ok(Sizing::Refused(reason)) => {
                sizing.refused += 1;
                self.tallied.refused.insert(front.path.clone(), reason);
                Settle::Ready(front.clone())
            }
            // Gone or replaced (evicted by the sizing), or no longer a
            // single: kept in the queue so that its re-match finds what is
            // there now and reports the unit `stale` (where pass 2 evicted
            // it before the walk and the report said nothing).
            Ok(Sizing::Gone | Sizing::Missing) => Settle::Ready(front.clone()),
            Ok(Sizing::Paused | Sizing::NoRoom) => {
                if self.past_deadline() {
                    self.stop(&Stop::DeadlinePassed {
                        path: front.path.clone(),
                    });
                    return Settle::Stop;
                }
                // `--tally-budget` ended (`NoRoom` cannot happen: no cursor
                // is kept): the rest of the tier by the keys it has.
                self.deferred_budget_spent(sizing);
                Settle::Final
            }
        }
    }

    /// A single of the queue, found again by the tier's discovery
    /// restricted to its path and compared with its stored identity. A
    /// refusal of this run's own tally applies to the candidate found at
    /// its path, never to a unit discovery would not reach (TASK-252: the
    /// tally sizes units it did not find, and would report a refusal of
    /// one below a directory discovery cannot list).
    fn rematch(&mut self, find: &Find, entry: &QueueEntry, handled: &mut Handled) -> Rematched {
        let Queued::Single(single) = &entry.unit else {
            unreachable!("rematch of a block")
        };
        let started = self.host.now();
        let (found, stopped) = fs_kind::discover_beneath(
            self.host,
            &self.tier.roots,
            find,
            &entry.path,
            Beneath::Exact,
            self.deadline,
        );
        self.record_overrun("discover", &entry.path, started, self.host.now());
        if let Some(stop) = stopped {
            // As for discovery: an incomplete one acts on nothing.
            self.stop(&stop);
            return Rematched::Stop;
        }
        if let Some(refused) = found.refused.first() {
            // A root or a directory on the way refused: the tier's own
            // discovery reports it and decides.
            (self.log)(&format!(
                "{}: {} left to discovery: refused on the way: {refused}",
                self.tier.name,
                entry.path.display()
            ));
            return Rematched::Next;
        }
        if !found.aliased.is_empty() {
            // A multiply linked file: discovery reports it under the name
            // it meets first, which may not be this one (TASK-252). The
            // tier's own discovery judges it.
            (self.log)(&format!(
                "{}: {} left to discovery: multiply linked",
                self.tier.name,
                entry.path.display()
            ));
            return Rematched::Next;
        }
        let candidate = found
            .candidates
            .into_iter()
            .find(|(path, _)| *path == entry.path);
        let detail = match candidate {
            // Discovery's own refusal (a mount, an identity the kernel
            // does not report): the tier's discovery reports it.
            Some((_, Err(_))) => return Rematched::Next,
            Some((_, Ok(found))) => {
                if let Some(&reason) = self.tallied.refused.get(&entry.path) {
                    // This invocation's own tally refused the candidate at
                    // this path: skipped for the run, never walked or locked
                    // again, whatever object is there now (one verdict per
                    // path and run; docs/architecture.md, Refusals). A hint
                    // already in the index is not this: it only ranked the
                    // unit last, and the unit is judged like any other.
                    handled.paths.insert(entry.path.clone());
                    self.skip(
                        &entry.path,
                        &Skip::new(reason, entry.path.clone(), "refused by this run's tally"),
                    );
                    return Rematched::Next;
                }
                if index_identity(found.identity, found.seen) == Some(single.identity) {
                    return Rematched::Candidate(Box::new(found));
                }
                "replaced: another object under its name"
            }
            None => "gone, or no longer found by its source under the tier's root",
        };
        self.skip(
            &entry.path,
            &Skip::new(Reason::Stale, entry.path.clone(), detail),
        );
        if let Some(state) = self.index.as_deref_mut() {
            state.evict_single(&entry.mount, &entry.path);
        }
        Rematched::Next
    }

    /// A block of the queue, expanded: the tier's discovery restricted to
    /// its site (from the tier's bound root where that lies below the
    /// site); the members it finds in the block's family join the queue.
    /// `false`: the run stops.
    fn expand(
        &mut self,
        find: &Find,
        entry: &QueueEntry,
        queue: &mut TierQueue<Member>,
        handled: &Handled,
    ) -> bool {
        let Queued::Block(block) = &entry.unit else {
            unreachable!("expand of a single")
        };
        let started = self.host.now();
        let (found, stopped) = fs_kind::discover_beneath(
            self.host,
            &self.tier.roots,
            find,
            &entry.path,
            Beneath::Subtree,
            self.deadline,
        );
        self.record_overrun("discover", &entry.path, started, self.host.now());
        if let Some(stop) = stopped {
            self.stop(&stop);
            return false;
        }
        if !found.refused.is_empty() {
            // An expansion that did not complete acts on nothing it listed.
            return true;
        }
        let of = BlockOf {
            mount: entry.mount.clone(),
            site: entry.path.clone(),
            finders: block.finders(),
            family: block.family.clone(),
        };
        let now = self.host.wall_clock();
        // Multiply linked members are left to the tier's discovery (as in
        // `rematch`): still there, so the block is not empty. Accepted
        // cost: a block of only such members, which discovery keeps
        // refusing, is expanded again on every run until a tally evicts it.
        let aliased = found
            .aliased
            .iter()
            .filter(|path| member_of(&entry.path, &block.family, path))
            .count();
        let mut members = 0usize;
        for (path, candidate) in found.candidates {
            let Ok(found) = candidate else {
                continue; // discovery's own refusal: the tier's discovery reports it
            };
            if !member_of(&entry.path, &block.family, &path)
                || handled.ids.contains(&found.identity.id)
            {
                continue;
            }
            members += 1;
            // Unknown times sort as just changed: last, never first.
            let key = found.seen.changed.unwrap_or(now);
            queue.push_member(found.identity.id, key, Box::new((found, of.clone())));
        }
        (self.log)(&format!(
            "{}: block at {} ({} recorded member(s)) expanded: {members} member(s) found{}",
            self.tier.name,
            entry.path.display(),
            block.members,
            if aliased > 0 {
                format!(", {aliased} multiply linked left to discovery")
            } else {
                String::new()
            }
        ));
        if members == 0
            && aliased == 0
            && let Some(state) = self.index.as_deref_mut()
        {
            state
                .index
                .evict_block(&of.mount, &of.site, of.finders, &of.family);
        }
        true
    }

    /// A candidate the queue took: the protected check and the read-only
    /// tally (a refusal skips it, never locked), then the reclaim.
    fn judge_from_index(
        &mut self,
        found: &Found,
        origin: &Origin,
        judge: &fs_kind::Judge<'_>,
        checker: &mut Checker<'_>,
        avail: u64,
    ) -> Acted {
        let next = Acted::Next {
            avail,
            reclaimed: false,
        };
        match self.protected(&found.path, ProtectAt::FirstScreen) {
            Protect::Clear => {}
            Protect::Skipped => return next,
            Protect::Stopped => return Acted::Stop,
        }
        let started = self.host.now();
        let tallied = fs_kind::tally(self.host, judge, found, self.deadline);
        self.record_overrun("tally", &found.path, started, self.host.now());
        let tally = match tallied {
            Ok(Ok(tally)) => tally,
            Ok(Err(skip)) => {
                self.skip(&found.path, &skip);
                return next;
            }
            Err(stop) => {
                self.stop(&stop);
                return Acted::Stop;
            }
        };
        let acted = self.act_fs(found, Some(&tally), avail, judge, checker);
        if let Acted::Next {
            reclaimed: true, ..
        } = acted
        {
            self.reclaimed_from_index(origin, &tally);
        }
        acted
    }

    /// The index after a deletion from it completed (docs/architecture.md,
    /// Index update after an action): a single is evicted; an expanded
    /// block loses the member and its tallied size.
    fn reclaimed_from_index(&mut self, origin: &Origin, tally: &fs_kind::Tally) {
        let Some(state) = self.index.as_deref_mut() else {
            return;
        };
        match origin {
            Origin::Single(mount, path) => {
                state.evict_single(mount, path);
            }
            Origin::Member(b) => {
                let files = u64::try_from(tally.entries).unwrap_or(u64::MAX);
                state.index.block_member_removed(
                    &b.mount,
                    &b.site,
                    b.finders,
                    &b.family,
                    tally.bytes,
                    files,
                );
            }
        }
    }
}

/// What [`Reclaim::rematch`] came to.
enum Rematched {
    Candidate(Box<Found>),
    /// Skipped, or left to the tier's discovery: the next item.
    Next,
    Stop,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first(order: Order, has_contract: bool, n: usize) -> Vec<usize> {
        found_ahead_batches(order, has_contract).take(n).collect()
    }

    /// A liveness tier ordered `oldest` doubles its found-ahead batches from
    /// the first up to the cap and stays there; a plain one keeps the first
    /// size; `best-fit` takes one at a time either way.
    #[test]
    fn found_ahead_batches_grow_only_for_a_liveness_tier_and_stop_at_the_cap() {
        assert_eq!(
            first(Order::Oldest, true, 7),
            [64, 128, 256, 512, 512, 512, 512]
        );
        assert_eq!(first(Order::Oldest, false, 4), [64; 4]);
        assert_eq!(first(Order::BestFit, true, 4), [1; 4]);
        assert_eq!(first(Order::BestFit, false, 4), [1; 4]);
    }
}
