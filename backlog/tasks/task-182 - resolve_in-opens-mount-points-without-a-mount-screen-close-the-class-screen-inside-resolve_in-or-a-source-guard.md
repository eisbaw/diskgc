---
id: TASK-182
title: >-
  resolve_in opens mount points without a mount screen: close the class (screen
  inside resolve_in, or a source guard)
status: Done
assignee: []
created_date: '2026-10-25 05:33'
updated_date: '2026-10-26 06:37'
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

2026-10-26 t182 (branch t182: 10eb58f, 11c8a8d, 792bd53; not merged; base master 0f4a2ec). One screen: resolve_in/resolve run screen_mount_chain first and return ResolveError::Screened{path, why} (skip_reason: networked->unsupported_fs, else unsafe_path) without opening anything. Opt-out resolve_unscreened (pub(crate)) used only by executor/preflight.rs measure (PATH); guard only_path_skips_the_mount_screen. Hand screens removed (executor preflight_roots + overlap copy, judge check_root, cargo resolve_root, invocation root_finding, collapse reach_site); one is_mount_point. Newly screened places listed in 10eb58f's message. Gate: just check exit 0, flake check --no-build 0, e2e-clippy/e2e-tests 0; e2e-one mount/FUSE/root set (23 tests) + CI journey PASS at 11c8a8d; dead-FUSE rerun PASS at 792bd53. Review: mped-architect ship, no blockers. Remaining class: Host::executable_file (TASK-241).
<!-- SECTION:NOTES:END -->
