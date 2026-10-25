---
id: TASK-162
title: 'Host lock: a host I/O failure while locking is exit 2 / cause usage'
status: To Do
assignee: []
created_date: '2026-10-25 00:44'
labels:
  - followup
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while doing TASK-151 (2026-10-25). executor::lock_refusal maps LockError::Failed (openat/reopen/flock/fcntl failure) to exit 2, status usage_error, cause usage, unchanged from before. reference.md says host I/O failures are exit 3 / cause host. Decide (CLI contract, before 0.1) whether Failed should be status error, exit 3, cause host; update reference.md Exit codes and a vector.
<!-- SECTION:DESCRIPTION:END -->
