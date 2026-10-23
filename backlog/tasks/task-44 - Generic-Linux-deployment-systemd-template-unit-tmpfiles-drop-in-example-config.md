---
id: TASK-44
title: >-
  Generic Linux deployment: systemd template unit, tmpfiles drop-in, example
  config
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-23 16:19'
labels:
  - wave-5
  - deploy
dependencies:
  - TASK-43
  - TASK-119
references:
  - docs/architecture.md#deployment
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: most users are not on NixOS. Ship a systemd template unit for watch (user and system variants), the tmpfiles drop-in for the host lock, and docs/example.yaml consistent with docs/reference.md.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 systemd-tmpfiles --root=<scratch> --create creates the lock file as specified (container test)
- [ ] #2 example.yaml passes check
- [ ] #3 systemd-analyze verify passes on an instance of the template unit with the binary present
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-2: the e2e base image currently bakes /run/diskgc (0755 root) and /run/diskgc/lock (0644 root) per the two tmpfiles lines in docs/safety.md (fakeRootCommands of e2eBaseImage in flake.nix). sandbox_trivial checks owner and mode. Once the shipped tmpfiles drop-in exists, apply it in the harness with systemd-tmpfiles --root=<scratch> --create (docs/testing.md, Shipped files) and drop the hand-written copy, so there is one source.

Pulled out into task-66 for the 0.1 ensure MVP (2026-10-18): the tmpfiles drop-in for /run/diskgc/lock and the example config (ACs #1 and #2 here, and the forward-carried harness note above). What stays here: the systemd template unit for watch (user and system variants, AC #3).

Orchestrator 2026-10-23 (design audit): now depends on TASK-119 (unit/module design).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
