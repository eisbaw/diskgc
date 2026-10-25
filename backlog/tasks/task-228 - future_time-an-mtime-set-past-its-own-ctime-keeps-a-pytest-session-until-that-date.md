---
id: TASK-228
title: >-
  future_time: an mtime set past its own ctime keeps a pytest session until that
  date
status: To Do
assignee: []
created_date: '2026-10-25 17:10'
labels:
  - shipping
  - policy
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in TASK-227 (2026-10-25). The 331 future_time sessions on the laptop /tmp (TASK-157) are exactly the 331 pytest sessions holding files whose mtime is 2027-01-15T08:00:00Z = epoch 1,800,000,000, a fixed future time a test suite sets with utime (files like test_*/.arch/waves/*/node.yaml and .arch/events/*.yaml); their ctimes are 2026-10-18..23, all past. Not clock skew, not extraction, not a diskgc bug: diskgc takes newest change = max(mtime, ctime) and a future newest change is an unknown age (safety.md, idle filters), so future_time is the right reason and fails closed. Cost: these sessions are kept until 2027-01-15 plus the idle window, never reclaimed by pytest-tmp before then. Option to decide (policy, safety review): an mtime later than the same inode's ctime can only come from an explicit utime (the kernel sets ctime on every change, including utime itself), so the entry's last change is its ctime; keep future_time only when the ctime itself is in the future (clock skew). Evidence command (read-only): find each pytest-of-* session with -newermt now -o -newerct now; mtime 2027-01-15 on 58,363 entries, ctime none in the future. Not a 0.1 blocker.
<!-- SECTION:DESCRIPTION:END -->
