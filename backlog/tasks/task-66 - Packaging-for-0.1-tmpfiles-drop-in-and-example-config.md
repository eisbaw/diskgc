---
id: TASK-66
title: 'Packaging for 0.1: tmpfiles drop-in and example config'
status: To Do
assignee: []
created_date: '2026-10-17 23:55'
labels:
  - wave-2
  - deployment
dependencies:
  - TASK-15
references:
  - docs/safety.md
  - docs/architecture.md#deployment
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: 0.1 ships as a static binary with a tmpfiles drop-in and an example config (PRD Scope, Releases). Pulled out of task-44: the tmpfiles lines for /run/diskgc and /run/diskgc/lock (docs/safety.md, Host lock) and docs/example.yaml only; the systemd watch unit stays in task-44.

Carried from task-44's note: the e2e base image bakes /run/diskgc (0755 root) and /run/diskgc/lock (0644 root) by hand (fakeRootCommands of e2eBaseImage in flake.nix). Once the shipped drop-in exists, apply it in the harness with systemd-tmpfiles --root=<scratch> --create (docs/testing.md, Shipped files) and drop the hand-written copy, so there is one source.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The drop-in creates /run/diskgc/lock as PRD.md and docs/safety.md (Host lock) describe (systemd-tmpfiles --root=<scratch> --create, container test)
- [ ] #2 The example config passes diskgc check
- [ ] #3 Both are installed by the release package
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
