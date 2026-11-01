---
id: TASK-287
title: >-
  watch snapshot guard frozen at start: one failed first sample disables
  --min-free protection of snapshot writes
status: To Do
assignee: []
created_date: '2026-11-01T05:37:00Z'
updated_date: '2026-11-01T19:50:00Z'
labels:
  - watch
  - correctness
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. Codex 4 + mped SHOULD 2, one event: watch::start sets guard_min = goal.resolved.map(..); if the first statvfs fails (EIO) and the goal is a percentage (or byte thresholds), it resolves later in run but Writer.guard stays None for the life of the watch; snapshot writes then ignore --min-free. Fix: set the guard when the goal resolves (or pass the goal to write) and defer guarded writes while unresolved; model test.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: in progress in tmp/worktrees/t287 (branch t287-small-fixes).
<!-- SECTION:NOTES:END -->
