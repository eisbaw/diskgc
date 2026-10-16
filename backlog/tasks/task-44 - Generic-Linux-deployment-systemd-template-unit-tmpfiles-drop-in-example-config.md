---
id: TASK-44
title: >-
  Generic Linux deployment: systemd template unit, tmpfiles drop-in, example
  config
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:55'
labels:
  - wave-5
  - deploy
dependencies:
  - TASK-43
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

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
