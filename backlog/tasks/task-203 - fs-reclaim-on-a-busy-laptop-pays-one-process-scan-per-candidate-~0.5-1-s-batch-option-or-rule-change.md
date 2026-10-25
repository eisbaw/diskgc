---
id: TASK-203
title: >-
  fs reclaim on a busy laptop pays one process scan per candidate (~0.5-1 s):
  batch option or rule change
status: To Do
assignee: []
created_date: '2026-10-25 09:01'
updated_date: '2026-10-25 09:04'
labels:
  - shipping
  - performance
  - safety
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-184. Host scans now (1609 processes, load 17-19): glibc median 336 ms, static 514 ms, p95 1050 ms. A pytest session's walk + delete (~100-300 ms on this disk) pushes the next check past the 500 ms reuse limit, so each candidate gets its own scan: ~1-2 h to reclaim the laptop's 6400 sessions. When scans are fast the protocol already shares them (walk 2026-10-25-t184: 5-33 ms/candidate). Decision options (TASK-184 notes): (a) batch: lock and walk up to K candidates within a time budget, then one scan, check and delete each while the scan is < 500 ms old at its check and < 1 s at its delete start (no rule change; K locks held; walk->delete widens to the batch budget); (b) extend TASK-173's 1 s reuse from Cargo parts to fs candidates (rule change); (c) cheaper scans (TASK-174); (d) accept. Measure with a model test counting scans per candidate under a clock that advances per walk.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision 2026-10-25 by mark-emulator (Mark delegated; Mark may veto): extend the 1 s scan reuse to fs candidates as TASK-173 did for Cargo parts: one scan serves consecutive fs candidates while each delete starts on a scan younger than FRESHNESS (1 s); never start a delete on an older scan. One review round.
<!-- SECTION:NOTES:END -->
