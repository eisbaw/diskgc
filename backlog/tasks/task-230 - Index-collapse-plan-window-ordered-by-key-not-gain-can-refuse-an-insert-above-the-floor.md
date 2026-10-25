---
id: TASK-230
title: >-
  Index collapse: plan window ordered by key, not gain, can refuse an insert
  above the floor
status: To Do
assignee: []
created_date: '2026-10-25 17:22'
labels:
  - index
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-28 re-review (2026-10-25): the 1,024-step collapse plan window is ordered by key, not by gain, and a lump charges its union finder set as new even when it already exists. A crowded plan could stop above the low-water mark and refuse an insert above the floor, contradicting Mark's 'no refused insertion' decision. Not reproduced. Fix: order the window by gain (bytes freed per step), credit existing union sets, and add a property test that drives a crowded plan; do with TASK-29.
<!-- SECTION:DESCRIPTION:END -->
