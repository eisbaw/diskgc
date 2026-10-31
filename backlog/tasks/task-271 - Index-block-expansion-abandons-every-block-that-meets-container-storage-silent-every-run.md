---
id: TASK-271
title: >-
  Index block expansion abandons every block that meets container storage
  (silent, every run)
status: To Do
assignee: []
created_date: '2026-10-31T03:39:00Z'
labels:
  - index
  - performance
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. mped SHOULD 1: Reclaim::expand treats any found.refused as incomplete; storage refusals land there while the tally counts a storage-only listing complete (nothing_refused_but_storage), so the block is never evicted and a $HOME-rooted tier with rootless Podman pays a full restricted discovery per run for zero members. Speed only. Fix: test completeness with found.nothing_refused_but_storage(); log one line when an expansion is abandoned.
<!-- SECTION:DESCRIPTION:END -->
