---
id: TASK-2
title: Container e2e harness with deletion oracle and safety interlock
status: To Do
assignee: []
created_date: '2026-10-16 22:39'
updated_date: '2026-10-16 22:49'
labels:
  - wave-1
  - foundation
  - testing
dependencies:
  - TASK-1
references:
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: destructive tests must never touch the live system, and every later e2e test needs a harness. Build the rootless-Podman harness per docs/testing.md (Container setup, Safety interlock): one container per test with --cap-add SYS_ADMIN and --network=none, harness as container root, diskgc run as uid 1000, other owners via mapped subuids; a base image with toolchains, tools, markers and the tmpfiles-provisioned host lock, tagged by its Nix store hash and reused; binaries in a thin layer; the deletion oracle (whole-container snapshot by path, device, inode and birth time, plus an strace record); the e2e (fast) and e2e-full gates with per-test timing.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->



## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 just e2e builds the base image once and reuses it when only code changed (measured: a code-only change does not rebuild the base)
- [ ] #2 diskgc runs as uid 1000 inside the container; a test proves it cannot read a mode-000 directory owned by another mapped uid
- [ ] #3 The deletion oracle catches a deleted object outside the eligible set and a replaced one (same path, new inode, e.g. a recreated lock file), proved with deliberately wrong fixtures; the strace record agrees
- [ ] #4 The interlock refuses outside a container (proved with a do-nothing canary binary); a test touching a path outside its scratch mount aborts
- [ ] #5 Per-test durations are reported; just e2e fails when it exceeds its 5-minute budget; just e2e-full exists for the slow suites
<!-- AC:END -->
