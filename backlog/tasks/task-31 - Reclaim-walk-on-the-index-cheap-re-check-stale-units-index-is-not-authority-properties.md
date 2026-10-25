---
id: TASK-31
title: >-
  Reclaim walk on the index: cheap re-check, stale units, index-is-not-authority
  properties
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-25 17:21'
labels:
  - wave-3
  - irreversible
  - safety
dependencies:
  - TASK-30
references:
  - docs/architecture.md
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the index must change speed, never safety. Integrate the index into the reclaim walk: queue per tier, cheap identity re-check, stale skips, per-tier discovery after the queue, live checks unchanged; property tests with randomly stale/partial/corrupted indexes; the stale-index model scenarios from docs/testing.md.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Property: with a corrupted index nothing is deleted that a correct-index run would refuse
- [ ] #2 Stale-index scenarios (deep change, new keep file, new Cargo target inside, new protected match) are refused at deletion time
- [ ] #3 With an index present, new stale cruft created after the snapshot is deleted before any pressure tier runs
- [ ] #4 Time to first deletion on the 100k-entry fixture, cold and warm, is measured and logged (not a pass criterion: no latency target); ensure reaches --min-free and exits 0 in both
- [ ] #5 A block member opened by a process after an earlier member of the same block was deleted is skipped as in_use
- [ ] #6 A lump (block across finder sets, TASK-28) whose site lies above a tier's bound root is expanded by that tier's own discovery from the deeper of the site and the bound root, never from above the root; every member is still checked live (model test)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Orchestrator 2026-10-23 (design audit): time-to-first-deletion (PRD: 10 s on a warm 100k-entry /tmp) has no defined start/stop events, clock, or JSON report field; define them here and add the measurement to the report. The index-authority property (testing.md ~L471) is deferred to this task.

Forward-carried from TASK-64 (2026-10-23): a flock (or a bound Unix socket) is released when the LAST reference to its open file goes, which can be after close()/exit returned: any /proc/<pid>/fd reader (an in-use scan, diskgc's own included, lsof) holds a transient reference for the length of its syscall (measured up to 0.5 ms on 7.2; about 1 in 60 immediate retries busy under four readers). Never assert a release with one attempt right after a close; wait with a bound (RELEASE_BOUND, docs/testing.md Negative feedback). A held-lock assertion needs no wait.

Correction to the TASK-64 forward-carry above (2026-10-23, measured): which /proc reads hold a file reference depends on the kernel (fs/proc/fd.c): reading /proc/<pid>/fdinfo on every supported kernel, statting or reading an fd link from 5.11, listing the fd directory from 6.7. diskgc's scan lists and stats fd links, so it can delay another program's lock release only from 5.11 on; on 5.8.16 those reads gave no late release in about 1.7 million attempts. A test still waits with RELEASE_BOUND on every kernel (other readers, e.g. lsof, read fdinfo). docs/safety.md Host lock has the table.

Orchestrator 2026-10-24 (COMPASS round 2): takes TASK-108's AC1 (index is never an authority) as its own lock-in test, and the 'stale' skip-reason vector from TASK-127. Measure the 10 s time-to-first-deletion on the shipped static musl binary; TASK-115 (musl scan 3-6x slower) must land before that measurement.

Mark's decisions 2026-10-25 (his own, not the emulator): no latency acceptance (the 10 s first-deletion target is dropped); the reclaim walk acts on whatever the index says, re-checking live; coarse (collapsed) index blocks mean more stat work at delete time, never a weaker check.

Design settled 2026-10-25 in fe54fe6 (branch index-design, not merged): docs/architecture.md The index / Memory budget and collapse / Order within a tier / Refusals and index updates / The tally / reclaim walk; safety.md Discovery; testing.md index scenarios. AC review needed: #3 (10 s first-deletion bound dropped; time is measured and logged only). Index units are re-discovered and re-matched at reclaim (stale otherwise).

AC rewrite: design settled in fe54fe6 + 076c5ec (branch index-design); Mark's decisions 2026-10-25. Old #3 (meets the PRD's 10 s bound) replaced: the bound is dropped.

2026-10-25: AC wording bucket -> block (collapse model, design 076c5ec).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
