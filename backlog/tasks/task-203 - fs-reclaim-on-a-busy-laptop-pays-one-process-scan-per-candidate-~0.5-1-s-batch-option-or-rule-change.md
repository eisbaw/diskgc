---
id: TASK-203
title: >-
  fs reclaim on a busy laptop pays one process scan per candidate (~0.5-1 s):
  batch option or rule change
status: Done
assignee: []
created_date: '2026-10-25 09:01'
updated_date: '2026-10-25 11:37'
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

2026-10-25 implementer (branch inuse-pass). Commit 6faa797: fs_kind::reclaim_alternatives_checked runs under checker.reusing_for(FRESHNESS): one scan serves consecutive fs delete candidates while each delete starts on a scan < 1 s old; command actions keep 500 ms; FreshAttempts unchanged. Property test no_fs_delete_starts_on_a_scan_past_the_bound (random scan/unlink/final-check durations; fails if delete_cleared's expiry check is removed or the 500 ms window restored); model test: 600 ms scans, 200 ms per candidate -> 3 scans for 6 candidates (6 before). Adjusted forty_thousand_tmp_candidates_reclaim_once_per_reuse_window (1 s boundary) and vector busy-host-reclaim-resets-the-in-a-row-count. Walk tmp/walks/2026-10-25-inuse-pass/2*-busy-none-*: container with ~3,400 processes (2,500 extra sleeps), 8 sessions per run, load 5.9-7.9: before 1120/1046/63 ms per deleted candidate (median action 792/838/1 ms: a scan per candidate whenever the scan took > 500 ms; run 3's scans were apparently under 500 ms), after 66/102/64 ms (action median 2/2/1 ms). Without the extra processes scans are fast and both builds are 7-10 ms per candidate. Laptop host not walked (reclaim on host forbidden).

Merged to master as 91a23e6 on 2026-10-25T11:37Z. Orchestrator accepted the implementer's safety addition to the mark-emulator decision: full re-listing after the walk when the newest walked change is unknown or within 3 s of the tier listing, or the wall clock jumped (fail-safe; adds checking only).
<!-- SECTION:NOTES:END -->
