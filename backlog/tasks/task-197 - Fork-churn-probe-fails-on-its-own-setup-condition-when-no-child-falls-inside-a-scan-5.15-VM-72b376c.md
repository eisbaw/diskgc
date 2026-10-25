---
id: TASK-197
title: >-
  Fork-churn probe fails on its own setup condition when no child falls inside a
  scan (5.15 VM, 72b376c)
status: To Do
assignee: []
created_date: '2026-10-25 06:26'
labels:
  - test
  - flaky
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Async QA on 72b376c (2026-10-25): host_inuse_scan_settles_after_fork_churn in vm-pidfd-linux-5-15 exited 1 with 'no entire child spawn-and-wait interval fell within a measured in-use scan' (events_within_scan=0 at 0/1/4/8 workers). No safety violation: unsettled/blocked/checker_unsettled all false; it failed before the post-storm assertion. Passed in every earlier batch and in 4 other VMs and the fast suite in the same run; host was swapping (~23 GiB). Cause: the probe's overlapped_scan setup condition (diskgc-e2e/src/host_probe.rs) depends on timing. Fix the test design (sample until a full spawn-and-wait falls inside a scan, bounded; report 'not exercised' distinctly) rather than rerunning. Logs: tmp/e2e-logs-batch/e2e-72b376c.log; nix log of vm-pidfd-linux-5-15.
<!-- SECTION:DESCRIPTION:END -->
