---
id: TASK-186
title: >-
  Scan: identify processes by pidfd inode on kernels >= 6.9 instead of holding
  one pidfd per process
status: To Do
assignee: []
created_date: '2026-10-25 05:34'
labels:
  - performance
  - limits
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-166: the scan holds one pidfd per visible process for the whole scan (peak 3694 fds with ~3500 processes). On Linux >= 6.9 a pidfd's inode number identifies the process, so the scan could hold fds per reader, keeping the pidfd pin as fallback on older kernels.
<!-- SECTION:DESCRIPTION:END -->
