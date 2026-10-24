---
id: TASK-131
title: 'Owner PID: a live thread id in an rc name is in_use, not producer_unknown'
status: To Do
assignee: []
created_date: '2026-10-24 07:49'
labels:
  - liveness
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-112 step 4's host dry run (2026-10-24, Linux 7.2.9): 59 of 125,215 nix-shell-rc candidates were producer_unknown because the pid in the name is a live non-leader thread id. /proc lists only thread-group leaders, so the NSpid table misses it, and pidfd_open(n, 0) answers ENOENT (EINVAL before Linux 6.9), not ESRCH. Fail-closed today (kept), but under the wrong reason. Fix at the probe primitive (open_pidfd, crates/diskgc-linux/src/host.rs): on ENOENT/EINVAL retry pidfd_open(n, PIDFD_THREAD); success means a live task carries the number -> Ok(true) -> in_use (as a reused pid); if PIDFD_THREAD is unsupported (< 6.9) keep producer_unknown. No /proc/n/status Tgid read (races, hidepid). Model: a 'thread carries the number' fact plus a vector; Linux test with a real thread. Docs: safety.md Owner PID step 5 sentence (TASK-126 reference).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A live non-leader thread id in an rc name is in_use (owner_pid) on kernels with PIDFD_THREAD, producer_unknown otherwise, never eligible
- [ ] #2 Model vector and a Linux test with a real thread pin it; mutation bites
<!-- AC:END -->
