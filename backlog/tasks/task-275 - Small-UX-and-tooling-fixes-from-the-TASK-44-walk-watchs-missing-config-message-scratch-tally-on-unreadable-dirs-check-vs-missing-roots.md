---
id: TASK-275
title: >-
  Small UX and tooling fixes from the TASK-44 walk: watch's missing-config
  message, scratch-tally on unreadable dirs, check vs missing roots
status: To Do
assignee: []
created_date: '2026-10-31T04:49:00Z'
labels:
  - ux
  - tooling
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-44 walk and review (2026-10-31): (1) watch with no configuration and no --select says 'ensure without --select needs one' (invocation prepare hardcodes the command name; pass it in). (2) just scratch-tally exits 1 on an unreadable directory in a worktree (tmp/worktrees/t242/tmp/system/dg-939022-0/searchable-only, a test fixture): report the path, mark the total partial, carry on. (3) diskgc check exits 0 on the example's missing root while watch/ensure exit 2 (by design: check needs nothing from the machine, for the NixOS build-time check); the install steps rely on check, so say it only warns on missing roots, or add a host-aware mode (cf. TASK-212).
<!-- SECTION:DESCRIPTION:END -->
