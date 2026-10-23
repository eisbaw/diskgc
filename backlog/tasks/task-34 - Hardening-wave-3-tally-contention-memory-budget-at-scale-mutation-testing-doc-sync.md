---
id: TASK-34
title: >-
  Hardening wave 3: tally contention, memory budget at scale, mutation testing,
  doc sync
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-23 15:57'
labels:
  - wave-3
  - hardening
dependencies:
  - TASK-33
references:
  - docs/architecture.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: index and tally are stable now. Measure actual contention of the tally on a real disk outside the harness's tmpfs (in a container with a loop-mounted image if possible, else documented as a gap), memory budget on a million-entry fixture, cargo-mutants on the walk, docs/architecture.md sync, deferred findings.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Measured tally contention and memory are recorded in docs/architecture.md
- [ ] #2 No surviving mutants in the walk's stop and order logic, or each justified
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from TASK-70 (2026-10-23): the shipped static musl build's in-use scan costs 3-6x the wall time and ~10x the CPU of a glibc build on a real host (581 processes); filed as TASK-115. Scale measurements must name the build (musl static vs glibc) they ran.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
