---
id: TASK-191
title: >-
  Startup-failure report drops the parsed goal; agent-pre-job.py exits 4 instead
  of 3
status: Done
assignee: []
created_date: '2026-10-25 05:59'
updated_date: '2026-10-25 07:32'
labels:
  - shipping
  - json
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the 10-task codebase review at 72b376c (2026-10-25). main.rs:ensure -> fail_host/failed_report: min_free_bytes and target_free_bytes are null although parsed; docs/agent-pre-job.py:check_space raises 'report has no space goal' and exits 4. Triggers include pidfd under seccomp (TASK-126). Fix: build the report from EnsureOptions before startup(); hook test for exit 3.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented on branch contract (3234666): EnsureOptions::report / executor Request::report seed every report with PATH, --dry-run and the goal as far as the command line gives it (bytes as given; a percentage stays null until sampled). main.rs startup, signal-handler and config failures start from it. docs/agent-pre-job.py: an exit-3 report that measured nothing may lack a percentage goal -> abort_error (class fix: any exit 3 before the first sample, e.g. a PATH statvfs failure, had the same exit-4 bug). Hook test test_a_startup_failure_is_abort_error_not_an_invalid_report (byte and percent goals). Walked: startup failure provoked with unshare -U -p -f (NSpid check) in walk env contract: master hook exit 4 'report has no space goal', contract hook exit 3 abort_error with min_free_bytes 1073741824 (dumps 23-25). pidfd-under-seccomp itself not provoked.

Merged to master as d4f2bb4 on 2026-10-25T07:32Z.
<!-- SECTION:NOTES:END -->
