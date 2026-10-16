---
id: TASK-9
title: >-
  cargo-target kind: discovery, lock allowlist, last build, eligibility, process
  heuristics
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:54'
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

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
