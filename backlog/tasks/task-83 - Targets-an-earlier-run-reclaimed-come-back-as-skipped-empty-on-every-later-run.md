---
id: TASK-83
title: >-
  Targets an earlier run reclaimed come back as skipped (empty) on every later
  run
status: To Do
assignee: []
created_date: '2026-10-19 02:14'
updated_date: '2026-10-19 02:54'
labels:
  - cli
  - ux
  - wave-2
dependencies: []
references:
  - docs/safety.md
  - diskgc-e2e/src/journey_suite.rs
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the CI operator journey (task-12). After a run reclaims a Cargo target, its profile directories hold only lock files, and every later ensure lists it in `skipped` with reason `empty` (stderr: "skip empty: /tmp/builds/idle-1/target: its profile directories hold nothing but lock files"). On a persistent runner almost every target goes through this state, so `skipped` grows with every job: the journey's second run reported 4 skipped for 2 targets actually kept, and the human summary says "5 skipped" for 2 kept + 3 emptied. A dashboard counting skips, or an operator reading "skipped", is misled; the example hook (docs/ci-pre-job.sh) has to subtract `"reason":"empty"` itself.

docs/safety.md (cargo-target, Discovery, idleness, eligibility: Empty) says such a target "is `empty` and not re-listed on every run", which the binary does not do (no index yet: it is listed, and reported, every run). Decide what the contract is (not reported; reported apart from skips; reported only in --dry-run plans) and make safety.md, reference.md (Output) and the binary agree. Interacts with the JSON contract freeze (task-26).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 safety.md, reference.md and the binary agree on whether and where an empty target is reported
- [ ] #2 A target reclaimed by one ensure does not count as skipped in the next one's summary, or is clearly separated (journey asserts it)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decide at the contract freeze (task-26 depends on this task). If the decision is that "not re-listed on every run" is a promise of the index era (task-28 onwards), qualify that sentence in safety.md (Empty) now, so it does not describe behaviour the 0.1 binary lacks.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
