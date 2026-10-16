---
id: TASK-13
title: >-
  Hardening wave 1: deletion path deep review, mutation testing, fd and depth
  limits
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 23:54'
labels:
  - wave-1
  - hardening
dependencies:
  - TASK-12
  - TASK-52
  - TASK-58
  - TASK-60
references:
  - docs/testing.md
  - docs/safety.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the wave-1 surfaces (host primitives, deletion engine, in-use checks, Cargo protocol) are now stable and are the hardest to walk back. Deep review (mped-architect plus a safety reviewer), cargo-mutants on the safety and executor modules (a surviving mutant in a safety check is a test bug), deep/huge tree limits, doc sync of docs/safety.md, plus every finding deferred during wave 1.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Deep review findings fixed or filed with an owner
- [ ] #2 docs/safety.md matches the implemented protocols (reviewed line by line against the code)
- [ ] #3 cargo-mutants (workspace tests) on diskgc-core and on diskgc-linux path resolution, mountinfo and /proc code: no surviving mutants in safety checks, or each justified; every safety check has a model test
- [ ] #4 Re-plan gate: if the race tests changed the Cargo protocol, task-9 and task-53 are re-reviewed under the deep gate and tasks 16, 22 and 33 re-checked; if the race tests cannot pass, cargo-target stays planned and the journey is rescoped
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: 'just mutants' is a placeholder (prints NOT RUN, exits 0). Replace it with the cargo-mutants invocation.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
