---
id: TASK-102
title: 'In-use scan: a process in a dead FUSE mount hangs /proc statx'
status: Done
assignee: []
created_date: '2026-10-19 15:05'
updated_date: '2026-10-20 06:01'
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
- [x] #1 A process whose cwd and an open fd sit in a mount that stopped answering does not hang ensure: the scan finishes within the deadline (container test with a FUSE server that answers then stops)
- [x] #2 The candidate it would have touched is still never reported 'not in use' falsely (fail closed: unreadable evidence keeps it)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-20 partial fix in commit 8a84934: STATX_DONT_SYNC avoids the demonstrated FUSE hang when cached attributes exist. Disposable-container test asserts ensure exits in under six seconds after a 3s CLI timeout, reports in_use, and preserves the candidate; removing the flag made it hang 8.09s. Full e2e passed 127/127 Nix and 2/2 Docker, with QA/MPED review. AC1 still requires finishing within the 3s deadline and is not proven by that assertion, so task remains In Progress pending a strict deadline assertion and passing evidence. Dead NFS remains a separate documented possible hang.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Commit 8a84934 added STATX_DONT_SYNC for /proc magic-link target statx, avoiding the demonstrated stopped-FUSE metadata hang while retaining in_use refusal and candidate preservation. The disposable-container E2E now asserts the diskgc subprocess completes within its 3s CLI deadline; four focused runs passed, with total per-test duration 2.22-2.26s including a deliberate 2s stalled-mount probe. Mutation removing the flag previously hung 8.09s beyond the CLI timeout. Full nix develop -c just e2e passed 127/127 Nix and 5/5 native Docker tests. Dead NFS remains a separate documented limit.
<!-- SECTION:FINAL_SUMMARY:END -->
