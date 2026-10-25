---
id: TASK-230
title: >-
  Index collapse: plan window ordered by key, not gain, can refuse an insert
  above the floor
status: To Do
assignee: []
created_date: '2026-10-25 17:22'
updated_date: '2026-10-25 18:53'
labels:
  - index
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-28 re-review (2026-10-25): the 1,024-step collapse plan window is ordered by key, not by gain, and a lump charges its union finder set as new even when it already exists. A crowded plan could stop above the low-water mark and refuse an insert above the floor, contradicting Mark's 'no refused insertion' decision. Not reproduced. Fix: order the window by gain (bytes freed per step), credit existing union sets, and add a property test that drives a crowded plan; do with TASK-29.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed on branch t29 in 346751c: the plan window keeps key order (small first, deepest first) but admits only steps that would shrink the index now; a lump's existing union set kept by units outside the step costs nothing; finder-set user counts kept per pass (asserted). Test a_crowded_plan_never_refuses_an_insertion_above_the_floor fails (NoRoom, 1,061 families) on the previous collapse.rs. Did not order the window purely by gain: that would pick shallow all-names steps first, against architecture.md 'Which site first' - open point for Mark.
<!-- SECTION:NOTES:END -->
