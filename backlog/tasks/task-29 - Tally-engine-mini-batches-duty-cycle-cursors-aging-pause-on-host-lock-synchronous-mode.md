---
id: TASK-29
title: >-
  Tally engine: mini-batches, duty cycle, cursors, aging, pause on host lock,
  synchronous mode
status: Done
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-25 22:06'
labels:
  - wave-3
  - architecture
dependencies:
  - TASK-28
references:
  - docs/architecture.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the index must stay fresh without pounding the disk. Implement the tally per docs/architecture.md#the-tally over the Host port: batches of at most 50 ms or 2,000 operations, sleep 19x the work, idle I/O priority and nice, resumable cursors surviving ENOENT, aging between discovery/re-check/re-tally, pausing while the host lock is held, synchronous mode with --tally-budget before taking the lock.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The tally pauses whenever another process holds the host lock (container test)
- [ ] #2 --tally-budget 0 runs to completion; a small budget leaves untallied candidates reported in the index block
- [ ] #3 The duty cycle and operation counts are asserted as bounds, not exact values, in model tests with the virtual clock
- [ ] #4 Background tally backs off under measured pressure (PSI io/memory > 10%, cpu > 50% some avg10; skip batch, doubling sleep to 60 s; reset below all thresholds); without PSI only the duty cycle applies with one log line; proven by model tests with scripted PSI and a VM test where I/O pressure drops batches/min >= 4x and recovers within 2 min
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-10 (mped): the pre-index tally treats a refusal as final for the run (missed reclaim, never unsafe). The index must never carry a stored refusal across runs as final, or 'never fake exhaustion' breaks; a stored refusal is a hint, re-judged at reclaim.

Orchestrator 2026-10-23 (design audit): design gaps: 'aging' and 're-tallies scheduled by age' have no policy or constants; no re-check cadence; cursor encoding unspecified; the contention test referenced as 'measured separately' is not specified in docs/testing.md. Decide and document in this task.

Forward-carried from TASK-64 (2026-10-23): a flock (or a bound Unix socket) is released when the LAST reference to its open file goes, which can be after close()/exit returned: any /proc/<pid>/fd reader (an in-use scan, diskgc's own included, lsof) holds a transient reference for the length of its syscall (measured up to 0.5 ms on 7.2; about 1 in 60 immediate retries busy under four readers). Never assert a release with one attempt right after a close; wait with a bound (RELEASE_BOUND, docs/testing.md Negative feedback). A held-lock assertion needs no wait. The tally's 'pause on host lock' must poll for the lock, not expect it free the instant another reclaim exits; and the tally's own /proc scans delay other processes' lock releases by the same mechanism (harmless: a lagging release only keeps a lock longer).

Correction to the TASK-64 forward-carry above (2026-10-23, measured): which /proc reads hold a file reference depends on the kernel (fs/proc/fd.c): reading /proc/<pid>/fdinfo on every supported kernel, statting or reading an fd link from 5.11, listing the fd directory from 6.7. diskgc's scan lists and stats fd links, so it can delay another program's lock release only from 5.11 on; on 5.8.16 those reads gave no late release in about 1.7 million attempts. A test still waits with RELEASE_BOUND on every kernel (other readers, e.g. lsof, read fdinfo). docs/safety.md Host lock has the table.

Mark's decisions 2026-10-25 (his own, not the emulator): the background tally (watch, async) must NOT load the system (duty cycle, nice/ionice, pause on host lock as designed); the synchronous tally (ensure) MAY load the machine and must be fast and quickly produce a USEFUL tally (one that lets the reclaim walk act). The tally updates the index; deletion enacts it.

Design settled 2026-10-25 in fe54fe6 (branch index-design, not merged): docs/architecture.md The index / Memory budget and collapse / Order within a tier / Refusals and index updates / The tally / reclaim walk; safety.md Discovery; testing.md index scenarios. Schedule decided (discovery 5 min, re-check 10 min, re-tally 60 min, lateness/period aging); sync tally = pass 1 discovery of every selected tier, pass 2 sizing; cursors not persisted.

Mark's decisions 2026-10-25 (his own): background tally backs off on measured pressure (PSI cpu/io/memory where available) in addition to the duty cycle and priorities; ensure with a valid but old snapshot re-sizes only the first tier's stale units.

2026-10-25: pressure-brake AC added (Mark's decision; design 076c5ec).

Mark's answers 2026-10-25 (his own) on TASK-28's open points: a complete discovery listing of a block's site recounts the block's members and estimates; partial listings leave them (TASK-28 open point 1).

Implemented on branch t29 (not merged): 346751c (TASK-230), e1bf9a2 (engine), a87997e (VM test, built not run), 5ce3859 (review fixes). Module crates/diskgc-core/src/tally/ {budget,pace,schedule,size,listing,recount,sync,background}; Host::pressure + Host::lower_priority (LinuxHost, ModelHost scripted 'pressure:' steps and 'call_cost'); hostlock::held_by_another (shared flock probe, closed at once); Index::{set_cursor_bytes,drop_finder,recount_block}; engine::Seen on fs Found / cargo Target. Model tests crates/diskgc-model/tests/tally.rs (21). 22 manual mutations killed. One mped-architect round: fix first (stalled sizing cursor, unbounded host-error retries, unbounded background listing) -> fixed in 5ce3859, fixes not re-reviewed. Open for Mark: background listing is not resumable (bounded at 1 s; roots too large never complete in the background); 100,000-entry no-starvation model run not done (scaled down); AC #1 container test and AC #2 report field belong with watch/ensure wiring (TASK-32/31). VM test vm-tally-pressure ready, not run.

Mark's decision 2026-10-25 (his own): background discovery is RESUMABLE (cursor across batches, within the 50 ms / 2,000-op bound); a complete listing is one whose cursor reached the end without refusals.

Mark's decision (2026-10-25, his own, relayed by the orchestrator): background discovery is RESUMABLE - discovery keeps a cursor like sizing, so watch lists huge roots across many small batches within the 50 ms / 2,000-op bound; a complete listing is one whose cursor reached the end without refusals, across batches; entries changing under the cursor must not corrupt it. Implemented on t29 (rebased on 926fc88): 3da4d83 (fs_kind and cargo_target discovery as names-only explicit-stack walks, DiscoverCursor; tally ListingCursor / TallyState::list_round; round eviction by checked_at < round start; block recount skipped when a collapse ran during the round; no dissolution in background rounds), b59198d (re-review non-blockers). mped-architect re-review of e83a9ba + 3da4d83: ship, no blockers; non-blockers fixed in b59198d. Gate: just check exit 0 on a forced rebuild; fast container suite 225/225 (an earlier run 224/225: pre-existing command-output flake, TASK-237). Follow-ups filed: TASK-235 (100k no-starvation model run), TASK-236 (watch: tally on its own thread for per-thread lower_priority; AC #1 container test). AC #2 (--tally-budget reporting) belongs with TASK-31's ensure wiring (SyncReport carries untallied/listed/skipped). VM test vm-tally-pressure built, not run (awaiting the VM window). Head: b59198d.

Merged to master as 271cbe6 on 2026-10-25T22:06Z (not in release-0.1).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
