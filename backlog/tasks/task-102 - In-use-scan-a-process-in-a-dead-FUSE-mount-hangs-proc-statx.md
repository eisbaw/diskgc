---
id: TASK-102
title: 'In-use scan: a process in a dead FUSE mount hangs /proc statx'
status: To Do
assignee: []
created_date: '2026-10-19 15:05'
updated_date: '2026-10-19 21:27'
labels:
  - safety
dependencies:
  - TASK-18
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: task-18 limit (standing condition): the in-use scan statx-es each visible process open fd, cwd and root through /proc; a process in a dead FUSE mount can hang the scan. AT_STATX_DONT_SYNC may make FUSE use cached attributes, but needs a container test. A dead NFS mount is a separate limit: /proc magic-link lookup can call d_weak_revalidate and still wait for the server even with AT_STATX_DONT_SYNC. Diagnose NFS separately; do not claim this task fixes it. This blocks 0.1 if the FUSE hang is reproducible: a stale mount can hang every reclaim.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A process whose cwd and an open fd sit in a mount that stopped answering does not hang ensure: the scan finishes within the deadline (container test with a FUSE server that answers then stops)
- [ ] #2 The candidate it would have touched is still never reported 'not in use' falsely (fail closed: unreadable evidence keeps it)
<!-- AC:END -->
