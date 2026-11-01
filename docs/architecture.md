# diskgc architecture: index, tally, reclaim walk, implementation

Normative for how diskgc gathers information and orders its work, and how
it is built and deployed. What is deleted and how safely is in
[safety.md](safety.md); the user-facing contract in
[reference.md](reference.md).

## Gathering and reclaiming

diskgc separates **knowing what could be freed** from **freeing it**. When a
disk is nearly full is the worst moment to discover what is big: walking a
45 GiB `/tmp` with 143,000 top-level entries took ncdu over 7 minutes on the
development machine.

1. A **tally** builds and maintains an **index** of what the profiles find,
   with size estimates, ages and last-checked times: slowly in the
   background in `watch`, at full speed for a bounded time in `ensure`.
   Both save it as a snapshot, so each run starts warmer.
2. A **reclaim walk** takes candidates from a **priority queue** over that
   index and acts on them, after the mandatory per-candidate safety checks.

**The index is a cache, never an authority.** It decides the order of work
and saves global discovery and size tallying. It never decides whether a
deletion is safe: every check in [safety.md](safety.md), including the
eligibility walk of each candidate's tree, runs on the current filesystem
state at deletion time. A stale, partial, corrupt or missing index can make
diskgc slower or pick a worse candidate first; it can never make it delete
something the checks would refuse.

## The index

A set of trees, one per filesystem, of **units**: things the source
profiles found, with what the tally learned about them. Its structure,
constants and the snapshot format are tentative (PRD, Irreversibility
map); the rules below are decided for the first implementation
(Mark, 2026-10-25: collapse at the ceiling, `ensure` writes the
snapshot, a gentle background tally and a fast synchronous one).

```
Unit = Single | Block

Single {                        # one candidate
  identity                      # (st_dev, inode, birth time); see Identity
  name                          # last path component (bytes); the path is the trie path
  finders                       # set of (profile, profile hash, root) that found it
  est_bytes, files              # allocated blocks, each inode counted once
  newest_change                 # newest mtime/ctime seen in its tree (idle-* filters, `oldest`)
  checked_at, tallied_at        # monotonic in memory, wall clock in the snapshot
  cursor                        # resumable tally walk of a big tree; in memory only
  refusal                       # the last tally refusal and when (a hint, see Refusals)
}

Block {                         # many candidates, collapsed
  site                          # identity of a directory D (st_dev, inode, birth time)
  finders                       # the members' common set of (profile, profile hash, root); a lump's: their union
  family                        # name family of the members, or any
  members                       # count; names and identities are not stored
  est_bytes, files              # sums over the members
  oldest_change, newest_change  # min and max of the members' newest_change
  tallied_at                    # oldest tallied_at among the members
}
```

- **Granularity.** One `Single` per candidate (one pytest session, one
  Cargo target, one top-level `/tmp` entry) for as long as memory
  allows. A **block** stands for candidates found by its finders
  beneath its site directory `D`: a family block for those directly
  beneath `D` whose name is in its family, an `any` block for every one
  at any depth. Name family is the `survey` rule
  ([reference.md](reference.md#survey)). A candidate discovery finds
  that a block stands for (one of the block's finders found it) is not
  inserted again: a block stores no names, so its `members` cannot tell
  it from one it already counts and stays as it is; a path that holds a
  single gains the finder instead. Blocks exist only through
  collapse (below); there is no eager bucketing by size, so with memory
  to spare every candidate keeps its own place in the order. (A dry-run
  plan of 100,000 rows is a report concern, not an index one: the dry
  run judges every candidate whatever the index holds.)
- **Identity** is (`st_dev`, inode, birth time from `statx`), falling
  back to (`st_dev`, inode, the parent directory's inode; for a mount's
  root, its own inode) where the filesystem reports
  no birth time; no mount id (mount ids are per boot and reused), so the
  stored identity survives in a snapshot. ctime and size are not
  identity; a change only means the estimate is stale. The stored
  identity is a match key, not the identity the deletion checks compare
  against: at reclaim time the unit is reached and `statx`ed again, and
  that `statx` is its discovery
  ([safety.md](safety.md#deletion), Discovery). A path that several
  profiles find is one unit with several finders, so it is counted
  once.
- **Finders, not a shape hash.** Each finder records the profile, a hash
  of that profile's definition (kind, `find`, `where`, `action`) and the
  root it was bound to when it found the unit. A unit enters a tier's
  queue only through a finder whose profile and root are what that tier
  binds in this run and whose profile hash matches the loaded
  definition; a finder whose hash no longer matches is dropped (a unit
  left with none is evicted). Changing tiers, `--select` or `--roots`
  therefore invalidates nothing: units bound to roots this run does not
  use stay for the runs that do (a CI runner calling `ensure` with
  different roots keeps one snapshot warm for all of them).
- **Trie and queue.** Units live in a per-mount path trie, which gives the
  reclaim walk a cheap first answer to overlap questions (`protect`,
  `!profile`); the deletion-time checks answer them again from the disk.
  Each root a finder names also records `discovered_at`, the time of its
  last complete discovery listing, which drives the tally's schedule and
  `coverage` in the report. The **priority queue** is a derived view:
  for each tier, the units its expression selects on the requested
  filesystem, ordered by group, tier and the tier's `order`. Changing
  tiers rebuilds the queue, not the index.
- **Freshness** has two levels. A **re-check** is cheap (`statx` the
  unit's root and its parent, or a block's site: still there, same
  identity). A **re-tally** walks the tree to refresh size and
  `newest_change`; directory mtimes do not reveal growth deep inside a
  tree, so re-tallies are scheduled by age (The tally: Schedule).
- **Eviction.** A unit leaves the index when a re-check or a collapse's
  re-validation finds it gone or replaced (another identity under the
  name), when a complete discovery listing of its root no longer finds
  it, when its last finder is dropped, or when an action removed it
  (Index update after an action). A replaced path is a new unit when
  discovery finds it again. Nothing is kept for a path that is gone.

### Memory budget and collapse

The index (units, the path trie and live tally cursors together) has a
fixed budget of 64 MB, a constant in code (not configurable in v1;
injectable in model tests). It is a ceiling, never exceeded: an
insertion that would cross it first collapses existing units until the
index is at or below 75 % of the budget (the **low-water mark**), then
inserts. The tally never stops adding because of memory, and
`coverage` never reports `partial` because of it. The count is an upper
bound of the heap the index owns, from the shapes of its containers
(`diskgc-core` `index::memory`; checked against a counting allocator):
about 370 bytes per `Single` with a 16-byte name (its trie node, name,
identity, finders, counters; the allocator measures about 325), so the
budget holds roughly 170,000 singles: the development laptop's `/tmp`
(about 155,000 top-level entries) still fits without collapsing;
collapse is for larger trees and many roots on one filesystem. The
finder-set and mount tables count against the same budget and shrink
with the units: after every collapse pass, a finder set or mount no unit
uses is dropped (a finder names its root by path; a set's id is never
reused, so an id kept past the sweep is stale, never another set's).

**No insertion is refused, no coverage is partial** (Mark, 2026-10-25).
Collapse continues past one block per (finder set, root) with
**lumps** (below) up to one lump per mount. A candidate that still does
not fit as a single then joins its mount's lump, which gains its finder.
The budget only has to hold one lump per mount with its finders and the
tables, a few hundred bytes per mount; below that floor (only a test
budget can be) an insertion is refused. The budget is never raised.

**What a collapse does.** It picks a **site**, a directory `D`, and a
finder set `F`, and replaces every unit beneath `D` whose finders are
exactly `F` (singles and finer blocks) by one block at `D`. Coarsening
has two steps per site, taken in this order: first the units of one
name family directly beneath `D` (the old "bucket"), then all families
(`any`) beneath `D`; once `D`'s block is `any`, the next collapse of
that subtree climbs to `D`'s parent and absorbs `D`'s block and its
siblings. A site never lies above its finders' root (patterns are
relative to it) nor on another mount than its members.

**Lumps.** Once those steps make no more progress (everything is one
block per finder set and root) and the index is still above the
low-water mark, collapse merges across finder sets: a **lump** at a site
`D` takes every unit beneath `D`, of any finder set, into one block of
family `any` whose finders are the union of theirs. Lumps take the same
key (small first, deepest first), so they climb towards the mount's
root; at the mount's root a lump may take a single unit (a deep path
becomes one block), so the limit is one lump per mount. A lump never
crosses a mount. Its site may lie above some of its finders' roots: for
discovery a lump never stands above a finder's root, because a tier
expands it by its own discovery from the deeper of the lump's site and
the tier's bound root (below, Expanding a block). The price is the
widest expansion: more listing and `statx` at delete time, every member
still checked live.

Every collapse step is taken only if it shrinks the index, counted
exactly beforehand (the units and the trie nodes it frees against the
new block and, for a lump, its union finder set, crediting the sets only
its units used); a long family name over singles whose trie nodes stay
could otherwise grow it. So every step strictly shrinks the index and a
sequence of collapses ends, at worst, at one lump per mount: the index
then knows sizes and age ranges per mount and nothing finer.

**Which site first.** A lexicographic key over the candidate sites
(every directory with at least two units of one finder set beneath
it):

1. sites whose units are all below 64 MiB estimated before sites with
   a larger unit (small candidates matter least to freeing space and to
   `best-fit`; big ones keep exact places);
2. then the **deepest** site (a deep block expands with a short, narrow
   discovery at reclaim time; a block at `/tmp` itself would mean
   discovering all of `/tmp` again).

Ties are broken by the trie's path order, then the family step before
the all-families step before a lump, then the finder set and the
family, so collapse
is deterministic for a given index. A pass plans at most 1,024 steps
in that order (a bounded working set), taking only steps that would
shrink the index now (counted at planning), and executes them until the
low-water mark; the next pass plans again from what is left. A step
that would not shrink it never takes a place in the window, so a crowd
of them (long family names over singles whose nodes stay) cannot stop a
collapse above the low-water mark while a shrinking step exists
(TASK-230). Further keys (most units, youngest first) wait for a
measured case where these two choose badly.

**Re-validation before a collapse.** The collapse first proves the
inputs are still what the index says, with the cheap re-check: `D` is
reached from its mount root one directory at a time without following
symlinks or crossing mounts (as the tally walks) and must have the
recorded identity; then each unit to be absorbed is re-checked (its
root, or a finer block's site, still present beneath `D` under the same
identity). A unit that fails (gone, replaced, not permitted) is evicted
and contributes nothing to the block's `members`, sums or ages; a
failure of the host itself (EMFILE, ENOMEM, EIO) stops the collapse
with that error instead. If `D` itself is gone or replaced, nothing is
collapsed there: every unit at and beneath it is evicted and the next
site is taken; a block sited at `D` under another identity than `D`'s
now is evicted (and a single replaced by discovery takes the blocks
sited at its path with it). Re-validation keeps a block's estimates honest
(vanished members would inflate it and misorder the queue). It is not
a safety check and grants nothing: a block's members are found and
judged afresh when it is expanded. In `watch` the re-validation's
`statx` calls are tally work inside the duty cycle; there the tally
does no other work until the index is back under the low-water mark.

**No re-splitting in place.** A block dissolves only through
discovery: when a complete discovery listing of its root runs while the
index is below the low-water mark and the block's recorded `members`
fit without crossing it, discovery inserts the members it listed as
singles and drops the block; otherwise the block stays. A dedicated
split step waits for a measured need (backlog).

### Order within a tier

- `oldest` (default): least recently changed first (by `newest_change`).
  Keeps the LRU gradient even in tiers without an `idle-*` filter. A
  block ranks by its `oldest_change`, the oldest of its members, so
  collapsing never lets a younger candidate overtake the oldest member.
- `best-fit`: the smallest unit whose estimate covers the remaining need,
  otherwise the largest. Frees something sizable in one step when the
  disk is nearly full. A block ranks by its total `est_bytes` (freeing
  it all is the step) and after singles of the same size, since it is
  many deletions. Because the next pick depends on space actually
  freed, `best-fit` is adaptive and has no exact dry-run equivalent.
- **Selecting a block.** A block enters a tier's queue when one of its
  finders satisfies the tier's source terms (profile, bound root,
  profile hash) and its age range could pass the tier's idle filter
  (`oldest_change` for a positive one, `newest_change` for a negated
  one). Filters, `!profile:`, `protect` and refusals are applied per
  member after expansion, never to the block as a whole.
- **Expanding a block.** When a block reaches the head of its tier's
  queue, the reclaim walk runs the tier's own discovery restricted to the
  subtree beneath the block's site, or from the tier's bound root where a
  lump's site lies above it (one code path: the same patterns
  relative to the bound root, any depth they reach, no symlinks, no
  mounts, the same listing bounds) and keeps the names in the block's
  family. Like per-tier discovery, it acts only on an expansion that
  completed: one stopped by the deadline or an error acts on nothing it
  listed. A multiply linked member is not taken (the single's rule in
  The reclaim walk: discovery judges it under the name it meets first);
  it still counts as present, so its block is not evicted as empty.
  Members are held as per-tier discovery holds its candidates
  (outside the index budget, which bounds what is kept between runs).
  They join the tier's queue in the tier's order, keyed for `oldest`
  by `max(mtime, ctime)` of the member's own root (a cheap stand-in for
  `newest_change`; a deep change makes it look older than it is, and
  the deletion-time idle check then skips it `not_idle`). Under
  `best-fit` the expanded block's members are taken in that `oldest`
  order until the need is met or the block is done, then `best-fit`
  picks again. A member already in the queue as a single (found by the
  same identity) is not added twice, and the tier's later per-tier
  discovery skips what an expansion in this run already queued. This is
  the price of coarser blocks: more listing and `statx` at delete time,
  and a coarser order, never a weaker check.
- **Order with blocks and hints is approximate.** A plan
  (`--dry-run`) judges and orders every candidate with full
  information. A reclaim takes the same order in the first tier with
  candidates when its index holds no blocks and no stored refusal hints
  and its tally was not cut short (ties by path, not by discovery's
  order; under `oldest` also only while no timestamp goes back during
  the run and every root reports its times, The tally: Synchronous,
  pass 2); later tiers are ordered by their stored keys or, for units
  this run's tally did not size, by the provisional key (`max(mtime,
  ctime)` of the unit's root), so their order can differ from the plan's
  too. Blocks (ranked by their oldest member, members by their root's
  times) and hints can reorder candidates within a tier. Tiers and
  groups are never reordered, and the set of candidates each tier can
  reach is the same.

### Refusals and index updates

- **Refusals.** A tally refusal (kept, another user's, too many entries,
  a checkout) is stored with its time as a hint. A refusal found by this
  run's own synchronous tally skips the candidate for the run, as
  before the index existed, once the unit is re-matched at its path (a
  unit discovery would not reach, below a directory it cannot list, is
  left to discovery, which reports what it meets instead; TASK-252):
  the walk knows these by the tally's own list,
  never by a hint's time (a hint timed in the run by a clock step or a
  corrupt snapshot would otherwise suppress an eligible candidate and let
  a later tier run first; Codex review of TASK-31). One from an earlier
  tally (the snapshot, a long-running `watch`), whatever its time, only
  ranks the unit after the tier's other units;
  before the tier is done it is tallied again, read-only and without
  taking any lock (a kept or another user's Cargo target is never
  locked for this), and then treated as any other candidate, so a
  stored refusal can never make a tier look exhausted.
- **Index update after an action.** A unit whose deletion completed is
  evicted. An expanded block's member that is deleted subtracts its
  measured size (from its eligibility walk) and one member from the
  block; a block reaching zero members is evicted. A deletion that
  stopped part-way (`changed`, an error) marks the unit for re-tally and
  keeps it if its root is still there. A skip changes nothing but the
  unit's `checked_at` and refusal hint; `stale` and `missing` evict it.
  Docker and Nix units are re-queried by the next tally.

## The tally

The tally updates the index; the reclaim walk enacts it. One engine,
over the `Host` port, in two modes that differ only in pacing and in
the order of work.

- **Background (in `watch`, asynchronous): must not load the system.**
  The tally runs forever in mini-batches of at most 50 ms or 2,000
  filesystem operations, with a **worker duty cycle**: after a batch
  that took *t*, it sleeps 19 × *t*, so the tally worker is active about
  5 % of wall-clock time whatever the device. This bounds the worker,
  not the device: it does not measure device busy time, and several
  `watch` processes on one disk add up. It also runs at idle I/O
  priority and `nice 19` (idle I/O priority is honoured only by the BFQ
  scheduler; on NVMe with scheduler `none` it does nothing). Actual
  contention is measured by a container test
  ([testing.md](testing.md#5-end-to-end-tests-in-containers)). Big trees
  are tallied across many batches via their `cursor` (the path of names
  below the unit's root and the last name done in each directory, in
  byte order), which survives entries vanishing under it (`ENOENT`
  ends that branch). A cursor holds no file descriptor between batches:
  each batch reaches the unit's root again from its mount (checking its
  identity) and reopens the cursor's directories, so a sleeping tally
  never keeps a directory inside a candidate open (another process's
  in-use check would see it). The cursor keeps the listing of each
  directory on its path (so a huge flat directory is listed once per
  walk, not once per batch; entries created after it was listed are
  missed); a listing is charged one operation per name. A batch that
  paused before doing any entry (reaching and reopening took it all)
  marks the cursor stalled, and the next batch does at least 32 entries
  whatever its bounds, so every walk ends. One cursor is live at a
  time, counted against the index's budget with its listings; it enters
  at most 64
  directory levels and remembers at most 16,384 hard-linked inodes (both
  tentative constants; beyond them the estimate is coarser). A walk past
  the deletion's entry budget stores a `too_many_entries` refusal hint,
  one holding an entry this user may not read an `unreadable` one, and
  one holding a `systemd-private-*` name a `contains_special` one (the
  name is never statted, as the eligibility walk never stats it).
  Cursors are not persisted; a unit whose walk did not finish counts as
  untallied. **Discovery is resumable too** (Mark, 2026-10-25): a
  listing **round** of one (profile, root) runs across batches within
  their bounds, by the kind's own discovery walk kept as names only (per
  directory on the way down: its name, identity, the patterns still
  active, its sorted names and how many are done; no file descriptor
  between batches). Each step reaches the root again and reopens the way
  down by name: a directory gone ends its branch; one replaced by another
  inode, or a replaced root, is refused (`changed`), so that round is not
  complete; names created in a directory after it was listed are not
  found by that round. A step after a stalled one (paused before any
  operation) does at least 32 operations whatever its bounds, as for the
  sizing cursor. A listing is charged one operation per name, and a
  frame keeps only the names a pattern matches. What a round finds is
  inserted at once. One round is live at a time, counted against the
  budget with the sizing cursor (never collapsed for: a round that does
  not fit is dropped, partial, and its source waits its period), and
  finished before another starts. A round spans minutes on a big root:
  a candidate renamed during it from a directory not yet listed into
  one already listed is not found, so a complete round can evict a unit
  that still exists (it is found again by the next round, and the
  reclaim walk's own discovery finds it meanwhile: only warmth is
  lost). A round is **complete** when its walk
  reached the end and no step of it refused anything. A job that fails with a failure of the host
  (`EIO`, `EMFILE`) waits its period (a unit's: 10 minutes) and the batch
  goes on; after a batch whose every job failed the sleep is at least
  1 s, doubling per such batch up to 60 s; each distinct failure is
  logged once. The worker's time
  `t` is the whole turn: the host-lock test and the pressure read
  included.
- **Pressure brake (background)** (Mark, 2026-10-25: duty cycle and
  priorities, and back off on measured pressure). Before each batch the
  tally reads the system-wide pressure stall information
  (`/proc/pressure/io`, `memory` and `cpu`, the `some avg10` value; a
  read-only `Host` query). The system is **under pressure** when
  `io` or `memory` is above 10 % or `cpu` above 50 % (CPU is held to a
  higher bar: a busy laptop runs at load 5 to 18, and `nice 19` already
  yields the CPU). Under pressure the batch is skipped and the sleep
  doubles, from 19 × the last batch's *t* (at least 1 s) up to 60 s;
  the first reading below all three thresholds runs a batch and returns
  the sleep to 19 × *t*. Where PSI is absent (a kernel without
  `CONFIG_PSI`, booted with `psi=0`, or `/proc/pressure` unreadable),
  the duty cycle and priorities alone apply, with one log line at
  start. The thresholds and the 60 s cap are tentative constants in
  code. The synchronous tally ignores pressure.
- **Schedule (background).** Three kinds of work, each with a period:
  discovery of each (profile, root) every 5 minutes (listing the profile
  roots for new and vanished candidates), a re-check of each unit every
  10 minutes, a re-tally of each unit every 60 minutes; plus collapse
  as memory requires. The next job is the one most overdue
  relative to its period (lateness ÷ period), so none starves the others
  on a busy `/tmp`. Constants are tentative and live in code. A source
  is scheduled by its last listing attempt (a listing that is never
  complete must not run every batch), never listed: due from the start;
  a single never sized is due from the start too (an estimate nobody has
  outranks a re-tally); a single needs sizing when never sized or sized
  (tallied, or refused as a hint) 60 minutes or more ago, one rule for
  both modes. A listing is itself a re-check
  of what it finds (their `checked_at` is refreshed), so
  re-checks mostly serve units of roots the tally does not list. Blocks
  are neither re-checked nor sized here (they have no check time and no
  member names): collapse re-validates them and complete listings
  recount them.
- **What a listing means.** A listing is **complete** when discovery ran
  to its end and refused nothing on the way (no unreadable directory, no
  candidate whose own `where` could not be told) but container storage,
  which is excluded on purpose, never unread (TASK-242,
  [safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate)):
  a root below `~` that holds rootless Docker or Podman storage still
  completes, and what the index held inside storage is forgotten. Every listing inserts
  what it found; only a complete one is evidence of absence: a single of
  that finder below its root that the listing did not find loses the
  finder (and leaves with its last one, or when the smaller finder set
  would not fit), and the blocks it stands for are dissolved (a block of
  one finder whose listed members fit under the low-water mark) or
  **recounted** (Mark, 2026-10-25: a complete listing of a block's site
  recounts its members and estimates; partial listings leave them). A
  listed candidate counts for the shallowest block that stands for it,
  as insertion's coverage does, unless its path holds a single (then it
  is that single). Recounting to fewer members shrinks
  `est_bytes` and `files` in proportion; to more, the new members are
  untallied, so the estimate becomes a lower bound (`tallied_at` none);
  to none evicts the block; ages are left. A block of several finders is
  recounted only when every one of them was listed completely in the
  same round: the synchronous tally's pass 1 is one round, each
  background listing its own. A background round keeps no list of what
  it found: a single of its finder whose `checked_at` is older than the
  round's start was not found (every insertion refreshes it; a re-check
  during the round does too, which only delays that eviction by a
  round); blocks of its finder alone are recounted from the members
  counted as the round inserted them, unless a collapse ran during the
  round (the blocks changed under the counts: they are left as they are);
  background rounds never dissolve a block (no member list).
- **Pausing:** before each batch the background tally tests, without
  blocking, whether anyone holds the lock a reclaim of its user takes:
  the host lock, or, for a user on a host without it, the user's
  per-user lock if it exists ([safety.md](safety.md#host-lock); the
  probe makes nothing) (a `LOCK_SH | LOCK_NB`
  `flock` on its own read-only open of the lock file, closed at once;
  a reclaim holds it exclusively, so a shared probe detects it and
  never excludes another prober; the probe can make one of a reclaim's
  10 to 100 ms lock retries fail, [safety.md](safety.md#host-lock)); if so, a reclaim is
  running (in this or another process) and the tally waits, polling
  every second (a lock released by its holder can still look held for a
  moment, [safety.md](safety.md#host-lock)). A missing host lock for
  root, an unsafe per-user lock, or one that does not exist means no
  reclaim of this user holds it: the tally goes on (the first two with
  one log line); any other
  failure of the test pauses it. The background tally never speeds up
  under pressure.
- **Synchronous (in `ensure`): may load the machine; must be fast and
  useful.** It runs at full speed (no sleeps, normal priority), for at
  most `--tally-budget`, **before** taking the host lock (it is
  read-only; but for a first tier ordered `oldest` its sizing, pass 2,
  runs under the lock, as the reclaim walk reaches each unit, still
  read-only and within the same budget), over the tiers the run selects,
  in their policy order (group, tier), in two passes:
  1. **Breadth (discovery):** for every selected tier, list its sources'
     roots by their patterns and `statx` each candidate's root. Every
     candidate gets a unit with identity and a provisional key
     (`max(mtime, ctime)` of its root). A (profile, root) whose
     discovery in the loaded snapshot is younger than 5 minutes is not
     listed again.
  2. **Depth (sizing), first tier only** (Mark, 2026-10-25): tally the
     trees (size, `newest_change`) of the **first** selected tier's
     stale units, that is never tallied or tallied more than 60 minutes
     ago. A unit refused (a hint) within the 60 minutes counts as
     sized. The first tier is the earliest in policy order with a
     discovered candidate: the one that will act first. Under
     `best-fit` the tally sizes them before the host lock, by the stored
     estimate, largest first, unknown last: the first pick compares
     every estimate. Under `oldest` the reclaim walk sizes them, as the
     tier's queue reaches each one (TASK-251: 140,000 tiny rc
     directories on the development laptop would otherwise all be sized
     before the first deletion, whatever the goal needs). The order is
     the one full sizing gives when each unit's key in the queue is at
     most the `newest_change` its sizing finds: the walk then sizes the
     unit with the lowest key and takes a sized unit once no unsized
     one's key comes before its new key. `max(mtime, ctime)` of the
     unit's root is such a lower bound (the sizing walk counts the root's
     own times), so a key from this run's listing of the root serves; a
     key stored before (an earlier tally's `newest_change`, which a
     removed future-dated file can leave too high, or a snapshot's
     listing this run did not repeat) does not, and the walk reads that
     unit's root again (one `statx`) before ordering the queue (Codex
     review of TASK-251). The order is **exact** with keys from this run
     and roots read now; it is **approximate**, never a check, when
     timestamps go back during the run (a clock stepped back, a time set
     into the past), or when a root reports no times (keyed as just
     changed, as discovery keys it). The walk sizes as the tally does
     (read-only, the same walk, a refusal is this run's tally's), keeps
     no paused walk, and spends what pass 1 left of `--tally-budget`:
     each stretch of it (the root reads, or one unit's sizing) is charged
     from before it reads the mount table; the wait for the host lock and
     the walk's own work between stretches are not; past the budget the
     rest of the tier goes by the keys it has and `tally_cut_short` is
     set. It also counts against the tier's own `timeout`. A failure of
     the host there is an error of the run, as anywhere in the walk (The
     reclaim walk: a failure of the host). Units with a refusal hint are
     not sized ahead: they come after the tier's others (Refusals). The
     walk's tier takes this sizing when its sources are among the
     first tier's (preflight may drop a root on another filesystem), and
     a later `oldest` tier with the same sources continues it; one line
     says so when no tier took it. Other later tiers keep their stored
     sizes and ages for their order (units without any are ordered by
     the provisional key, after sized ones under `best-fit`); the
     reclaim walk's own checks size what it acts on. The same rule
     applies with and without a seed. The tally then ends, even with
     budget left, so the reclaim starts sooner. Blocks are not sized
     (their members have no names in the index); the reclaim walk's
     checks size the members it expands.

  **Useful** means: when pass 1 has finished, every selected tier has a
  queue in its order and the reclaim walk can act on it; pass 2 only
  sharpens the first tier's order (true `newest_change`, sizes for
  `best-fit` and the report). When the budget ends during pass 1, the walk's per-tier
  discovery covers what is missing. Whether one worker or several walk
  disjoint trees is an implementation choice measured on the 100,000-
  entry fixture, not a contract.
- **Sources of a run's index.** `watch` uses its own live index,
  seeded at start from a valid snapshot (the same validity rules).
  `ensure` and `survey` load the snapshot as a **seed** when it is valid
  (Snapshot, below), whatever its age, and run the synchronous tally on
  it: discovery is redone for roots not listed in the last 5 minutes,
  and only the first tier's stale units are re-sized; either way the
  seed only orders work. (`survey` sizes everything it reports.) Without a valid snapshot, or with `--no-index`, the synchronous
  tally starts from nothing.
- **`--no-index`:** the run neither reads nor writes the snapshot (no
  snapshot operation at all; a user's reclaim on a host without the host
  lock still makes its per-user lock, which may be in
  `~/.local/state/diskgc`, [safety.md](safety.md#host-lock)); it builds its index from nothing by the
  synchronous tally (within `--tally-budget`) and discards it at exit;
  a run that tallies says so in one log line. The live index is used as
  in any run. For diagnosing a suspect index; never needed for safety.
- **Snapshot:** the index written atomically to
  `$XDG_STATE_HOME/diskgc/index-<fs>` (`$HOME/.local/state/diskgc` when
  `XDG_STATE_HOME` is unset or not absolute; no snapshot at all when
  neither is absolute; mode 0600: it lists paths of the user's files, no
  contents). The state directory (with the lock file) is created mode
  0700 by a run that tallies, before its reclaim walk, and must be a
  directory of diskgc's user that no one else may write to (otherwise
  nothing is read or written there, said in one log line). It is
  reached following symlinks, like the configuration file: a user's
  `~/.local/state/diskgc` that is a symlink to another directory of
  theirs is used, and only diskgc's own `index-<fs>` name is ever
  replaced there (TASK-30 review: accepted). `<fs>` is `uuid-<uuid>`, the filesystem's UUID where the system
  names one (for btrfs, whose superblock's device is anonymous, its FSID
  read from `PATH`'s held descriptor with `BTRFS_IOC_FS_INFO`; otherwise
  the udev link `/dev/disk/by-uuid` whose block device is the device
  mountinfo names for `PATH`'s mount), otherwise `dev-<major>-<minor>` of the
  device mountinfo names (Filesystem matching: one key for every
  subvolume of one btrfs); not `f_fsid`, which is 0 on tmpfs on older
  kernels (and per subvolume on btrfs), and not the mount path. tmpfs,
  overlays and containers without udev links get the device number, so
  their snapshots are valid within one boot only (Validity). A UUID that cannot be read
  (`EACCES` on the udev links) or used (not hex digits and dashes) falls
  back to the device number with one log line.
  - **Who writes, when.** `watch` every 5 minutes and on exit. `ensure`
    once, when its run tallied, at the run's one exit after the tally
    and before its final space sample (so the sample, the exit code, a
    signal's status and the report's `duration_ms` include the write):
    after the reclaim walk, and equally when the run ends earlier under
    the host lock (the space found there, a root or `protect` refusing
    the run) or because the host lock was refused. Never past the
    deadline and not after a terminating signal: a run that waited for
    the host lock until its deadline, or was interrupted, writes none and
    says so in one line (TASK-270). An index whose
    encoding is larger than a snapshot may be (twice the budget), or
    past another limit the reader holds a file to (a path or a sum,
    Validity: a collapse adds a single's counters to a block, so sums
    at the cap can grow past it), is not written, with one log line:
    every later run would ignore it (TASK-279). A run that exits 0 at once because space is
    already there stays an instant no-op (Mark, 2026-10-25): no tally,
    no snapshot write. Warmth on a machine that is rarely short of
    space comes from `watch` or `watch --tally-only`, not from
    `ensure`. `survey` and
    `--dry-run` do not write (read-only commands write no state); a
    dry run does not read it either (its plan judges every candidate
    whatever an index holds). Not after a terminating signal, and not
    at or past the run's deadline (checked again right before the
    write). `ensure` seeds and writes only when its run tallies (it
    selects a tier the index serves).
  - **Never at the cost of the goal.** The snapshot is usually on
    `/home`, possibly the filesystem just reclaimed. Unless the state
    directory's filesystem is proved independent of the one the run
    works on (Filesystem matching: on it, on an overlay of it, or
    unproved), the writer skips the write (one log line) unless
    available space, the lower of the state directory's and `PATH`'s
    held descriptors' samples, minus what the write takes stays at or
    above `--min-free` (in `watch`, its
    `--min-free`; in `watch --tally-only`, its optional `--min-free`,
    and without one only a failed write skips it). A percentage is
    resolved at each such write against the size `PATH`'s sample
    reports, so the guard holds from a `watch`'s first write even when
    its first sample failed and its goal is not resolved yet (TASK-287);
    a write whose samples fail is skipped. What it takes is
    its size rounded up to whole blocks (`f_frsize`) plus one block for
    the new directory entry: a file of a few hundred bytes still takes
    a block (TASK-30 review). The state directory and the lock file
    are made before the walk (above), so what they take is already in
    the samples the walk acts on; so is a per-user reclaim lock made
    in `~/.local/state/diskgc` ([safety.md](safety.md#host-lock)), which
    is made when the reclaim locks. The state directory is placed by
    its own mount (its descriptor's `stx_mnt_id`), never the lock
    file's: a file bind mounted over `index-<fs>.lock` puts the lock
    file on another filesystem while the snapshot is still written into
    the directory, and every write is made relative to the directory
    held since the lock was opened, never the path opened again. On a
    filesystem stored on a loop device it is not written (Filesystem
    matching). The temporary file is an `O_TMPFILE` linked in only
    when complete, so a failed write leaves nothing behind; `ENOSPC` or
    any other write error is one log line and never changes the exit
    code.
  - **Single writer.** A separate lock file, `index-<fs>.lock`, that is
    never renamed. `watch` holds it exclusively for its lifetime; at
    start it waits for it at most 10 seconds (an `ensure` holds it only
    while writing) and then fails (exit 2: another `watch` for the same
    filesystem and user). `ensure` tries it just before writing, for at
    most 100 ms (a lock its last holder just released can look held for
    the length of another process's `/proc` read,
    [safety.md](safety.md#host-lock)): still held (a `watch` runs, or
    another `ensure` is writing) means it skips the write with one log
    line, losing only warmth: a running `watch` keeps its own index and
    re-checks what `ensure` deleted. The lock file is created by its own
    state operation (The `Host` port).
  - **Replace, never merge.** The writer's index already started from
    the seed and carries this run's tally and actions, so it replaces
    the file whole (temporary file + rename). Two `ensure` runs that
    overlap can overwrite each other's warmth (last writer wins); each
    file is a complete, valid index.
  - **Atomicity and integrity.** A reader sees the old file or the new
    one: the `O_TMPFILE` is filled, linked in as `index-<fs>.new` and
    renamed onto `index-<fs>`; a crash between the link and the rename
    leaves a complete `.new`, which the next write renames onto the
    snapshot before linking its own. There is no `fsync` (it can stall
    on unrelated I/O); a file torn by a crash fails its checksum and is
    ignored. Two filesystems never share a file name: a UUID names one
    filesystem, and a device number one filesystem within a boot.
  - **Format.** Binary (paths are bytes, not UTF-8): a header (magic,
    format version, diskgc version, the `<fs>` key, the owner's uid,
    the boot id, the write time), a table of finders with each source's
    `discovered_at`, a table of mounts, the singles and the blocks, and a
    trailing 64-bit FNV-1a checksum (it detects tearing and damage, not
    tampering: the file is the user's own and grants nothing). The byte
    layout is in `diskgc-core` `snapshot::format`. No fixed size beyond
    the budget: a file larger than twice the budget is refused unread.
    Cursors and listing rounds are not written. A single's `checked_at`
    is written: `watch`'s background schedule reads it (TASK-32).
  - **Times.** Monotonic times mean nothing in another process's boot,
    so none is written: each is written as the wall-clock time it
    corresponds to at the write, and read back as the monotonic time
    that is the same age at the load, at least one nanosecond before the
    load (so before the loading run started). A time that cannot be read
    back so is unknown: in the future (the wall clock was set back) or
    older than the monotonic clock's origin (this boot). Unknown is as
    if never: no `discovered_at` (listed again), no tally (sized again),
    no refusal hint. Refusal hints decide nothing about this run either
    way (Refusals).
  - **Validity.** Loaded as a seed only if it is a regular file (never
    through a symlink) owned by diskgc's user that no one else may read
    or write (`mode & 0o077 == 0`), and magic, format version, checksum,
    diskgc version (profile hashes are stable only within one build),
    `<fs>` key and owner uid match; for a filesystem without a UUID
    (tmpfs), also the boot id, since its device number and inode
    numbers repeat after a reboot. Every path must be absolute and
    canonical (no `.`, `..`, empty or doubled component, no trailing
    `/`), a unit strictly below its mount point and below each of its
    finders' roots, a block's site at or below its mount point, every
    index in range (an identity's device is its `statx` device, which on
    btrfs is its subvolume's, not the mount's: not compared; the reclaim
    walk compares it live); no path longer
    than 4,096 bytes or deeper than 256 components (the index's trie is
    walked and dropped recursively, one level per component); members,
    `est_bytes` and `files` summed over all units at most 2^62 each (so
    no sum the index makes of them, plus what the run adds, overflows):
    one entry that is not is a bug or tampering, so the whole snapshot
    is ignored (the index asserts these shapes and its sums panic on
    overflow; a loaded file must never reach either, TASK-30 review). A finder whose profile this run binds under another hash
    (the profile was edited) is dropped on load with its
    `discovered_at`, and a unit left without a finder is not restored
    (Finders); finders of profiles this run does not bind stay for the
    runs that do (they enter no queue with a wrong hash anyway). An
    ignored snapshot (missing, another user's, mismatched, corrupt,
    oversized) is one log line naming why, and the run's synchronous
    tally starts from nothing; the report's `index` block (its `source:
    sync`) is TASK-244. A valid write replaces it. Age never invalidates
    a snapshot: the tally re-lists and re-tallies by age.
- **Docker units** carry the reclaimable size the bound backend reports
  (`docker buildx du`, `docker system df`), or no size if unknown; TASK-120
  settles which query, since Buildx writes client configuration.
- **Nix units carry no size: Nix is sized only by its action.** The only
  way to learn what a GC would free is Nix's own liveness computation
  (`--print-dead`), which takes as long as the root discovery of a real GC,
  reports no sizes, and unlinks stale `gcroots/auto` registrations, so it
  is neither read-only nor cheap enough for a tally or a dry run. The
  consequences: a `nix-store` tier is one unit with `estimated_bytes:
  null`; its place in the policy is decided by its group and tier alone,
  never by size (`best-fit` has nothing to compare, and a tier holds one
  Nix unit); a dry run cannot say whether the GC would free anything; and
  a reclaim runs the GC whenever earlier tiers left the goal unmet, even
  when little is dead, costing the GC's fixed time (root discovery and the
  `.links` scan, [safety.md](safety.md#nix-store)). Put the Nix tier after
  cheaper tiers that free comparable space. TASK-98's out-link estimate
  is a separate, hypothetical-root analysis, not this tally.

## The reclaim walk

```
reclaim(min, goal, filesystem, deadline):
  if avail(filesystem) >= min: return met            # no lock taken
  index := live index (watch)
           | synchronous tally (budget) seeded by a valid snapshot, or from nothing
  lock /run/diskgc/lock (a user without it: the per-user lock), waiting until deadline
    on timeout: end (met if avail >= min else lock_timeout)
  if avail(filesystem) >= min: end (met)             # someone else freed it
  preflight all tiers (environmental validation, backend binding)
  for group in groups, tier in group (ranking):
    queue := units of this tier on this filesystem, in the tier's order
    loop:
      if avail >= goal: end (met)
      candidate := next from queue
                   (a block at the head is expanded first: its members,
                   found and statx'ed now, join the queue; under `oldest`
                   the first tier's stale unit at the head is sized first,
                   The tally: Synchronous, pass 2)
                   or: break                            # queue done
      re-match: the tier's discovery restricted to the unit's path finds
        it there; else skip (stale)
      identity as stored; gone or replaced: skip (stale)
      eligibility walk + in-use + selection re-evaluated now   # safety.md
        failed: record skip with reason; update index; continue
      act(candidate, need = goal - avail); record avail_delta; update index
    discover this tier's sources (bounded by their patterns); judge and act
      on what the queue did not judge, as above (the tier's order)
  end: (every way out after the tally, TASK-270)
    write the snapshot (ensure; Snapshot above)
    return met if avail >= min else (error if any errors else exhausted)
```

- **A candidate from the index is a candidate only as discovery would
  make it.** A single is re-matched by the tier's own discovery
  restricted to its path (`fs_kind::discover_beneath`, exact): the same
  walk of the same bound roots (each root judged as discovery judges it),
  the same patterns, `where`, mount and `systemd-private-*` rules, level
  by level, but looking up only the names on the way instead of listing
  each directory. Where discovery would list a directory on the way (a
  pattern has a `*` at that level), the re-match opens it for reading as
  the listing would and reads nothing (`Host::listable`), so a directory
  diskgc can search but not read (mode `0333`) is refused there as
  discovery refuses it (`unreadable`), never passed by name (TASK-252).
  It must find a candidate at exactly that path, with the
  stored identity; otherwise the unit is `stale` (gone, replaced, no
  longer matching, or below another candidate, which discovery would find
  instead) and is never walked or locked. A refusal on the way (a root or
  directory refused) leaves the unit to the tier's discovery, which
  reports it. So does a multiply linked file (a link count other than 1,
  or none reported): discovery finds an inode once, under the first of
  its names it meets, and judges that name, which may lie outside the
  re-match's way (TASK-252; [safety.md](safety.md#other-properties), Hard
  links). **A tier whose roots lie on two mounts of one filesystem** (a
  bind mount: one device in mountinfo, two mount ids) takes no queue at
  all, and its discovery alone decides: one directory entry can be
  spelled under each root, discovery records it under the first root
  that reaches it (roots outermost first) and judges that spelling's way
  down, and a link count does not show such an alias (TASK-252 review).
  One line is logged. A root whose mount cannot be told counts the same. A block's members are found by the same discovery
  restricted to its site (Expanding a block). The identity re-discovery
  saw is then the identity "discovery found" for every later check of
  [safety.md](safety.md#deletion), and the candidate is judged exactly
  as a discovered one: read-only tally (a refusal skips it, never
  locked), then the kind's protocol. The index can thus add work and
  misorder it; it cannot name a path that the run's own sources would
  not find, nor skip a check. The one difference from a listing: a
  directory too large to list (`too_many_entries`) is not listed on the
  way down to a unit, so not refused. (Also, the re-match opens such a
  directory without reading it, so a listing that opens and then fails,
  `getdents` with `EIO`, passes the re-match while discovery stops on it
  as an error of the run: the limit of a host failure that first appears
  after the tally, below.) This holds for units of this
  run's tally and equally for units seeded from an earlier run's
  snapshot whose root was listed less than 5 minutes ago (a root that
  grew past the listing limit since is not listed by this run's tally,
  and its units are found again by name): `too_many_entries` is a
  resource bound, not a safety check, and every per-candidate check
  still applies to each (TASK-30 review: accepted).
- **Found ahead.** The queue finds a *found-ahead batch* of candidates
  again before it judges the first of them, so that one in-use scan,
  started after all of them were found, can serve them: a liveness
  contract takes no scan started before its candidate was found
  ([safety.md](safety.md#producer-liveness), Owner PID, *When*), and one
  scan per candidate made 6,400 pytest sessions take one to two hours
  (TASK-203). The batches (`found_ahead_batches` in the executor):
  - `oldest`, a tier whose source has a liveness contract: 64 first
    (what the first deletion waits for), then each twice the one before,
    up to 512 (`FOUND_AHEAD_MAX`). Each of its batches costs a scan
    however few of the scan's second its candidates use, so the batches
    grow until one fills about a scan's second with the cheapest
    candidates. A batch is at most what was judged before it plus 64,
    which bounds what is found ahead and never judged (the goal met
    first). Its candidates wait longer between being found again and
    being judged: up to their batch's judging, under a second for the
    smallest, longer for large ones. That identity is only what the
    judgement's own reach must find again
    ([safety.md](safety.md#deletion), steps 2 to 4), so a change in
    between is `changed` or `missing`, as with any batch.
  - `oldest`, a plain tier: 64 each time. Its candidates have no such
    floor, so one scan already serves them across batches for as long
    as it is fresh (Freshness); a larger batch would only make them wait
    longer between being found and being judged, and find more ahead of
    a goal that may be met first.
  - `best-fit`: one at a time, since each pick depends on the space
    freed by the last action; a liveness tier ordered `best-fit`
    therefore scans once per candidate.

  A batch whose judging outlasts its scan gets a new one, as any
  candidate does (Freshness): the batches change how many scans a run
  makes, never which scan may serve a candidate.

- **Discovery is per tier.** A tier is not left until its own sources have
  been discovered afresh, so a stale index can never make a pressure tier
  run while candidates of an earlier stale tier exist but are unknown.
  The tier's discovery runs once its queue is done, always (even when the
  synchronous tally listed its roots in this run: one more listing, and
  no rule about which listing counts): it finds candidates created after
  the tally's listing or the snapshot's, ones the index could not take,
  and whatever object now has a `stale` unit's name. It judges only what
  the queue did not: an object the queue judged (by identity) and a path
  the queue gave a verdict (the object there now waits for the next run,
  one verdict per path and run, as before the index) are passed over.
  Tiers the index does not serve (below) are this discovery alone.
  Discovery costs what the profiles' patterns cost (fixed depths, no `**`);
  for `roots: required` profiles such as `cargo-target` it is a bounded walk
  of the roots.
- **The first deletion waits only for its own checks.** With a warm index,
  the time to the first deletion is the eligibility walk of the first
  candidate (plus, for a block, listing its site), not a survey. A giant
  candidate costs a giant walk; that is the price of checking its
  contents. There is no latency target (Mark, 2026-10-25): the time to
  the first deletion is measured and reported by the scale tests, not
  promised. **Measured** on the `Host` port's monotonic clock
  (`CLOCK_MONOTONIC` on Linux) from the run's start (the first event of
  `ensure`'s executor, before the sandbox gate and the first `statvfs`)
  to the end of the first action that removed anything (its candidate's
  deletion finished, before the space is sampled), and logged once per
  run: `first deletion: <ms> ms after the run started (<candidate>)`.
  Command-line and configuration loading before the executor are not in
  it; a run that deletes nothing logs nothing. Not in the JSON report.
  A first-tier candidate is walked three times before its deletion: the
  tally's sizing (before the lock under `best-fit`; under `oldest` by
  the walk as the queue reaches it, so the first deletion waits for the
  sizing of the units found ahead of it, not of the whole tier, and for
  one root read of each unit whose key an earlier run stored), the
  candidate's own read-only tally, and the locked eligibility walk; the
  second could reuse the first (TASK-245). A signal is not checked
  inside one unit's sizing walk (under the lock under `oldest`), only
  between units, as for every walk of a candidate.
- Every step checks the cooperative deadline
  ([safety.md](safety.md#deadlines)). `watch` calls the walk with
  `min = --min-free`, `goal = --target-free` and a deadline of `--timeout`
  from the sample that triggered the cycle
  ([reference.md](reference.md#the-reclaim-cycle)); `ensure` with its two
  flags.
- `exhausted` is reported only after every tier's discovery, so an index
  that missed something can delay success but never fake exhaustion.
  `stale`: the unit was gone or replaced at re-check, or no longer matches
  its source under the tier's root. `changed`: something
  moved under a check (a Cargo lock or build appeared after the locks were
  taken, or an entry's identity changed during deletion).
- **Tiers the index does not serve.** The index serves `fs` tiers of one
  source without `!profile:` (`executor::indexed`). Cargo-target tiers,
  filesystem unions and tiers with `!profile:` are not tallied by the
  synchronous tally and take no queue (Docker tiers have nothing to
  order); for them, and for what an indexed tier's queue did not judge,
  `ensure` works as before the index: after taking the lock it discovers
  the tier's candidates and tallies each (the eligibility walk without
  the idle and in-use checks, read-only) for the tier's order, within
  `--tally-budget`; candidates the budget left untallied follow the
  tallied ones in the order discovery found them. It then acts on each in
  that order, every check made again under the candidate's own locks (a
  Cargo target's lock files, an `fs` candidate's `flock`;
  [safety.md](safety.md#deletion)), so a candidate that is not idle is
  locked and walked before it is skipped. A candidate the tally refuses
  is skipped for the run and never locked: re-judging it under its locks
  would take Cargo's locks on kept and other users' targets (or `flock`
  a kept directory) and walk oversized ones again. A refusal that
  stops being true after the tally (a `.diskgc-keep` removed) is a missed
  reclaim until the next run, never an unsafe delete. This tally runs
  under the host lock, unlike the synchronous one, so other reclaims
  wait that much longer.
- **A failure of the host during the synchronous tally** before the
  host lock (`EIO`, `EMFILE`, `ENOMEM`: an error of the run, not a skip;
  [safety.md](safety.md#the-eligibility-walk-mandatory-per-candidate),
  Fail closed), in its listing (pass 1) or its sizing of a `best-fit`
  first tier, ends the tally with one log line, and **no tier takes a
  queue from the index in that run**: each tier is its own discovery
  alone, which meets a failure that persists and stops the tier with
  nothing of it reclaimed, as without the index. Otherwise a queued unit
  could be deleted before discovery of the same root stops on another
  branch, which discovery alone never does (it acts only on a listing
  that completed). The rule is coarse on purpose (every tier, not only
  those whose roots failed): the others lose only speed. Not reported in
  the JSON beyond the run error discovery records (TASK-247).
- **A failure of the host during the deferred sizing** of an `oldest`
  first tier (its root reads or a unit's sizing, under the host lock as
  the queue reaches it) is an **error of the run**, as any failure of the
  walk's own checks: the run stops there (exit 3). It can come after
  earlier candidates of the queue were deleted (the queue sizes each
  found-ahead batch before judging it, Found ahead above), each of which
  passed every live check; it is the same kind of limit as the next one,
  and the queue rule above does not apply: no queue is taken back once
  the walk has begun.
- **Remaining limit: a failure that first appears after the tally.** A
  host failure on a branch the tally listed cleanly that first shows when
  the tier's discovery lists it after the queue (a disk going bad during
  the run) can come after queued candidates of that tier were deleted;
  each of them passed every live check, and the discovery's error still
  stops the tier and the run (exit 3). The same holds for a root the
  tally did not list because the snapshot's listing of it was recent.

### Measuring space

- **Available space** = `f_bavail × f_frsize` from `statvfs`: what an
  unprivileged writer can use, never the root reserve. No `syncfs` (it
  waits for unrelated I/O and cannot be cancelled). Some filesystems free
  blocks asynchronously, so if a sample after an action shows less than
  expected, diskgc re-samples a few times over at most 2 s.
- **`avail_delta`** is the observed net change around an action, signed:
  concurrent writers can make it negative, unrelated deletions can inflate
  it. Success is judged only by the final sample.
- **Stopping:** diskgc stops as soon as an observed sample reaches the goal.
  It does not bound eventual reclamation: if a filesystem frees blocks later
  than the re-sampling window, further candidates may already have been
  deleted by the time the space appears. Docker and Nix actions are whole
  operations and can overshoot by design; a Nix GC's `--max-freed` is a
  lower bound on Nix's own byte count, not on available space, and in an
  optimised store its space appears only at the end of the GC
  ([safety.md](safety.md#nix-store)).
- **Tallies** count allocated blocks and each inode once (Cargo hard-links
  binaries from `deps/`), and are estimates; they order work and are never
  used to decide success.

### Filesystem matching

**The same filesystem** (TASK-286) is decided one way everywhere, in
`places::space`: by the device mountinfo names for each path's mount, the
filesystem's superblock's (`space_of`). Every mount of one filesystem
shows the same one: a bind mount, and every btrfs subvolume of one btrfs
(Fedora's `/` and `/home`, openSUSE's `@` subvolumes). A `statx` `st_dev`
is never compared for this (the code keeps the two apart by type): it
identifies an object (with the inode,
[safety.md](safety.md#path-traversal)), and on btrfs it is each
subvolume's own anonymous device, never the superblock's. Where the
superblock device does not identify one live filesystem alone (bcachefs
takes it from one member device and keeps it when that member is removed
and its device reused), the filesystem id (`statfs` `f_fsid`, made from
its UUID) read from the held descriptors on both sides must agree too
(`space_at`); one that cannot be read agrees with nothing. The rule
decides:

- which roots and candidates are on `PATH`'s filesystem (`other_fs`
  otherwise): a root by the mount it resolves to, a candidate by the mount
  discovery found it on (a Cargo target, and a candidate inside an
  unmounted nested btrfs subvolume, on its root's mount); Docker by its
  bound backend's storage root; Nix by `/nix/store`, the only store it
  collects ([safety.md](safety.md#nix-store));
- whether a `watch` cycle's `PATH` is still the filesystem watched since
  start ([reference.md](reference.md#watch));
- the snapshot's key `<fs>` (Snapshot, below), so one `watch` serves every
  subvolume of one btrfs;
- which open-but-deleted files `held_by_deleted_files` lists: each placed
  by the mount it was opened through (`stx_mnt_id`), compared by device
  (a report, never a deletion: a bcachefs device shared after a member's
  reuse, or a mount id reused after a lazy unmount, can misplace one; the
  unique mount id of Linux 6.8 is not used yet); one opened through a
  mount diskgc's table does not list (another mount namespace's) counts
  only when its `st_dev` is `PATH`'s own.

**Subvolume boundaries.** A directory on another `st_dev` than the one
it is in, with no mount between, is the top of a nested (unmounted)
btrfs subvolume or snapshot, which `rmdir` deletes once it is empty
(Linux 4.18, unprivileged). diskgc never removes one: such a directory
matched as a candidate, met by the eligibility walk anywhere inside a
candidate or a Cargo profile entry, or met by the Cargo inspection inside
a target is `contains_mount` (the whole candidate is kept), as the tally
does not enter one. What lies inside one is taken like anything on its
mount. Directories only: overlayfs without `xino` shows a file of a lower
layer under that layer's device, never a directory.

Deleting needs the same filesystem proved. Writing diskgc's own state
needs the opposite proved: the snapshot is guarded (Snapshot: Never at the
cost of the goal) unless the state directory's filesystem is proved
**independent** of the reclaimed one (`write_guard`): another
superblock; neither filesystem one that may keep its data on another (an
`overlay`; `ecryptfs`, `aufs`, `shiftfs`; FUSE, network types and
`autofs`); neither stored on a loop device (`on_loop`: a loop device, a
partition of one, or a btrfs with a member on one, read through the held
directory as [safety.md](safety.md#filesystem-candidates) says under Not
protected, not excluded, Btrfs membership); and not two ZFS datasets of one pool (by
the source's pool name). Guarded, the state directory and `PATH` are both
sampled and the lower decides. A state directory on a filesystem stored
on a loop device is not written at all (one log line, warmth only): its
image may be a sparse file on `PATH`'s filesystem that a write fills by
more than its own size. So an overlay whose upper directory is on
`PATH`'s filesystem is `other_fs` for deleting and guarded for writing.
Limits: ZFS datasets of one pool are `other_fs` for each other (they
share the pool's space; reclaiming across them is not claimed); a block
device stacked on a file in another way (device-mapper or md over a loop
device, a network block device) counts as independent.

A tier whose backend is elsewhere is skipped and reported (`other_fs`,
`remote_backend`): deleting there would hurt a cache and free nothing here.

## Hysteresis

| Where | Mechanism |
|---|---|
| `watch` | `--min-free` triggers, `--target-free` (required) is the goal. |
| `ensure` | Same pair; `--target-free` defaults to `--min-free` (no surprise deletions); CI hooks set it higher. |
| Candidate eligibility | `idle-*` filters: recently used caches are not eligible. |
| Nothing left to free | `watch` back-off until space drops further or 15 minutes pass; sooner after environment errors ([reference.md](reference.md#the-reclaim-cycle)). |
| Within a reclaim | Stop as soon as the goal is observed. |

Per-tier cooldowns are not included: the snapshot could carry them, but it
is a cache that may be missing, and a cooldown that silently disappears is
worse than none. The cost (a no-op Nix GC per CI job when nothing is dead)
is accepted until measured otherwise.

## Implementation

- **Rust everywhere**: the product, the test harness, the host-model
  generator, test helpers. No shell scripts in product or tests; `just`
  recipes only call Nix, Cargo and Podman.
- **Nix flake** is the single development entry point:
  - build with **crane** (one shared dependency build for package, clippy
    and tests), with source filtering so that editing docs rebuilds nothing;
  - product toolchain from **rust-overlay**, pinned by `rust-toolchain.toml`;
    the binary is statically linked (musl) so it runs on any distribution;
  - *Cargo under test* is separate: pinned toolchains from the same locked
    rust-overlay, in the test image ([testing.md](testing.md#cargo-toolchains-under-test));
  - outputs: `packages.default` (the development package),
    `nixosModules.default` (NixOS module, below), `devShells`, `checks`
    and the rest are listed, each with its comment, in `flake.nix`;
    [testing.md](testing.md) says which recipe builds which check.
- **Cargo workspace:**

  The product workspace and the separate `diskgc-e2e` workspace both set
  `overflow-checks = true` in their release profiles. The flake builds the
  shipped binary and runs its Rust tests with release optimizations; checked
  arithmetic there must panic on overflow rather than silently wrap while
  deciding whether to delete. This can cost some throughput, which the
  container scale tests measure on the same release builds.

| Crate | Role |
|---|---|
| `diskgc-core` | Config schema and grammar; the index types and queue derivation; the **planner** (pure: index in, plan, skip reasons, exit-code precedence and report out); the **tally** and the **executor** (re-check → walk → checks → act → re-sample), both over the `Host` port; the `Host` trait. No syscalls of its own. |
| `diskgc-linux` | `LinuxHost`, the real `Host`. A library, because the contract tests use it. |
| `diskgc-model` | `ModelHost`, a `Host` backed by a YAML host model, and the model-test harness. Test-only. |
| `diskgc-modelgen` | Generates host-model values (fixtures and `proptest` strategies). Test-only. |
| `diskgc-e2e` | Destructive tests that need a real kernel, their harness and the host-side runner. Excluded from the workspace; built by the flake only. See [testing.md](testing.md). |
| `diskgc` | The binary: CLI and output; installs signal delivery into `LinuxHost`; wires `diskgc-core` to `LinuxHost`. |

### The `Host` port

- **Two shapes of operation.** Files and locks are syscall-shaped
  (`openat2`, `statx`, `getdents` (names only), `flock`, `unlinkat`,
  `statvfs`, `mountinfo`, `/proc` reads), so the safety logic (flags,
  ordering, rechecks) lives in the core where the model can test it. Docker and Nix are domain-shaped, in two
  operations. A **query** run (`docker context`/`builder`/`info`, `nix
  store info --json`) is on `Host`: not gated, so `survey`, `--dry-run` and
  `--tally-only` work anywhere; it takes a closed enum of queries that
  `LinuxHost` maps to argv, never argv from the caller or from config. A
  **deleting** run (a prune, a GC, every command action) is
  `run(argv, env, deadline)` on `Delete`, behind the pre-release sandbox
  gate. Each has one wrapper in `LinuxHost`, allowlisted by the source scan
  ([safety.md](safety.md#pre-release-sandbox-gate)).
- **What the in-use check reads** ([safety.md](safety.md#in-use)): the
  `/proc` readers; per process its `status`, `uid_map` and the owner of a
  live task's `fd` directory (`proc_status`, readable for every process);
  diskgc's own pid; and two socket-diagnostics dumps of the Unix sockets
  bound in diskgc's network namespace (`bound_unix_sockets`, a read-only
  `NETLINK_SOCK_DIAG` query: ungated, like the query runs); the PID
  namespace facts (`pid_namespace`: the initial namespace, `hidepid`) and
  whether this is the e2e harness's build in a marked test container
  (`test_container_pid_namespace`, the visibility rule's one test-only
  exception); and the configured loop devices with their backing files
  (`loop_backing_files`, `/sys/block/loop<N>`, read-only) and each held
  file's identity by the device's own word (`loop_backing_identity`:
  `/dev/loop<N>` opened read-only for `LOOP_GET_STATUS64`, where diskgc's
  user may open it). Reading a list
  of processes is a provided method (`proc_views`) that calls the readers
  one process after another, so `ModelHost` sees every reader call;
  `LinuxHost` reads several processes at once, each on its own, and the
  test driver's wrapper passes the call through to it unless a pause or
  kill point names a reader.
- **diskgc's own state** (the index snapshot) is written through one port
  operation, `state_write`, that takes only a typed key (`FsKey`; no fd
  or name from the caller) and the held single-writer lock: `LinuxHost`
  opens the state directory itself (given to it by the binary at
  construction), creates the temporary file with `O_TMPFILE` (mode 0600)
  and links it in as `index-<fs>.new` with `linkat`, never `O_TRUNC`, and
  renames only onto `index-<fs>`; the lock must be that key's lock file
  (same object) held exclusively, which `ModelHost` checks (a test fails
  otherwise). When its caller's `cancel` says so (`watch`'s, after a
  second signal, [reference.md](reference.md#watch-signals); `ensure`'s
  never) it does not link the file in and reports the write cancelled.
  The port counts signals the same for every command
  (`second_signal`); what a second one does is core's policy, passed in:
  this `cancel`, and a command's `second_signal` (leave a child running
  or wait out its grace). It is not gated (it is diskgc's cache, not user data, and
  `watch --tally-only` must work anywhere). The single-writer lock file
  is a second typed operation beside it, `state_lock`: `LinuxHost` opens
  (creating if absent, with the state directory, mode 0700)
  `index-<fs>.lock` beneath the state-directory fd,
  `O_RDWR|O_CREAT|O_NOFOLLOW|O_CLOEXEC` (never `O_TRUNC`), mode 0600, for
  `flock` only: an NFS client emulates `flock` with byte-range locks, and
  an exclusive one needs a descriptor open for writing (TASK-288; where
  the user may not write the file, `EACCES` or `EROFS`, `O_RDONLY` as
  before, which a local `flock` needs no more than); nothing is ever
  written to it or renamed onto it. Each of the two is
  its own ungated entry in the source scan's allowlist
  (`diskgc-linux/src/state.rs`, with its reason and exact match count;
  the open for writing, `open_for_flock`, is a third);
  the per-user reclaim lock is a third, `user_lock` (what it checks and
  makes: [safety.md](safety.md#host-lock)): it takes no name, only
  whether it may make what is missing; `LinuxHost` finds the place
  itself (`user_lock_path`, read-only: `/run/user/<uid>` and the account
  database, whose failed lookup is its error), and `ModelHost` has the
  same operations, refusals and errnos included. With the `mkdir -p` the state directory and that
  lock's parent share (`make_dirs_ungated`), these are the only
  writes, creations or renames the scan allows outside
  `Delete`. Read-only beside them: `state_read` (the snapshot, never
  through a symlink, with its owner and mode), `fs_uuid`, `boot_id` and
  `loop_backing_file` (`/sys/dev/block/7:N/loop/backing_file`, for the
  `protect` verdict on loop images).
- **Time is part of the port.** The clock, every blocking operation (host
  lock wait, child wait, the SIGTERM → 10 s → SIGKILL sequence, re-sampling)
  and the tally's batch timing and sleeps are port operations; `ModelHost`
  keeps a virtual clock and delivers signals as events, so deadlines, exit
  codes 75 / 124 / 130 / 143 and the duty cycle are model-testable. Two
  clocks: `now` (monotonic, including suspend) for deadlines and freshness,
  `wall_clock` (realtime, the scale of file times) only for comparing with
  file times in the idle filters.
- **File contents are not read**, with one exception: `read_head` reads the
  first bytes of a small marker file (a `CACHEDIR.TAG`'s signature) through
  the descriptor of the object checked, never by name again; anything but
  a regular file is refused on that descriptor's type before it is
  opened for reading (a FIFO or a device is never opened). diskgc's own
  configuration file is read by the binary, outside the port
  (`LinuxHost::read_config_file`, the same type check first, at most
  1 MiB): it may be a symlink, and only its content is validated; parsing
  it is pure.
- **Locking a file checked by identity** (Cargo's lock files):
  `reopen_regular` opens again, read-only and non-blocking, the regular
  file an `O_PATH` fd refers to, through that fd (never by name), refusing
  anything else by the fd's own type before opening it, since `flock`
  refuses an `O_PATH` fd ([safety.md](safety.md#reclaim-protocol)).
- **Deleting operations are a trait of their own** (`Delete: Host`:
  `unlinkat`, `fchmod` for `make_writable`, later the deleting `run`, and
  `may_delete`, the gate's decision asked before a reclaim starts), so
  code given only a `Host` (the dry run, survey, tally) cannot delete or
  change permissions, and the pre-release sandbox gate guards exactly
  those methods.
- **Who diskgc is, and what its children keep:** `effective_uid` (the
  ordinary-mode owner every entry must have, and what `{uid}` stands for;
  root mode uses the resolved `owners:` IDs for entry ownership),
  `account_uid` (an `owners:` name's uid in the account database, through
  the name service; what uids are acceptable is the composition's policy)
  and `inherit_lock`, which makes a held lock inheritable by children by
  clearing close-on-exec on that one fd (the host lock,
  [safety.md](safety.md#host-lock)).
- **Listings are bounded**: `read_dir` takes a limit and stops past it
  (the deletion engine passes what is left of a candidate's entry budget),
  so a huge directory, or one filled faster than it is read, cannot exhaust
  memory or keep a listing going.
- **`ModelHost` checks how it is called**: an `openat2` without the required
  `RESOLVE_*` flags, a `statx` of more than one name or from the root, or
  an `unlinkat` without an identity check right before it fails the test
  ([testing.md](testing.md#2-model-tests-a-yaml-host-model-instead-of-a-real-disk)).
  Releasing or re-locking a held lock cannot be expressed:
  the lock calls take the fd by value and return a `Locked` handle that
  lends the fd out by reference only and closes it when dropped, and there
  is no unlock operation; the model keeps each lock exactly as long as an
  fd refers to its open file description.
- **statx fields are optional in the port**: a field the kernel did not
  mark as answered (`stx_mask`, `stx_attributes_mask`) is absent, so the
  core cannot use it by accident and `ModelHost` can model a filesystem
  that does not answer.

## Deployment

- One statically linked Rust binary, Linux 5.8 or newer, any distribution.
  Nix, Docker and Cargo are optional. It needs the `/proc` of its own pid
  namespace and `pidfd_open` (not blocked by seccomp), and refuses to start
  otherwise ([safety.md](safety.md#in-use), unreadable or vanishing
  processes). To reclaim (and for a dry run) it also needs the initial
  PID namespace and a `/proc` without `hidepid`, so a reclaiming
  `ensure` or `watch` runs on the host, not in a container or a unit
  with `PrivatePIDs=` or `ProtectProc=`; in a container, `ensure` only
  confirms space that is already there
  ([safety.md](safety.md#in-use), Visibility: a process list that cannot be
  complete). Its in-use scan also needs a hard `RLIMIT_NOFILE` above the
  number of processes it can see; it raises its soft limit itself
  ([reference.md](reference.md#ensure), Open-file limit).
- **Generic Linux:** the binary, the two `watch` units below, the tmpfiles
  drop-in for the host lock file, and an example config. Distribution
  packages can follow. The drop-in is needed for root and for the system
  template; a user's `ensure` and the user template work without it,
  each user's reclaims then taking turns through a per-user lock only
  ([safety.md](safety.md#host-lock)).
- **NixOS**, as one option: the module below. It renders the YAML,
  provisions the host lock, runs `check` at build time, and
  starts the `watch` instances.
- **CI runners:** binary, YAML, the tmpfiles drop-in (or equivalent: it
  serialises the runner's reclaims with every other user's, and the
  system template requires it), and
  `diskgc ensure --min-free 50G --target-free 80G "$CI_BUILDS_DIR"` in the
  pre-job hook; optionally a `watch --tally-only` instance between jobs for
  a warm index (below).

Designed by TASK-119; Mark's decisions (2026-10-23, 2026-10-25) are the
user service by default, the system template as the option, both in the
module, and `Restart=always` with `RestartPreventExitStatus=2`. The rest
is listed under Choices to confirm. TASK-44 ships the units, TASK-45 the
module; their tests are in [testing.md](testing.md#deployment-units).

### Release procedure

The version in `Cargo.toml` (`[workspace.package]`) says what a build is,
and `diskgc --version` follows it
([reference.md](reference.md), `diskgc --version`):

- **Master** carries the next minor release with a pre-release suffix
  (`0.2.0-dev`). Every build of it names its commit.
- **A release** drops the suffix in exactly one commit, the one tagged
  `v$VERSION` (annotated). Its `release-candidate` package is then the
  release; it prints the version alone, and its shipped binary is its
  tested one (no stamp). `just release-verify` fails on a plain version
  whose `HEAD` is not that tag or whose tree is dirty, and passes a
  development version.
- **The next commit** on that release branch bumps to `X.Y.(Z+1)-dev`, so
  a later build from the branch (a backport) is never mistaken for the
  tag.
- **What a release runs and ships** is its row of the release descriptor,
  `crates/diskgc-core/src/releases.json` (its commands, the builtin.yaml
  `releases:` label its catalog narrows to, its package's files); a
  version picks its row by `MAJOR.MINOR`, in the binary and in the flake.
  Moving master to a new minor adds that row: the flake does not evaluate
  a release candidate of a version without one, and a plain `cargo build
  --features release-catalog` of such a version builds but exits 2 on
  every command but `--version`, naming the missing row. The tagging
  commit sets the row's
  `tag`, and a tagged row is never edited again (a test pins 0.1's to
  what v0.1.0 shipped).

### The `watch` units

A **user service by default**: the laptop owner runs `watch` under
`systemd --user`, as themselves, so it sees their processes and never
needs root, not even to provision the host lock: without it the user's
reclaims take their per-user lock ([safety.md](safety.md#host-lock)). A **system template** is the option for CI runners and
`--allow-root`. Both are templates with one instance per watched
filesystem and carry the same name, so the commands read alike:

| Package file (source in `packaging/systemd/`) | Manager | Start one |
|---|---|---|
| `lib/systemd/user/diskgc-watch@.service` | `systemd --user` | `systemctl --user enable --now "diskgc-watch@$(systemd-escape --path /home).service"` |
| `lib/systemd/system/diskgc-watch@.service` | system | `systemctl enable --now "diskgc-watch@$(systemd-escape --path /srv/ci).service"`, after a `User=` drop-in |

Install by copying the user file to `/etc/systemd/user/` (every user) or
`~/.config/systemd/user/`, the system file to `/etc/systemd/system/`
(the steps: [reference.md](reference.md#watch-as-a-systemd-service)).

**The instance is `PATH`.** The instance name is `PATH` escaped by
`systemd-escape --path` (`/` is `-`, `/home` is `home`, `/srv/ci-builds`
is `srv-ci\x2dbuilds`), and `%f` turns it back into `PATH`. One watcher
serves a filesystem ([reference.md](reference.md#watch), One process per
filesystem), so start one instance per filesystem: any `PATH` on it
selects it, and the mount point reads best. The thresholds and
every other flag come from the instance's environment file
`%E/diskgc/watch/%i.env` (`~/.config/diskgc/watch/home.env` for the user
manager, `/etc/diskgc/watch/srv-ci\x2dbuilds.env` for the system one),
which holds one variable:

```
DISKGC_WATCH_ARGS=--min-free 30G --target-free 60G
```

The value goes through two stages (systemd.exec `EnvironmentFile=`,
systemd.service Command lines; read in the systemd 260 manual, not yet
run under a live manager here):

1. **The file.** An unquoted value keeps its interior whitespace and every
   quote after its first non-whitespace character, so the quotes of
   `--select '...'` reach the variable as written. A value that *starts*
   with a quote is a quoted value instead: its quotes are removed here,
   so the line must start with a flag, not a quote.
2. **`ExecStart=`'s `$DISKGC_WATCH_ARGS`.** Written as a word of its own,
   it is split at whitespace with quotes respected, then the quotes are
   removed: one argument per word, one for each quoted string.

So a `--select` expression with spaces survives when it is quoted:

```
DISKGC_WATCH_ARGS=--min-free 30G --target-free 60G --select 'profile:cargo-target && filter:idle-7d' --roots /home/me/src
```

gives `watch` the arguments `--min-free`, `30G`, `--target-free`,
`60G`, `--select`, `profile:cargo-target && filter:idle-7d`, `--roots`,
`/home/me/src`. Selections are still easier to keep in the
configuration's tiers; the NixOS module replaces `ExecStart=` in a drop-in
instead. The file is optional (`-`): without it `watch` has no
thresholds, exits 2 naming `--min-free`, and the unit stays failed with
diskgc's message instead of looping.

The user unit, in full:

```ini
[Unit]
Description=diskgc watch of %f

[Service]
Type=exec
EnvironmentFile=-%E/diskgc/watch/%i.env
ExecStart=/usr/local/bin/diskgc watch $DISKGC_WATCH_ARGS "%f"
Restart=always
RestartSec=30s
RestartPreventExitStatus=2
KillMode=mixed
TimeoutStopSec=30s
NoNewPrivileges=yes
LockPersonality=yes
RestrictRealtime=yes
RestrictNamespaces=yes
RestrictSUIDSGID=no
RestrictAddressFamilies=AF_UNIX AF_NETLINK
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM

[Install]
WantedBy=default.target
```

- **`ExecStart`** names the README's install path; the NixOS module
  replaces the line. `Type=exec` makes a missing binary a failed start.
- **Restart** (Mark, 2026-10-25). `watch` exits 0 on SIGTERM, SIGINT and
  SIGHUP and reads its configuration only at start, so anything that
  stops it outside `systemctl stop` (a `kill -HUP` after editing the
  configuration) restarts it with the configuration read afresh. Exit 2
  (usage, configuration, privilege, the sandbox gate, a missing host lock
  file as root or an unusable one, an unsafe per-user lock, a missing
  root, another watcher holding the filesystem's
  single-writer lock: [reference.md](reference.md#exit-codes-watch)) is
  not restarted: the unit stays failed and `systemctl status` shows why.
  Exit 3 and a panic (101) restart after 30 s; at one start per 30 s the
  default start limit (5 in 10 s) never trips, so a fault that persists
  costs one journal entry per 30 s. There is no `ExecReload=`: diskgc
  does not reload, and `systemctl restart` does what SIGHUP does.
- **Stop.** `KillMode=mixed` sends SIGTERM to diskgc alone, so its own
  stop runs (a child gets SIGTERM and SIGKILL after 10 s, then the
  snapshot is written; [reference.md](reference.md#watch-signals)). When
  diskgc has exited, systemd SIGKILLs whatever is left in the unit's
  cgroup, and at `TimeoutStopSec=30s` it SIGKILLs everything; both are
  safe (deletions are crash-safe, a killed child releases the host lock).
  The default `control-group` would SIGTERM the children alongside
  diskgc.
- **Configuration and state.** The user unit sets neither: the
  configuration is the usual lookup (`~/.config/diskgc/config.yaml`, then
  `/etc/diskgc/config.yaml`) and the state directory
  `$XDG_STATE_HOME/diskgc` (`~/.local/state/diskgc`), the same ones the
  user's agents' `ensure` uses, so the watcher's snapshot warms them.
- **Priority.** No `Nice=`, `IOSchedulingClass=`, `CPUSchedulingPolicy=`,
  `CPUWeight=` or `IOWeight=`. The background tally lowers only its own
  thread (`nice 19`, idle I/O class; The tally); the reclaim runs on the
  main thread at the unit's priority and must not inherit idle priority
  at the moment the disk is full and builds compete for it.
- **`LimitNOFILE=`** is left at systemd's default hard limit (524288),
  above the in-use scan's one descriptor per visible process.
- **Lingering.** A user manager runs only while its user has a session;
  `loginctl enable-linger USER` keeps it (and the watcher) running for
  overnight agents after logout.

The system template is the same file with the `[Unit]` and `[Service]`
lines below added and its `[Install]` section replaced:

```ini
[Unit]
RequiresMountsFor=%f
AssertPathExists=/run/diskgc/lock

[Service]
Environment=DISKGC_CONFIG=/etc/diskgc/config.yaml
CapabilityBoundingSet=

[Install]
WantedBy=multi-user.target
```

- **The host lock is required at start** (`AssertPathExists=`): a system
  instance does not start without it, whatever its `User=`, so the
  instances of a fleet and its pre-job hooks take turns through the host
  lock. Without the tmpfiles drop-in the start fails (`systemctl status`
  shows the failed assertion; diskgc does not run), `--tally-only`
  instances included. The assertion is checked only at start: a host
  lock removed later makes a running non-root instance fall back to its
  `User=`'s per-user lock from its next cycle on
  ([safety.md](safety.md#host-lock)). A path, not the file's type:
  something other than a regular file there passes the assertion and is
  diskgc's exit 2 (`provisioning`), as for root.

- **`User=`** is not in the template: each instance gets a drop-in
  (`systemctl edit diskgc-watch@srv-ci.service`: `[Service]`,
  `User=gitlab-runner`). Its supplementary groups (such as `docker`)
  apply. An instance without one runs as root without capabilities and
  exits 2 (root without `--allow-root`), so a forgotten `User=` is a
  failed unit that says so, never a root watcher.
- **Root.** `--allow-root` in `DISKGC_WATCH_ARGS`, an `owners:` list in
  the configuration ([reference.md](reference.md#configuration-file)),
  and a drop-in with `CapabilityBoundingSet=CAP_DAC_OVERRIDE CAP_FOWNER
  CAP_SYS_PTRACE CAP_CHECKPOINT_RESTORE`: read and delete in the owners'
  trees (`CAP_DAC_OVERRIDE`, which also covers reading and searching;
  `CAP_FOWNER` for the sticky `/tmp` and `make_writable`), read the
  owners' `/proc/<pid>` entries and `kcmp` them (`CAP_SYS_PTRACE`), read
  `map_files` links (`CAP_CHECKPOINT_RESTORE`). `CAP_DAC_READ_SEARCH` is
  not needed: the snapshot write links its `O_TMPFILE` through
  `/proc/self/fd` with `AT_SYMLINK_FOLLOW`, not `AT_EMPTY_PATH`.
  `CAP_SYS_PTRACE` is mandatory ([safety.md](safety.md#privileges)).
  The others fail safe
  when missing (an unreadable tree is refused, an unreadable `map_files`
  link retains); `CAP_CHECKPOINT_RESTORE` does not exist before Linux
  5.9; there diskgc cannot hold it, and `map_files` links stay
  unreadable, which fails safe the same way. `--allow-root`
  also requires the configuration file and every directory above it to
  be root-owned and not group- or world-writable
  ([safety.md](safety.md#privileges)): `/etc/diskgc/config.yaml`
  qualifies; a `/nix/store` path does not (the store is writable by
  group `nixbld`).
- **Configuration and state.** `DISKGC_CONFIG` pins one file whatever the
  `User=`'s home holds; an environment file may set another. The state
  directory is not overridden: it is `~/.local/state/diskgc` of `User=`,
  the one a pre-job `ensure` run by the same user reads. A user without a
  writable home gets a watcher without a snapshot
  ([reference.md](reference.md#watch-start), step 6) unless its
  environment file sets `XDG_STATE_HOME` to what the hook uses. No
  `StateDirectory=`: under `/var/lib` it would split the watcher's
  snapshot from the hook's, and instances of different users would fight
  over its owner.
- **Ordering.** Default dependencies already order the unit after
  `sysinit.target` and so after `systemd-tmpfiles-setup.service`, which
  creates the host lock from the package's `lib/tmpfiles.d/diskgc.conf`
  ([safety.md](safety.md#host-lock)) before the system template's
  assertion is checked, and `sysinit.target` is itself
  after `local-fs.target`, so an `After=local-fs.target` would add
  nothing. `RequiresMountsFor=%f` waits for `PATH`'s own mount. Nothing
  orders it after `docker.service` or the Nix daemon: a backend that is
  not up yet is a `retry` back-off
  ([reference.md](reference.md#the-reclaim-cycle)).

### Hardening

The rule: **no option that gives diskgc a namespace of its own** (mount,
user, PID, network). diskgc must see the user's processes and files as
they are; what protects them is the code ([safety.md](safety.md)), not a
narrowed view. In a user manager, **any option that implies
`PrivateUsers=`** is fatal (below): every mount-namespace option, and
every option that edits the capability bounding set (systemd.exec,
Sandboxing and Capabilities), so only seccomp-based options remain
there; they are the shipped list in both units. Checked against
`systemd-analyze syscall-filter` and systemd.exec of systemd 260.
Operator-added `ProtectProc=` or `PrivatePIDs=` is **refused**: a
reclaiming `watch` exits 2 at start naming the rule, so the unit fails
where its operator looks instead of running blind
([safety.md](safety.md#in-use), Visibility: a process list that cannot be complete;
TASK-263).

| Option | Verdict | Why, by what diskgc reads and writes |
|---|---|---|
| `ProtectProc=` other than `default` | no: refused at start (exit 2, TASK-263) | mounts the unit's `/proc` with `hidepid`, which hides the user's own processes the kernel will not let diskgc read, a shell started with `newgrp` or `sg` and its children among them (for root, those an LSM keeps from it). Visible, such a process makes candidates `in_use_unknown`; hidden, its open files would go unseen, so diskgc refuses ([safety.md](safety.md#in-use)). System units only. |
| `ProcSubset=pid` | no | hides `/proc/sys/kernel/random/boot_id` (snapshot validity) and `/proc/pressure` (the pressure brake goes off and the tally loads the system). |
| `PrivateTmp=` | fatal | an empty private `/tmp`: every `/tmp` candidate (`pytest-tmp`, `nix-shell-rc`) is invisible to discovery and walks. |
| `ProtectHome=` | no | hides or freezes `/home`, `/root`, `/run/user`: Cargo roots, the user's configuration and the state directory vanish, or deleting and the snapshot write fail `EROFS`. |
| `ProtectSystem=strict` | no | every candidate tree read-only, deletions fail `EROFS`. Reopening them with `ReadWritePaths=` would copy the configuration's roots into the unit (two sources of truth) and make each a mount point in diskgc's own mount table. |
| `ProtectSystem=yes` or `full` | works, not shipped | no candidate lies under `/usr`, `/boot`, `/efi` or `/etc`, and the configuration is only read. It costs a mount namespace: diskgc's `mountinfo` then differs from every judged process's, so held paths always take the other-namespace translation ([safety.md](safety.md#producer-liveness), A process's paths). The gain is small: a non-root watcher cannot write there, and a root one refuses root-owned entries. |
| `ReadWritePaths=` | not used | meaningful only with `ProtectSystem=strict` (above). |
| `PrivateDevices=` | works, not shipped | hides `/dev/disk/by-uuid`, so the snapshot key falls back to the device number (warmth only); a mount namespace. |
| `ProtectKernelTunables=`, `ProtectKernelModules=`, `ProtectKernelLogs=`, `ProtectControlGroups=`, `ProtectHostname=`, `PrivateMounts=`, `PrivateIPC=` | not shipped | harmless to what diskgc reads (`/proc/sys` stays readable), but each needs a namespace: the rule, and in a user manager a user namespace. |
| `PrivateUsers=` (any value) | fatal, silently | in a child user namespace the kernel refuses the ptrace-read access to `/proc/<pid>/fd`, `maps`, `environ`, `cwd` of every process of the initial namespace, the user's own included (a reader in another user namespace needs `CAP_SYS_PTRACE` in the target's), and the "more capable" excuse is off there ([safety.md](safety.md#in-use)): every filesystem and Cargo candidate is `in_use_unknown`. Nothing is deleted wrongly, and only the daemon-side kinds (Docker, Nix) still reclaim. |
| `PrivatePIDs=` | no: refused at start (exit 2, TASK-263) | a new PID namespace with its own `/proc`: the in-use scan would see only the unit's processes, so every other process's open files, maps and working directories would go unseen; diskgc refuses ([safety.md](safety.md#in-use)). |
| `PrivateNetwork=` | no (unsafe under `ipc: live-only`) | `bound_unix_sockets` dumps the Unix sockets bound in diskgc's network namespace; a private one is empty, so a socket another process has bound looks stale. Daemon sockets would still connect (path sockets cross network namespaces). |
| `RemoveIPC=` | no | at stop it removes every System V and POSIX IPC object of `User=` (its `/dev/shm` segments, semaphores, message queues), the user's own programs' included. System units only. |
| `ProtectClock=` | system: not shipped; user: fatal | it edits the capability bounding set, which in a user manager implies `PrivateUsers=`. |
| `DynamicUser=` | fatal | implies `PrivateTmp=`, `ProtectSystem=strict` and `ProtectHome=read-only`, and its uid owns no candidate. |
| `NoNewPrivileges=yes` | shipped | diskgc never gains privileges; it also keeps a command action from becoming root through `sudo` or a setuid helper. |
| `SystemCallFilter=@system-service`, `SystemCallErrorNumber=EPERM`, `SystemCallArchitectures=native` | shipped | the group holds what diskgc calls: `pidfd_open` and `pidfd_send_signal` (`@process`), `openat2` and `statx` (`@file-system`), `setpriority` and `ioprio_set` (`@resources`), `prlimit64` (`@default`), `kcmp` and `flock` (listed in `@system-service` itself), `socket` for `NETLINK_SOCK_DIAG` (`@network-io`). Not with the common `~@resources`: the tally then runs at normal priority (one log line). Any filter without `pidfd_open` makes diskgc refuse to start, as under Podman 2.1.1's profile ([safety.md](safety.md#in-use)). Children (the Docker CLI, Nix, command actions) run under the same filter. |
| `RestrictNamespaces=yes` | shipped | the source scan forbids `setns` and `unshare`; a command action that needs namespaces (`bwrap`) fails loudly. |
| `RestrictAddressFamilies=AF_UNIX AF_NETLINK` | shipped | `NETLINK_SOCK_DIAG`; the Docker and Nix daemon sockets; nscd. A command action that needs the network fails loudly. |
| `LockPersonality=yes`, `RestrictRealtime=yes` | shipped | diskgc uses neither. |
| `RestrictSUIDSGID=` | shipped as `no` | set explicitly, because systemd 258's `DefaultRestrictSUIDSGID=` can turn it on for units that do not say. `make_writable` passes the directory's mode with the owner-write bit added (`st_mode & 0o7777 \| 0o200`), set-group-ID bit included, so its `fchmod` is denied on a read-only setgid directory and that candidate's deletion fails. |
| `MemoryDenyWriteExecute=yes` | works, not shipped | diskgc does not need writable executable memory, but children inherit the flag, and a command action on a JIT runtime (Node, a JVM) crashes. |
| `CapabilityBoundingSet=` | system: shipped empty | a non-root watcher needs no capability; root's set is above. In a user unit it would need a user namespace. |
| `Nice=`, `IOSchedulingClass=`, `CPUSchedulingPolicy=`, `CPUWeight=`, `IOWeight=` | not set | they apply to the reclaim too (Priority, above). |
| `LimitNOFILE=` | not lowered | a limit below the visible process count fails the in-use scan. |

### NixOS module

`nixosModules.default` (source `packaging/nixos/module.nix`), under
`services.diskgc`:

| Option | Type | Default | Meaning |
|---|---|---|---|
| `enable` | bool | `false` | the package on `PATH` (agents and hooks call `ensure`), its tmpfiles rule for the host lock (`systemd.tmpfiles.packages`) and both templates (`systemd.packages`) |
| `package` | package | the flake's `packages.default` | a reclaiming instance needs a release package whose release runs a reclaiming `watch` (none does yet; asserted, below): a development package refuses to delete outside test containers, so the instance exits 2 naming the sandbox gate and stays failed. `tallyOnly` instances work with the development package, and the module accepts `packages.release-candidate` too; a candidate built from master is a development version for disposable test environments only, not for deployment (PRD, Safety boundaries). |
| `settings` | null or YAML value | `null` | rendered to `/etc/diskgc/config.yaml` and checked at build time |
| `instances` | attribute set of instances | `{}` | one `watch` each |

`instances.<name>`:

| Option | Type | Default | Meaning |
|---|---|---|---|
| `scope` | `"user"` or `"system"` | `"user"` | which manager runs it |
| `user` | account name | required | an account in `users.users`: user scope, the one whose manager runs it (`ConditionUser=`); system scope, `User=`. A literal name (`[A-Za-z_][A-Za-z0-9_.-]*`, optionally ending in `$`): no UID, no special value such as `@system`, no specifier, whitespace or control character, so it cannot add a directive or mean root by being empty |
| `path` | absolute path, as a string | required | `PATH`; names the unit |
| `minFree` | null or string | `null` | `--min-free` when set |
| `targetFree` | null or string | `null` | `--target-free` when set |
| `tallyOnly` | bool | `false` | `--tally-only` |
| `extraArgs` | list of strings | `[]` | every other flag (`--interval`, `--timeout`, `--select`, `--roots`, `--json`), passed as given; diskgc validates them. No mode flag (`--tally-only`, `--min-free`, `--target-free`, `--dry-run`): asserted, below |
| `allowRoot` | bool | `false` | `--allow-root` and root's capability set; requires `scope = "system"` and `user = "root"` |
| `settings` | null or YAML value | `null` | this instance's own configuration (an `allowRoot` instance needs `owners:`), rendered to `/etc/diskgc/watch-<name>.yaml` and checked at build time |
| `configFile` | null or absolute path, as a string | `null` | a file the module does not manage or check; a string, so it is never copied into the store |

- **Instances to units.** Each instance is the packaged template's
  instance `diskgc-watch@${utils.escapeSystemdPath path}.service` in its
  scope, with a module drop-in (`overrideStrategy = "asDropin"`) that
  resets `ExecStart=` to the full command line (quoted by
  `utils.escapeSystemdExecArgs`, so a `--select` with spaces survives)
  and resets `EnvironmentFile=` to nothing, so a stale `.env` file left
  from a generic-Linux install cannot inject flags. `--config` is
  `configFile`, else the instance's `settings` file; otherwise a system
  instance gets `/etc/diskgc/config.yaml`, which then is the module's
  checked `settings` (asserted: a hand-placed file there is used only
  when named as `configFile`), and a user instance no `--config` at all,
  so the watcher and the user's agents find the same
  file by the usual lookup (`~/.config/diskgc/config.yaml`, then the
  module's `/etc/diskgc/config.yaml`) and share the snapshot's warmth.
  The drop-in also adds `User=` (system) or `ConditionUser=` (user),
  root's capability set for `allowRoot`, and `wantedBy`
  `multi-user.target` or `default.target`. The module writes no
  environment files. NixOS gives every service a `PATH` of coreutils and
  a few tools; a user instance's drop-in drops it, so the watcher runs
  with its user manager's `PATH` (the session's) and finds `docker`,
  `nix` and what command actions call as the user's agents do. A system
  instance gets the system profile (`/run/current-system/sw`) on its
  `PATH` for the same tools. Restart and hardening lines come from the
  packaged template, so generic Linux and NixOS share one unit text. A
  system instance restarts on `nixos-rebuild switch` when the checked
  file it reads changes (`restartTriggers`: the global one only for an
  instance that reads it); a user instance picks a change up at its
  next restart (of the service, or of the user's manager).
- **Switching** (measured in `vm-nixos-module`, nixpkgs 26.11). A switch
  starts new system instances after it has re-run tmpfiles, so the host
  lock exists at their start; it reloads every running user manager (a
  lingering or logged-in user's), which starts that user's new
  instances. That reload may come before tmpfiles: on the first switch
  a reclaiming user instance may then take its per-user lock for a cycle
  ([safety.md](safety.md#host-lock): which lock is decided at each
  use). An instance diskgc refuses at start (exit 2) stays failed, and
  may make the switch exit 4 naming it: whether the switch sees the
  failure is a race (`Type=exec`, and the switch's short wait for units
  to settle).
- **Assertions** (evaluation fails, naming the instance), only where
  the module shapes the unit or the package cannot run it; other flag
  combinations are diskgc's to refuse (exit 2, and the unit stays
  failed). With any instance, the package says (its
  `passthru.diskgcBuild`, its row of the release descriptor, which the
  flake's packages carry; [reference.md](reference.md#built-in-profile-library))
  that it ships both templates, which the drop-ins extend (TASK-289: the
  release candidate shipped none); each instance's command (`watch
  --tally-only` for a `tallyOnly` instance, else `watch`) is among the
  package's commands; and `extraArgs` holds no mode flag (`--tally-only`,
  `--min-free`, `--target-free`, `--dry-run`, also as `--flag=value`), so
  the mode comes only from the typed options (in `extraArgs` one could
  hide as another flag's value, `--config --tally-only`, and diskgc would
  read a reclaiming `watch`). The package's claim is not a proof: an
  `overrideAttrs` that swaps the source keeps the old
  `passthru.diskgcBuild`, and the binary then refuses what its own row
  does not run (exit 2, the unit stays failed). Then:
  one instance per unit name in each
  scope (user units live in `/etc/systemd/user` for every user, so two
  users cannot both have a user instance of one `path`); `allowRoot`
  only with `scope = "system"` and `user = "root"`; at most one of
  `settings` and `configFile` per instance; `user` is in `users.users`
  (either scope: a system-scope typo would otherwise restart forever on
  systemd's exit 217); a system instance has `configFile`, its own
  `settings` or the global `settings`. **Warning** (not
  an error): a user-scope `user` whose `users.users.<user>.linger` is not
  `true`, whose watcher then stops at logout (`null` warns too: a
  `loginctl enable-linger` is invisible to evaluation).
- **Build-time check** (TASK-45). Each rendered file is the output of a
  derivation that runs `diskgc check --config FILE` with the module's
  `package` and copies the file only when it exits 0; `environment.etc`
  takes its source from that output, so an invalid configuration fails
  the system build before anything is deployed. `check` needs nothing
  from the machine ([reference.md](reference.md#check)): roots absent from
  the build sandbox only warn, and `owners:` names resolve at run time.
  The files are copied into `/etc` (`mode = "0644"`), not linked to the
  store, so `--allow-root`'s ownership rule holds for them. Not checked at
  build time: `configFile`, and everything diskgc checks at run time.

### CI runner: a warm index

A system instance with `--tally-only`, as the runner user, on the
runner's builds directory. Generic Linux:

```ini
# /etc/systemd/system/diskgc-watch@home-gitlab\x2drunner-builds.service.d/user.conf
[Service]
User=gitlab-runner
```

```
# /etc/diskgc/watch/home-gitlab\x2drunner-builds.env
DISKGC_WATCH_ARGS=--tally-only --min-free 50G
```

NixOS:

```nix
services.diskgc = {
  enable = true;
  settings = { /* the pre-job hook's configuration */ };
  instances.runner = {
    scope = "system";
    user = "gitlab-runner";
    path = "/home/gitlab-runner/builds";
    tallyOnly = true;
    minFree = "50G";  # the pre-job hook's --min-free
  };
};
```

It must agree with the pre-job hook's `ensure` on the user (so both use
`~gitlab-runner/.local/state/diskgc`), the configuration (the hook passes
`--config /etc/diskgc/config.yaml`, or its lookup finds that file),
`--select` and `--roots`, and the filesystem
([reference.md](reference.md#tally-only)). It never samples for a goal,
takes the host lock or deletes, so the sandbox gate does not apply and it
runs from a development package too. `--min-free` only bounds the
snapshot write.

### Choices to confirm

Not covered by Mark's decisions; each is the simplest sound option found.

1. Instance name = escaped `PATH`, flags in an optional environment file
   per instance; no `--select` with spaces through that file.
2. One unit name, `diskgc-watch@.service`, for both managers.
3. No namespacing in either unit, `ProtectSystem=full` included.
4. Shipped `ExecStart=/usr/local/bin/diskgc` (the README's path);
   distribution packages and the module replace it.
5. The system template has no `User=`: unset means root without
   capabilities, which exits 2.
6. Root instances get the four-capability bounding set above, and
   `--allow-root` refuses to start without `CAP_SYS_PTRACE`. The set is
   unproved until a release package that includes `watch` runs an
   `allowRoot` reclaim (the module's VM test shows only that it is
   applied and that the refusal works).
7. `RestartSec=30s`, `KillMode=mixed`, `TimeoutStopSec=30s`, no
   `ExecReload=`.
8. State stays in `User=`'s home (no `StateDirectory=`); system units pin
   `DISKGC_CONFIG=/etc/diskgc/config.yaml`, user units use the normal
   lookup.
9. `RestrictAddressFamilies=` and `RestrictNamespaces=` shipped although
   they bind command actions; `MemoryDenyWriteExecute=` not shipped;
   `RestrictSUIDSGID=no` shipped explicitly.
10. Module: one `instances` set with `scope` (default `"user"`), typed
    options only where they shape the unit, everything else in
    `extraArgs`; per-instance `settings`, configuration files copied into
    `/etc`; it does not set `users.users.<name>.linger` but warns when a
    user-scope user lacks it.
11. One NixOS VM test for the module (TASK-45, `vm-nixos-module`), run
    like `vm-release-tmp-reclaim` but from `just e2e-full`, not
    `just e2e`, although the PRD lists a VM test suite as a non-goal for
    now.

Accepted by the mped-architect review of 2026-10-31 with the adjustments
above (Mark's confirmation still open).
