---
id: TASK-9
title: >-
  cargo-target kind: discovery, lock allowlist, last build, eligibility, process
  heuristics
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 03:33'
labels:
  - wave-1
  - irreversible
  - safety
dependencies:
  - TASK-5
  - TASK-6
  - TASK-8
  - TASK-14
  - TASK-51
references:
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Cargo targets are the biggest cache on the target machines. Implement discovery (CACHEDIR.TAG plus a profile dir with a known lock file, depth limit, no symlinks or mounts, network filesystems refused), the lock allowlist and unsupported_layout, the last-build measure, empty targets, and the process heuristics (exe or cwd in the target, cargo* in the project dir or CARGO_TARGET_DIR in the initial environment). The reclaim protocol is its own task. See docs/safety.md#cargo-target.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A profile dir with an unknown .cargo-*lock* file is skipped as unsupported_layout
- [ ] #2 Discovery finds targets by CACHEDIR.TAG plus a known lock file, refuses network filesystems, never follows symlinks or crosses mounts (model and container tests)
- [ ] #3 A running test binary, a cargo process in the project dir, and CARGO_TARGET_DIR in a process's initial environment each make the target in_use (container tests)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-3: Discovery must reach roots with diskgc_core::resolve::resolve (deepest mount, statx-confirmed) and descend with open_beneath (NO_SYMLINKS|NO_XDEV + mount-id check). The mount's fs_type for the network-filesystem refusal is on Resolved.mount (MountEntry from mountinfo). Stat fields are Option (mask-checked); a None mtime on a last-build check is unanswerable, so fail closed.

Forward-carried from task-3 round 5: proc_environ returns Failed, not an empty Environ, for a live process whose environment is empty (env -i); the empty read cannot be told apart from one bound to a dead old leader during a non-leader exec. So the CARGO_TARGET_DIR initial-environment heuristic gets Failed for such processes; treat it per task-6's rule for unreadable processes (conservative: in_use).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
