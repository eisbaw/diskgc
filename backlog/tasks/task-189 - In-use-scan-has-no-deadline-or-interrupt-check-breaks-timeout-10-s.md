---
id: TASK-189
title: In-use scan has no deadline or interrupt check (breaks --timeout + 10 s)
status: Done
assignee: []
created_date: '2026-10-25 05:59'
updated_date: '2026-10-25 12:45'
labels:
  - release-blocker
  - prd
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the 10-task codebase review at 72b376c (2026-10-25). inuse/scan.rs:gather and inuse/mod.rs:FreshAttempts::retry: up to 16 listings plus an unsettled retry can take tens of seconds with no kernel block, breaking the PRD's 'ensure returns within --timeout + 10 s'; the retry sleep ignores signals. Fix: pass the deadline and interrupted() into gather, check between listings, return Stop::DeadlinePassed.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 inuse2 ee643a6 on branch inuse2 (not merged; base master 28e9456). See commit message for the change. Scan takes the run deadline; stop_if_due (interrupted, then deadline) before every listing and after every pause in gather/pid_table; ScanError {Failed, DeadlinePassed, Interrupted} -> Stop (new Stop::Interrupted, no error recorded). Retry pause sliced at PAUSE_SLICE 50 ms. One freshness loop Checker::with_fresh_clear (TASK-196 item 3) for delete, Cargo parts and command actions. Evidence: model tests a_slow_unsettled_scan_stops_at_the_deadline (master: in_use after 6.42 s past a 1 s deadline; branch: DeadlinePassed <= 1.2 s), a_signal_during_the_retry_pause_stops_promptly (master: deleted after 5.25 s; branch: Interrupted after one 50 ms slice), a_signal_during_a_scan_stops_it_at_the_next_listing (master: 18 listings; branch: 3). Host walk (read-only dry run, 654 procs, load ~13): --timeout 30s -> status deadline, exit 124 at 30.07 s, held null.

Merged to master as ee643a6 on 2026-10-25T12:45Z.
<!-- SECTION:NOTES:END -->
