---
id: TASK-182
title: >-
  resolve_in opens mount points without a mount screen: close the class (screen
  inside resolve_in, or a source guard)
status: To Do
assignee: []
created_date: '2026-10-25 05:33'
updated_date: '2026-10-25 20:18'
labels:
  - safety
  - mounts
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-178 review. resolve_in has many callers (fs_kind judge/plan/app/liveness/source_anchor, executor overlap, docker_backend via resolve); some screen with screen_mount_chain first, others do not visibly, and TASK-178 was one that forgot. Options: move the screen into resolve_in with an explicit opt-out used only by PATH resolution; or a source-guard test (like procfs_is_reached_only_through_the_held_fd) requiring each call site to be screened or allowlisted. Cheap detector: a generator that sprinkles dead: true mounts (ModelHost, TASK-178) over existing vectors. Also ModelHost executable_file uses fixture lookup and is not held to dead mounts.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
COMPASS round 4 (2026-10-25): raised: do after the 0.1 cut and before TASK-31.
<!-- SECTION:NOTES:END -->
