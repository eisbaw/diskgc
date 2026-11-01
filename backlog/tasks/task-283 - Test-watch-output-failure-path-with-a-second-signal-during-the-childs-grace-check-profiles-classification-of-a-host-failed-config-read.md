---
id: TASK-283
title: >-
  Test: watch output-failure path with a second signal during the child's grace;
  check/profiles classification of a host-failed config read
status: To Do
assignee: []
created_date: '2026-11-01T05:25:00Z'
labels:
  - watch
  - test
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the watch batch (TASK-257/272, merged 2026-11-01): (1) run_command_permitted's output/waitid/fcntl failure path with a second signal during the grace has no test (needs real signals during the grace; the container test covers the main TERM path); (2) check and profiles still report a host-failed config read (procfs reopen, EIO) as exit 2, unlike ensure/watch (exit 3 host): decide whether check (documented as depending on the configuration alone) should match.
<!-- SECTION:DESCRIPTION:END -->
