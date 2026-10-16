---
id: TASK-31
title: >-
  Reclaim walk on the index: cheap re-check, stale units, index-is-not-authority
  properties
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:51'
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
- [ ] #3 Time to first deletion on the 100k-entry fixture meets the PRD bound for small candidates and is reported for large ones
- [ ] #4 A bucket member opened by a process after an earlier member was deleted is skipped as in_use
- [ ] #5 With an index present, new stale cruft created after the snapshot is deleted before any pressure tier runs
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
