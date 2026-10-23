---
id: TASK-29
title: >-
  Tally engine: mini-batches, duty cycle, cursors, aging, pause on host lock,
  synchronous mode
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-23 21:15'
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
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-10 (mped): the pre-index tally treats a refusal as final for the run (missed reclaim, never unsafe). The index must never carry a stored refusal across runs as final, or 'never fake exhaustion' breaks; a stored refusal is a hint, re-judged at reclaim.

Orchestrator 2026-10-23 (design audit): design gaps: 'aging' and 're-tallies scheduled by age' have no policy or constants; no re-check cadence; cursor encoding unspecified; the contention test referenced as 'measured separately' is not specified in docs/testing.md. Decide and document in this task.

Forward-carried from TASK-64 (2026-10-23): a flock (or a bound Unix socket) is released when the LAST reference to its open file goes, which can be after close()/exit returned: any /proc/<pid>/fd reader (an in-use scan, diskgc's own included, lsof) holds a transient reference for the length of its syscall (measured up to 0.5 ms on 7.2; about 1 in 60 immediate retries busy under four readers). Never assert a release with one attempt right after a close; wait with a bound (RELEASE_BOUND, docs/testing.md Negative feedback). A held-lock assertion needs no wait. The tally's 'pause on host lock' must poll for the lock, not expect it free the instant another reclaim exits; and the tally's own /proc scans delay other processes' lock releases by the same mechanism (harmless: a lagging release only keeps a lock longer).

Correction to the TASK-64 forward-carry above (2026-10-23, measured): which /proc reads hold a file reference depends on the kernel (fs/proc/fd.c): reading /proc/<pid>/fdinfo on every supported kernel, statting or reading an fd link from 5.11, listing the fd directory from 6.7. diskgc's scan lists and stats fd links, so it can delay another program's lock release only from 5.11 on; on 5.8.16 those reads gave no late release in about 1.7 million attempts. A test still waits with RELEASE_BOUND on every kernel (other readers, e.g. lsof, read fdinfo). docs/safety.md Host lock has the table.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
