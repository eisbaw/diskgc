---
id: TASK-191
title: >-
  Startup-failure report drops the parsed goal; agent-pre-job.py exits 4 instead
  of 3
status: To Do
assignee: []
created_date: '2026-10-25 05:59'
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
