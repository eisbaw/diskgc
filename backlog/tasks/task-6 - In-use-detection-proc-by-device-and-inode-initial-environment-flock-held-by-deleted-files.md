---
id: TASK-6
title: >-
  In-use detection: /proc by device and inode, initial environment, flock;
  held-by-deleted-files
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 00:22'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-2: (1) The code under test runs as uid 1000 with no capabilities, so it sees only same-uid processes' fds and maps. Helper processes that hold files open must run as tester to be visible, or as another uid to test the invisible case. (2) under_test::run_as_tester blocks until exit; long-lived helpers need a spawn variant (see the task-4 note). (3) Container root has CAP_SYS_PTRACE, so harness-side checks can read any /proc entry in the container. The container has its own pid namespace with a clean /proc.

Forward-carried from task-59: (1) /home/tester is a fresh tmpfs per test (0755 tester) and /run/diskgc a tmpfs with the lock; deleted-but-open files on these and on /tmp keep stable (dev, ino, btime) keys. (2) Helper processes that hold files open are fixtures: start them untraced via Command (as tester through 'diskgc-e2e as-tester -- ...' if they need tester's uid); only diskgc runs through ctx.run_as_tester, and every such run must be judged by the oracle (Ctx::teardown). (3) strace -y annotates fds of deleted files and directories with ' (deleted)'; the oracle classifies such removal paths as Unmatched::DeletedDirectory and logs them.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
