---
id: TASK-190
title: >-
  Unknown is first-class: unsettled/unreadable scan -> in_use_unknown; EMFILE
  never swallowed; failed scan -> held_by_deleted_files null
status: Done
assignee: []
created_date: '2026-10-25 05:59'
updated_date: '2026-10-25 12:45'
labels:
  - shipping
  - json
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the 10-task codebase review at 72b376c (2026-10-25). inuse/mod.rs:Scan::blocks_all / Scan::check: 'could not tell' gets in_use with real-looking evidence (same class as TASK-166's EMFILE mislabel); harnesses cannot tell retry from busy; these skips do not count toward the 3-in-a-row in_use_unknown breaker, so a fork storm costs two full scans per candidate. Fix: map to in_use_unknown, count toward the breaker, update safety.md 'Unsettled'.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
COMPASS round 3 (2026-10-25): one task with TASK-185 and TASK-187 (same concept, frozen JSON surface); do together with TASK-189 (deadline) in one in-use pass, unifying the FreshAttempts loop (TASK-196 item 3).

2026-10-25 inuse2 ee643a6 on branch inuse2 (not merged; base master 28e9456). See commit message for the change. Scan::unknown: unsettled scan / unexcused unreadable process -> in_use_unknown (fs Scan::check, cargo judge_in_use), counted by the 3-in-a-row breaker; human summary names the process ('pid N (cmd) cannot be inspected'). safety.md In use (Unsettled, unreadable, Freshness) updated. Evidence: an_unreadable_process_counts_toward_the_in_use_unknown_breaker (master: 5x in_use; branch: 5x in_use_unknown, 2 unchecked), unsettled probe (master in_use). diskgc-e2e projected in-use/held-lock vectors had agreed with Linux only through the unreadable rule; now model the copied probe. Container: python_in_use_vector_matches_linux, cargo_target_{held,shared}_lock_matches_linux, materialized_lock_releases_on_finish, host_inuse_scan_settles_after_fork_churn, host_inuse_scan_under_sustained_fork_churn PASS.

Merged to master as ee643a6 on 2026-10-25T12:45Z.
<!-- SECTION:NOTES:END -->
