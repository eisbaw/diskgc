---
id: TASK-295
title: >-
  index_authority property flakes: the busy-host breaker makes refusals
  order-dependent
status: Done
assignee: []
created_date: '2026-11-01T12:37:00Z'
updated_date: '2026-11-02T14:01:00Z'
labels:
  - test
  - index
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the TASK-267 implementer (2026-11-01): index_authority::the_index_is_never_an_authority failed once during just check; replayed deterministically and fails identically on master 6e0559d. The busy-host breaker (three in_use_unknown in a row skip the rest of the tier) makes refusals depend on candidate order; the corrupted index's reordering lets z4x (not_idle) be checked before the breaker trips. No deletion; a property flaw. Fix: the property compares refusal sets modulo breaker skips (or disables the breaker in that property), and commit the regression seed. Replay was in tmp/worktrees/t267/tmp/t267-replay (removed with the worktree; the seed is in tmp/t267-index-authority-failure.log there - copy it before removal).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Evidence preserved at tmp/evidence-t267/ (failure log + replay).

2026-11-02: in progress in tmp/worktrees/t295 (branch t295-test-flakes).

2026-11-02: merged (ff), test-only.
<!-- SECTION:NOTES:END -->
