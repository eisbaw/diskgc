---
id: TASK-231
title: >-
  Index: finder-set id u32 exhaustion under a watch pinned at its budget; lump
  steps walk the whole index
status: To Do
assignee: []
created_date: '2026-10-25 17:22'
labels:
  - index
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-28 re-review: finder-set ids are never reused (u32), so a watch that stays at its budget would eventually exhaust them and crash (far off); each lump step costs a walk of the whole index. Address before long-running watch soak tests (TASK-34).
<!-- SECTION:DESCRIPTION:END -->
