---
id: TASK-6
title: >-
  In-use detection: /proc by device and inode, initial environment, flock;
  held-by-deleted-files
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:49'
labels:
  - wave-1
  - irreversible
  - safety
dependencies:
  - TASK-3
  - TASK-14
references:
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: deleting files in use is the worst failure. Implement the in-use checks: open fds, cwd, root, memory maps matched by (dev, inode); initial-environment path heuristic; flock on the candidate directory; the one-second freshness bound per candidate; held_by_deleted_files collection. See docs/safety.md#in-use.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Container tests: a file held open, a cwd, an mmap, a TMPDIR in the initial environment, a flocked dir each make the candidate in_use / locked
- [ ] #2 The known limit (export after exec is invisible) is pinned by a test and documented
- [ ] #3 A process in another mount namespace (an unshare helper bind-mounting the scratch dir at a different path) is matched by device and inode
- [ ] #4 A same-user non-dumpable process whose fd directory gives EACCES makes candidates in_use; processes exiting mid-scan (ESRCH/ENOENT) do not abort the scan
- [ ] #5 A running binary mapping its shared library from an overlayfs scratch dir is detected (overlay maps-inode mismatch)
- [ ] #6 The one-second freshness bound holds across a simulated suspend (model test with a monotonic clock including suspend time)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
