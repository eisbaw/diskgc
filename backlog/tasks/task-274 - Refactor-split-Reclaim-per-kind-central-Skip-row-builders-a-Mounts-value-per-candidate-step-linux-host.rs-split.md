---
id: TASK-274
title: >-
  Refactor: split Reclaim per kind, central Skip-row builders, a Mounts value
  per candidate step, linux host.rs split
status: To Do
assignee: []
created_date: '2026-10-31T03:39:00Z'
labels:
  - refactor
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. mped SHOULD 5-9 (top refactors 2+3): Skip rows hand-built in seven places (SkipEntry::candidate / tier_level); ~20 independent mountinfo reads in core, ~7 per candidate with protect (one Mounts value read once per step: mount_for, screen_chain, place_of, refuse_overlay); executor.rs imports union/negated/Docker helpers from the dry-run module plan.rs (move to executor/union.rs, docker_backend); executor.rs -> executor/reclaim/{mod,fs,cargo,docker,union}.rs; diskgc-linux host.rs (~6.6k lines): move command and Docker code out; Host trait (52 methods) into capability sub-traits; remove the unguarded public fs_kind::reclaim, reclaim_alternatives, cargo_target::reclaim (no-op policy) so a new kind cannot call them by mistake. Do after the one-entry-point refactor.
<!-- SECTION:DESCRIPTION:END -->
