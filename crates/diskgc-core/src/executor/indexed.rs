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

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Acted, CandidateQueue, Protect, ProtectAt, Reclaim, SourceKind, Tier, sample};
use crate::config::Order;
use crate::engine::{Reason, Skip};
use crate::fs_kind::{self, Beneath, Find, Found};
use crate::host::{Delete, DevIno, Host, MountEntry, Timestamp};
use crate::index::queue::{self, AgeFilter, QueueEntry, Queued, Source, TierQuery};
use crate::index::{Family, Finder, FinderSetId, MountKey, name_family};
use crate::inuse::Checker;
use crate::outcome::Stop;
use crate::report::Report;
use crate::resolve::mount_for;
use crate::snapshot::{Reclaimed, RunIndex};
use crate::tally::Sizing;
use crate::tally::budget::Budget;
use crate::tally::listing::{SourceFind, TallySource, index_identity};
use crate::tally::schedule::needs_sizing;
use crate::tally::sync::{self, SyncTier};

/// Candidates the queue finds again ahead of judging them
/// ([`Reclaim::find_ahead`]). An in-use scan serves consecutive candidates
/// for up to a second (TASK-203: one scan per candidate made 6,400 pytest
/// sessions take one to two hours); 64 restricted re-discoveries, each a
/// few `statx` calls, are far cheaper than one scan, and bound what is
/// held ahead of the order (and how stale `best-fit`'s `need` gets).
const FOUND_AHEAD_MAX: usize = 64;

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
/// The order stays the one pass 2 gave (exact for singles): a stale
/// single's stored key, `max(mtime, ctime)` of its root or an older tally's
/// `newest_change`, is at most its sized `newest_change` (the walk counts
/// the root's own times, and times only grow), so a unit sized on the way
/// is taken once no unsized unit's stored key comes before its new key
/// ([`TierQueue::settle`]). Not sized: blocks (as in pass 2) and units
/// with a refusal hint (they come after the tier's others anyway,
/// Refusals). The sizing spends what pass 1 left of `--tally-budget`
/// (`None`: no limit), so the tally's work stays within its budget; past
/// it, the rest of the tier is taken by stored keys and the run's
/// `tally_cut_short` is set.
#[derive(Debug)]
pub(super) struct DeferredSizing {
    /// The tier's sources, as the walk's tier names them.
    finders: Vec<Finder>,
    /// Its stale singles when the tally ended.
    stale: usize,
    /// What is left of `--tally-budget` for sizing (`None`: no limit).
    left: Option<Duration>,
    /// The budget ended a sizing: the rest goes by stored keys.
    spent: bool,
    /// Singles sized, refused, and the time it took.
    sized: usize,
    refused: usize,
    took: Duration,
    /// The mount table, read once at the first sizing.
    mounts: Option<Vec<MountEntry>>,
}

/// Runs the synchronous tally over `tiers` on the filesystem `dev`, for at
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
    let (dev, deadline) = (run.dev, run.deadline);
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
    let done = sync::synchronous(host, state, &tiers, dev, until);
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
        left: budget_end.map(|end| end.saturating_since(host.now())),
        spent: false,
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

/// One item of a tier's queue: a unit of the index, or a member of an
/// expanded block found now.
enum Item {
    Unit(QueueEntry),
    Member(Box<Found>, BlockOf),
}

/// A unit of the `oldest` queue in the index's order
/// ([`QueueEntry::cmp_order`]).
#[derive(Debug)]
struct ByOrder(QueueEntry);

impl Ord for ByOrder {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp_order(&other.0)
    }
}

impl PartialOrd for ByOrder {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ByOrder {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ByOrder {}

/// What sizing the next unit of the `oldest` queue came to
/// ([`TierQueue::settle`]).
#[derive(Debug)]
enum Settle {
    /// Its stored key is final (sized recently, a block, or no more
    /// sizing in this run): it is taken as it is.
    Final,
    /// Decided: it waits under this key, its sized `newest_change`, or its
    /// stored key when the sizing refused it or found it gone (the
    /// re-match then gives its verdict: skipped as this run's tally
    /// refused it, or `stale`).
    Ready(QueueEntry),
    /// The run stops.
    Stop,
}

/// A tier's queue over the index (docs/architecture.md, Order within a
/// tier): the units in the tier's order, then those with a stored refusal
/// hint; an expanded block's members join it, under `oldest` by
/// `max(mtime, ctime)` of their own root, under `best-fit` all before the
/// next pick.
struct TierQueue {
    order: Order,
    /// `oldest`: the units in the index's `oldest` order, by their stored
    /// keys.
    oldest: VecDeque<QueueEntry>,
    /// `oldest`: the units sized on the way ([`Self::settle`]), by their
    /// new keys, each until no unit left in `oldest` can come before it.
    sized: BTreeSet<ByOrder>,
    /// `best-fit`: the units by their estimates.
    best_fit: CandidateQueue<QueueEntry>,
    members: BTreeMap<(Timestamp, u64), (Found, BlockOf)>,
    hinted: VecDeque<QueueEntry>,
    /// Ties of members keep the order expansion found them in.
    sequence: u64,
    /// Identities queued as members, so a member is queued once.
    queued: HashSet<DevIno>,
}

impl TierQueue {
    fn new(order: Order, queue: queue::TierQueue) -> Self {
        let (oldest, best_fit) = match order {
            Order::Oldest => (
                queue.ready.into(),
                CandidateQueue::new(order, Vec::new(), QueueEntry::estimate),
            ),
            Order::BestFit => (
                VecDeque::new(),
                CandidateQueue::new(order, queue.ready, QueueEntry::estimate),
            ),
        };
        Self {
            order,
            oldest,
            sized: BTreeSet::new(),
            best_fit,
            members: BTreeMap::new(),
            hinted: queue.hinted.into(),
            sequence: 0,
            queued: HashSet::new(),
        }
    }

    fn push_member(&mut self, found: Found, block: BlockOf, now: Timestamp) {
        if !self.queued.insert(found.identity.id) {
            return;
        }
        // Unknown times sort as just changed: last, never first.
        let key = found.seen.changed.unwrap_or(now);
        self.sequence += 1;
        self.members.insert((key, self.sequence), (found, block));
    }

    /// Whether the earliest unit sized on the way comes before the next
    /// one left in `oldest`.
    fn sized_first(&self) -> bool {
        match (self.sized.first(), self.oldest.front()) {
            (Some(sized), Some(unit)) => sized.0.cmp_order(unit).is_le(),
            (sized, _) => sized.is_some(),
        }
    }

    /// The next unit under `oldest`.
    fn oldest_head(&self) -> Option<&QueueEntry> {
        if self.sized_first() {
            self.sized.first().map(|s| &s.0)
        } else {
            self.oldest.front()
        }
    }

    /// Makes the next unit under `oldest` final, before [`Self::pop`]:
    /// while the next unit left in `oldest` comes before every unit sized
    /// on the way and before the next member, `size` decides on it by its
    /// stored key (a lower bound of the key sizing gives it,
    /// [`DeferredSizing`]); one sized moves to `sized` under its new key
    /// and the next one is looked at. So the units come out in the order
    /// their sized keys give, sizing only as many as that order needs.
    /// `false`: the run stops.
    fn settle(&mut self, mut size: impl FnMut(&QueueEntry) -> Settle) -> bool {
        loop {
            let Some(front) = self.oldest.front() else {
                return true;
            };
            if self.sized_first() {
                return true;
            }
            if self
                .members
                .first_key_value()
                .is_some_and(|((key, _), _)| *key < front.order_key())
            {
                return true; // a member comes first whatever the front's size
            }
            match size(front) {
                Settle::Final => return true,
                Settle::Ready(entry) => {
                    self.oldest.pop_front();
                    self.sized.insert(ByOrder(entry));
                }
                Settle::Stop => return false,
            }
        }
    }

    fn pop(&mut self, need: u64) -> Option<Item> {
        let member = |queue: &mut Self| {
            queue
                .members
                .pop_first()
                .map(|(_, (found, block))| Item::Member(Box::new(found), block))
        };
        match self.order {
            Order::Oldest => {
                let unit_first = match (self.oldest_head(), self.members.first_key_value()) {
                    (Some(unit), Some(((key, _), _))) => unit.order_key() <= *key,
                    (Some(_), None) => true,
                    (None, _) => false,
                };
                if unit_first {
                    let unit = if self.sized_first() {
                        self.sized.pop_first().map(|s| s.0)
                    } else {
                        self.oldest.pop_front()
                    };
                    return unit.map(Item::Unit);
                }
                if let Some(item) = member(self) {
                    return Some(item);
                }
            }
            Order::BestFit => {
                if let Some(item) = member(self) {
                    return Some(item);
                }
                if let Some(unit) = self.best_fit.pop(need) {
                    return Some(Item::Unit(unit));
                }
            }
        }
        self.hinted.pop_front().map(Item::Unit)
    }
}

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
        let mut deferred = self
            .tallied
            .deferred
            .take_if(|d| self.tier.order == Order::Oldest && d.finders == finders && !d.spent);
        let before = deferred.as_ref().map(|d| (d.sized, d.refused, d.took));
        let walked = self.walk_queue(find, judge, checker, handled, &finders, &mut deferred);
        if let (Some(d), Some((sized, refused, took))) = (&deferred, before)
            && (d.sized + d.refused > sized + refused || d.spent)
        {
            (self.log)(&format!(
                "{}: {} unit(s) sized as the queue reached them, {} refused, in {} ms ({} of \
                 the first tier's {} stale unit(s) so far){}",
                self.tier.name,
                d.sized - sized,
                d.refused - refused,
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
        let derived = queue::derive(
            &state.index,
            &TierQuery {
                dev: self.measured.dev,
                sources: &sources,
                age: AgeFilter::None,
            },
        );
        if derived.ready.is_empty() && derived.hinted.is_empty() {
            return Walked::Discover;
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
        let mut queue = TierQueue::new(self.tier.order, derived);
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
        let mut ready: VecDeque<(Found, Origin)> = VecDeque::new();
        loop {
            if self.stop_if_interrupted() || self.goal_reached(avail) {
                return Walked::TierDone;
            }
            if ready.is_empty() {
                let need = self.measured.target.saturating_sub(avail);
                if !self.find_ahead(find, &mut queue, &mut ready, handled, need, deferred) {
                    return Walked::TierDone;
                }
            }
            let Some((found, origin)) = ready.pop_front() else {
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

    /// Takes the next candidates from `queue` into `ready`, each found
    /// again (a single re-matched, a block expanded into the queue), up to
    /// [`FOUND_AHEAD_MAX`] of them, before any of them is judged: then one
    /// in-use scan started after all of them can serve them all (a
    /// liveness contract takes no scan older than its candidate's finding,
    /// [`crate::inuse::FreshAttempts::scan_not_before`]; TASK-203 shares a
    /// scan among consecutive candidates). Under `best-fit` one at a time:
    /// each pick needs `need`, the space still missing after the last
    /// action. `false`: the run stops.
    fn find_ahead(
        &mut self,
        find: &Find,
        queue: &mut TierQueue,
        ready: &mut VecDeque<(Found, Origin)>,
        handled: &mut Handled,
        need: u64,
        deferred: &mut Option<DeferredSizing>,
    ) -> bool {
        // `best-fit` picks each candidate with the space missing after the
        // last action (docs/architecture.md, Order within a tier): one at
        // a time.
        let ahead = match self.tier.order {
            Order::Oldest => FOUND_AHEAD_MAX,
            Order::BestFit => 1,
        };
        while ready.len() < ahead {
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
                Item::Member(found, block) => ready.push_back((*found, Origin::Member(block))),
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

    /// The deferred sizing of `front`, the next unit left in the queue
    /// ([`DeferredSizing`], [`TierQueue::settle`]): a stale single is sized
    /// now, read-only, in one go and keeping no cursor, until the deadline
    /// or what is left of `--tally-budget`. A refusal is this run's own
    /// tally's: the candidate is skipped once found again at its path
    /// ([`Self::rematch`]). A failure of the host is an error of the run,
    /// as for every check of the walk (docs/safety.md, Fail closed).
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
        let mounts = match sizing.mounts.take() {
            Some(mounts) => mounts,
            None => match self.host.mountinfo() {
                Ok(mounts) => mounts,
                Err(e) => {
                    self.host_failure(format!("mountinfo, to size {}: {e}", front.path.display()));
                    return Settle::Stop;
                }
            },
        };
        let started = self.host.now();
        let budget_end = sizing
            .left
            .map(|left| started.checked_add(left).expect("monotonic time overflow"));
        let until = match (budget_end, self.deadline) {
            (Some(b), Some(d)) => Some(b.min(d)),
            (b, None) => b,
            (None, d) => d,
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
        let ended = self.host.now();
        sizing.mounts = Some(mounts);
        let took = ended.saturating_since(started);
        sizing.took += took;
        sizing.left = sizing.left.map(|left| left.saturating_sub(took));
        self.record_overrun("tally", &front.path, started, ended);
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
            // single: the re-match finds what is there now and reports the
            // unit `stale`, as for any unit gone after the tally.
            Ok(Sizing::Gone | Sizing::Missing) => Settle::Ready(front.clone()),
            Ok(Sizing::Paused | Sizing::NoRoom) => {
                if self.past_deadline() {
                    self.stop(&Stop::DeadlinePassed {
                        path: front.path.clone(),
                    });
                    return Settle::Stop;
                }
                // `--tally-budget` ended (`NoRoom` cannot happen: no cursor
                // is kept): the rest of the tier by its stored keys.
                sizing.spent = true;
                self.report.tally_cut_short = true;
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
        queue: &mut TierQueue,
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
            queue.push_member(found, of.clone(), now);
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
