---
id: TASK-45
title: NixOS module with build-time check
status: Done
assignee: []
created_date: 2026-10-16 22:41
updated_date: '2026-10-31T23:00:00Z'
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
- [x] #1 An invalid config fails nix flake check evaluation
- [x] #2 Generated unit and tmpfiles rules are asserted on in evaluation tests
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: merged (ff) at e72c388 after one review round (mped + Codex). Limits: a reclaiming instance's deletion and ensure starting from a tally-only snapshot are not shown in the VM (need a release package with watch, TASK-276/280).
<!-- SECTION:NOTES:END -->
