---
id: TASK-259
title: >-
  watch: check PATH's device inside the cycle's own resolution, not just before
  it
status: To Do
assignee: []
created_date: '2026-10-31T00:59:00Z'
labels:
  - watch
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-32: watch re-resolves PATH right before each cycle and refuses the cycle (host error) when PATH is on another device than at start (a mount stacked on PATH). The executor then resolves PATH again for its own measurement; a mount made between the two is not seen by watch's check, so that cycle measures and reclaims on the new filesystem's terms (roots on other filesystems are still screened; deletion checks are unchanged). Documented as a limit in reference.md (watch). Closing it means passing the expected device into executor::Request (58 construction sites today) or measuring through watch's held fd.
<!-- SECTION:DESCRIPTION:END -->
