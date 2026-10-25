---
id: TASK-227
title: >-
  A dry run that hits the deadline reports an empty plan; keep and report what
  it judged
status: To Do
assignee: []
created_date: '2026-10-25 16:44'
labels:
  - shipping
  - release-blocker
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-157 host measurements (2026-10-25): a first pytest-tmp dry run on the laptop's /tmp hit the default --timeout 10m and exited 124 with an empty plan and skipped list, discarding 5,834 judged sessions. Decision by mark-emulator (Mark may veto): don't throw judged work away - a dry run that hits the deadline reports every candidate it judged (plan rows), status deadline, exit 124; keep the 10m default; the release notes tell first runs to give more. Check the live run too: a reclaim that hits the deadline must report every action and skip so far. Also look at the 331 future_time sessions seen on the host (why: clock, filesystem timestamps?) and say whether that reason is right.
<!-- SECTION:DESCRIPTION:END -->
