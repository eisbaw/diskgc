---
id: TASK-119
title: 'Design: systemd unit and NixOS module options; fix systemd-analyze doc drift'
status: To Do
assignee: []
created_date: '2026-10-23 16:19'
updated_date: '2026-10-23 21:01'
labels:
  - design
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Design before implementation (audit 2026-10-23 of docs vs PRD). Deliverable is docs text, reviewed by mped-architect, not code. No systemd unit, no NixOS module and no nixosModules flake output exist although architecture.md Deployment names them and Implementation lists nixosModules.default. docs/testing.md (~L1319) claims just check runs systemd-analyze verify; the justfile has no such step. Design: template unit instance naming, ExecStart, Restart, sandboxing options compatible with /proc and pidfd needs, user vs system service; NixOS module option schema and instance model. Fix or remove the testing.md claim.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Unit and module designs in docs/architecture.md Deployment with option names and instance model
- [ ] #2 testing.md systemd-analyze claim matches the justfile (recipe added or claim removed)
- [ ] #3 mped-architect design review GO recorded in notes
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mark's design decisions (2026-10-23, binding): Default shape: USER SERVICE BY DEFAULT (diskgc watch under systemd --user for the laptop owner: sees own processes, non-root), plus a SYSTEM TEMPLATE UNIT (instance per filesystem) as an option for CI runners and --allow-root use. The NixOS module must model both. Sandboxing options must not break /proc and pidfd needs; state which hardening is compatible.
<!-- SECTION:NOTES:END -->
