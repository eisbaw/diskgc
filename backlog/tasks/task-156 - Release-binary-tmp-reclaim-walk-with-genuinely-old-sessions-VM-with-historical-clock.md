---
id: TASK-156
title: >-
  Release-binary /tmp reclaim walk with genuinely old sessions (VM with
  historical clock)
status: To Do
assignee: []
created_date: '2026-10-24 23:17'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-65: no container test runs the release binary through a real /tmp reclaim past the 1h floor, because the release binary has no test clock offset and rootless containers cannot backdate ctime. Use a VM with a historical clock (like vm-historical-ctime) or a test that waits; relates to TASK-154.
<!-- SECTION:DESCRIPTION:END -->
