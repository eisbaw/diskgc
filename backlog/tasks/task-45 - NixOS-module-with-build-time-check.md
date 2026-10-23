---
id: TASK-45
title: NixOS module with build-time check
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-23 16:19'
labels:
  - wave-5
  - deploy
dependencies:
  - TASK-44
  - TASK-119
references:
  - docs/architecture.md#deployment
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: one deployment option among others. Module renders the YAML, ships the tmpfiles rule, runs check at build time, starts watch instances.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An invalid config fails nix flake check evaluation
- [ ] #2 Generated unit and tmpfiles rules are asserted on in evaluation tests
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
