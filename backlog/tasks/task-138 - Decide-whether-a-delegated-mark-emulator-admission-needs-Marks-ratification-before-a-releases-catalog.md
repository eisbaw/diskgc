---
id: TASK-138
title: >-
  Decide whether a delegated (mark-emulator) admission needs Mark's ratification
  before a releases catalog
status: To Do
assignee: []
created_date: '2026-10-24 18:43'
labels:
  - profiles
  - release
  - decision
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: TASK-122 admitted twelve verified heuristics as 'admitted by mark-emulator for Mark, 2026-10-24' and recorded the delegation in docs/safety.md (Producer liveness), with Mark's veto. Whether such an admission may enter a builtin.yaml releases: catalog without Mark's own ratification is open (safety.md names it). None of the twelve is in a catalog today. The sync test already parses the decider (golden.rs admission_recorded -> Decider::Mark | MarkEmulator), so the rule, once decided, is a few lines: e.g. a released built-in whose row's decider is MarkEmulator is a problem until a 'ratified by Mark, DATE' entry exists.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Mark's ruling recorded in docs/safety.md
- [ ] #2 If ratification is required, the sync test refuses a released built-in admitted only by mark-emulator
<!-- AC:END -->
