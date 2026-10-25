---
id: TASK-193
title: 'Human output: Report needs an explicit dry_run flag; dry-run listing unbounded'
status: Done
assignee: []
created_date: '2026-10-25 05:59'
updated_date: '2026-10-25 07:32'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the 10-task codebase review at 72b376c (2026-10-25). report/human.rs:summary guesses dry run from status==DryRun || !plan.is_empty(); a dry run failing before any plan row (protect scan failure in executor.rs:dry_run_inner) renders as a live run ('0 reclaimed (observed change +0 B)'). Dry-run listing is one line per candidate (~6400 lines for pytest-tmp). Fix: Report.dry_run: bool; group kept dry-run rows like the live summary (KEPT_EXAMPLES_MAX).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented on branch contract (3234666): Report.dry_run (JSON dry_run, a contract addition decided with TASK-192); human.rs uses it, so a failed dry run without plan rows renders 'dry run, error: ...' (walked by accident: a malformed test clock offset file gave exit 3 'dry run, error' with the goal kept, dump 12a/13a). Dry-run listing bounded like the live summary: shared KeptRow grouping above KEPT_EXAMPLES_MAX, first RECLAIMED_LINES_MAX per eligible verdict then a count with summed estimate; --json keeps every row. Walk: WALK_TMP_BULK=4000, 4162-candidate dry run: master 1968 pane lines, contract 7 (dumps 10-*). Kept rows now group by reason, so in_use rows precede the keep_marker row (order change visible in dump 30). Eligible-row bounding (>10 eligible) only unit-tested (6403-row test): the walk data had no eligible bulk rows (ctime ages; producer_unknown outside the host PID namespace).

Merged to master as d4f2bb4 on 2026-10-25T07:32Z.
<!-- SECTION:NOTES:END -->
