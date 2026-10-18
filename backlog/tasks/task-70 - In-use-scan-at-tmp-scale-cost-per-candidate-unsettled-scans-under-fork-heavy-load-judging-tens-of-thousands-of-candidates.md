---
id: TASK-70
title: >-
  In-use scan at /tmp scale: cost per candidate, unsettled scans under
  fork-heavy load, judging tens of thousands of candidates
status: To Do
assignee: []
created_date: '2026-10-18 11:06'
updated_date: '2026-10-18 12:42'
labels:
  - wave-2
  - spike
  - safety
dependencies: []
references:
  - docs/safety.md#in-use
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: task-68 dry-ran fs profiles on a real laptop (155,000 top-level /tmp entries, ~500 processes / 2,200 threads, load 10 to 90 from agents' builds). Measured:

- One process scan costs ~0.5 s wall unloaded (1.7k pidfd_open, 110k statx), several seconds loaded; perf shows the CPU in /proc maps parsing and musl malloc lock contention (__lock 47%, malloc/free 15%) across the parallel /proc readers.
- With inuse::Checker's reuse (<= FRESHNESS), a scan per candidate made a 328-candidate dry run take 586 s; reusing one scan for 60 s (the spike's DRY_RUN_SCAN_REUSE, dry run only) took it to 7 s. A reclaim cannot do that: each deletion needs a scan <= 1 s old, so candidates spread over a large tree (or found between long walks) each pay a scan.
- Judging alone (no scan per candidate) still ran ~55 candidates/s at nice 19 / ionice idle: nix-shell-rc (105k candidates, 350 MiB) did not finish in 20 min (exit 124); nix-shell-tmpdir (41k) took 18 min. The PRD's time-to-first-deletion is fine, but a reclaim of /tmp at this scale spends its timeout on tiny candidates.
- Under fork-heavy load the scan does not settle (16 listings bringing new pids) and blocks EVERY candidate as in_use for as long as it is reused: 13 of 328 candidates in one run. The spike reuses a blocking scan only within REUSE_LIMIT.

Scope (to decide with task-18 / task-29): batch the fs reclaim so one fresh scan clears every candidate checked within its freshness window (discover, walk, then check many against one scan right before deleting them); retry an unsettled scan after a short pause instead of blocking; cut the scan's allocations (or use a non-global-lock allocator in the static build); decide whether tiny-candidate profiles (nix-shell-rc: 3 KiB each) are worth per-candidate checks or need a cheaper class.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A reclaim of 40k small /tmp candidates on a model of this machine stays within one scan per freshness window, not one per candidate
- [ ] #2 An unsettled scan is retried before it blocks candidates; the limit stays documented in docs/safety.md
- [ ] #3 Scan cost on the real laptop is measured again after the change and recorded
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-68's review (mped S4): there are now two scan-reuse policies. inuse::Checker clamps reuse to FRESHNESS (the cargo dry run and every reclaim use REUSE_LIMIT, 0.5 s), and fs_kind::plan::Scans reuses a scan for DRY_RUN_SCAN_REUSE (60 s), except a blocking scan (0.5 s). Do: one scan-reuse policy in inuse with a plan mode (no freshness bound, never reusing a blocking scan past REUSE_LIMIT) used by both dry runs, then delete fs_kind::plan::Scans and fix the reference.md --dry-run sentence, which today states the difference (cargo 0.5 s, fs 60 s).
<!-- SECTION:NOTES:END -->
