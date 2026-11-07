---
id: TASK-348
title: >-
  A transient process whose read never settles ('no trustworthy read in 16
  rounds') is classed as a cause waiting does not end: the tier skips its rest
  (127,645 nix-shell-rc dirs)
status: Done
assignee: []
created_date: '2026-11-05T07:04:00Z'
updated_date: '2026-11-07T15:29:00Z'
labels:
  - dogfood
  - freshness
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Dogfood 2026-11-05 07:08-08:03, release-candidate bd68931 (with TASK-340), systemd-run --user unit diskgc-dogfood-rc2, ensure --json --timeout 30h --min-free 200G --select profile:nix-shell-rc /tmp, laptop load ~8-10 with other projects' cargo builds. TASK-340's back-off triggered correctly once ('3 candidates in a row could not be checked (the host is too busy): the tier backs off'). Later three skips in a row: 'pid 1857894 (rust-lld) cannot be inspected, so it may be using any candidate: read process 1857894: no trustworthy read in 16 rounds: no task of a complete listing was live' -> '(a cause waiting does not end, named in each skip): the rest of this tier is skipped unchecked' -> 127,645 kept in_use_unknown after 19,986 reclaimed. rust-lld is a short-lived linker of a concurrent build: its unreadability ends when it exits, so this is a busy-host symptom, not a long-lived uninspectable process (newgrp shell, setuid). Evidence: tmp/walks/2026-11-04-dogfood/a4-rc-real.{json,err} (err lines ~40059-40063). Do: classify an unsettled per-process read (process churning/exiting) as busy (back off) and keep only persistent uninspectability (EACCES/EPERM, non-dumpable, setuid, other user's) as skip-rest; or re-check the same pid after a back-off and skip-rest only if it is still alive and still unreadable. Model test from this log shape; prove it fails on master.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-05 orchestrator: implementer in tmp/worktrees/t348 (branch t348-transient-unreadable, from master 1b52de8d).

2026-11-07: cherry-picked onto the TASK-364 stack (c842655c, c24b9a89 on t364-busy-retry); merges with that stack's gate.

Merged in the stack ending 8fd85e87d0df6a586c50afcc8f6d9d8dddf4d2df (2026-11-07).
<!-- SECTION:NOTES:END -->
