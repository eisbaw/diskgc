---
id: TASK-270
title: >-
  Refactor: one ensure entry point (--select as a group of one) with a per-tier
  prepare seam; snapshot written on every exit after the sync tally
status: To Do
assignee: []
created_date: '2026-10-31T03:39:00Z'
labels:
  - refactor
  - index
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. mped SHOULD 3+4 (top refactor 1): ensure_inner and ensure_grouped_inner are ~800 lines of near-copies that drifted (action allowlist twice with different messages, command/Docker preflight twice and in different order, lock-timeout and met-after-lock logging only in one). index.finish runs only on the success tail, so a host-lock timeout, root preflight errors, protect enumeration errors or met-after-lock lose the tally's snapshot (PRD: each ensure that tallies leaves a snapshot). Fix: --select as a group of one with zero timeout; reclaimable(tier) + prepare_tier(...) -> Option<PreparedTier>; one exit after sync_tally that calls finish before the final sample; decide and pin by model test where a missing executable is reported. Unblocks TASK-36 (nix-store) and keeps watch on one path.
<!-- SECTION:DESCRIPTION:END -->
