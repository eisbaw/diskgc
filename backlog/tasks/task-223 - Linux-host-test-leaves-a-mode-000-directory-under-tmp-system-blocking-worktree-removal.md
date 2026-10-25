---
id: TASK-223
title: >-
  Linux host test leaves a mode-000 directory under tmp/system, blocking
  worktree removal
status: To Do
assignee: []
created_date: '2026-10-25 13:14'
labels:
  - test
  - tooling
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
2026-10-25: removing the merged rc-walk worktree failed ('Directory not empty'): tmp/system/dg-2689734-0/locked/unreadable had no read/write permission, left by a diskgc-linux host test (just check) that does not restore permissions on cleanup. 5.3 GiB stayed behind until permissions were restored. Fix the test to restore modes in a drop guard (also on panic).
<!-- SECTION:DESCRIPTION:END -->
