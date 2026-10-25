---
id: TASK-193
title: 'Human output: Report needs an explicit dry_run flag; dry-run listing unbounded'
status: To Do
assignee: []
created_date: '2026-10-25 05:59'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the 10-task codebase review at 72b376c (2026-10-25). report/human.rs:summary guesses dry run from status==DryRun || !plan.is_empty(); a dry run failing before any plan row (protect scan failure in executor.rs:dry_run_inner) renders as a live run ('0 reclaimed (observed change +0 B)'). Dry-run listing is one line per candidate (~6400 lines for pytest-tmp). Fix: Report.dry_run: bool; group kept dry-run rows like the live summary (KEPT_EXAMPLES_MAX).
<!-- SECTION:DESCRIPTION:END -->
