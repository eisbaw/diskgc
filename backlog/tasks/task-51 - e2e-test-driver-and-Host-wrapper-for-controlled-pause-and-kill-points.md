---
id: TASK-51
title: e2e test driver and Host wrapper for controlled pause and kill points
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-16 23:54'
labels:
  - wave-1
  - testing
dependencies:
  - TASK-2
  - TASK-3
  - TASK-59
references:
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: wave-1 container tests need a diskgc process before the ensure command exists, and race tests 5 and 6 plus the replace-between-walk-and-unlink test must stop it at an exact call. Build the e2e-only test driver (a small binary over diskgc-core and LinuxHost that runs single operations on a fixture: walk, delete, in-use check, Cargo discovery and protocol) and the test-only Host wrapper from docs/testing.md#controlled-pause-and-kill-points (pause or SIGKILL at call k, driven by the harness). Neither is part of the release build.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The driver runs a walk and a delete on a container fixture, and the harness pauses it at a chosen Host call and resumes it, and kills it at a chosen call
- [ ] #2 The release build is verified not to contain the driver or the wrapper (a test inspects the release package)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-2: (1) Binaries reach containers through e2eUnderTest in flake.nix: its $out/bin is bind-mounted read-only at /opt/diskgc-e2e/bin. Add the driver there; code changes never rebuild the base image. (2) under_test::run_as_tester blocks until exit and records strace; pause/kill control needs a spawn variant. Control files the uid-1000 driver must read or write belong on the scratch mount: /run/diskgc-e2e is root-only. (3) Container root has CAP_SYS_PTRACE, so it can read /proc of the driver and kill it.
<!-- SECTION:NOTES:END -->
