---
id: TASK-190
title: >-
  An unsettled scan or unexcused unreadable process is reported as in_use, not
  in_use_unknown
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
From the 10-task codebase review at 72b376c (2026-10-25). inuse/mod.rs:Scan::blocks_all / Scan::check: 'could not tell' gets in_use with real-looking evidence (same class as TASK-166's EMFILE mislabel); harnesses cannot tell retry from busy; these skips do not count toward the 3-in-a-row in_use_unknown breaker, so a fork storm costs two full scans per candidate. Fix: map to in_use_unknown, count toward the breaker, update safety.md 'Unsettled'.
<!-- SECTION:DESCRIPTION:END -->
