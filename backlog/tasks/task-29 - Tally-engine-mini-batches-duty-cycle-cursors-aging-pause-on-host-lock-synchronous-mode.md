---
id: TASK-29
title: >-
  Tally engine: mini-batches, duty cycle, cursors, aging, pause on host lock,
  synchronous mode
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:51'
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

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
