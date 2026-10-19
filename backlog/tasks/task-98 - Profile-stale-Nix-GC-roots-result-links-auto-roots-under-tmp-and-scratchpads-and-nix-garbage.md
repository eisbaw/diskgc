---
id: TASK-98
title: >-
  Profile: stale Nix GC roots (result links, auto roots under /tmp and
  scratchpads) and nix-garbage
status: To Do
assignee: []
created_date: '2026-10-19 12:19'
labels:
  - library
dependencies:
  - TASK-36
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: dead Nix store paths were only ~2 GiB, but 19-74 GiB of the 157 GiB store is held alive by stale auto GC roots: one old result link pins a whole 25.11 system closure (19 GiB with old generations), auto roots under /tmp (mostly agent scratchpads) and ~/.cache, direnv. Deleting a stale result symlink (an out-link nobody uses) then running the Nix GC is the reclaim. Needs: a profile for stale out-link symlinks that are registered auto roots (idle-gated, owned by the user, never the current system or profiles), then the nix-store kind's GC (task-36). Source: read-only gap investigation 2026-10-19 (scratchpad gap/summary.txt).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A profile finds result-style out-links registered under /nix/var/nix/gcroots/auto that point into the store and are idle, never system or user profile links (model tests)
- [ ] #2 Composed with the nix-store GC kind: a dry run reports the store bytes the GC would then free
- [ ] #3 Verified read-only on this machine
<!-- AC:END -->
