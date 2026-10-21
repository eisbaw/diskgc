---
id: TASK-108
title: Post-index property suite for index authority and adaptive dry-run differences
status: To Do
assignee: []
created_date: '2026-10-21 21:58'
labels:
  - wave-3
  - testing
dependencies:
  - TASK-28
  - TASK-29
  - TASK-24
references:
  - docs/testing.md#3-property-tests
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Once the index exists, generate stale, partial and corrupt index states and compare every deletion with the same run using a correct index. Add generated dry-run/run properties for indexed and best-fit ordering; explain each permitted difference due to overlap or adaptive re-pick. This is the index-dependent part of the original TASK-24 AC3, outside the 0.1 ensure MVP.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every generated deletion with a stale, partial or corrupt index passes deletion-time checks and is refused whenever a correct-index run would refuse it
- [ ] #2 For best-fit and overlapping candidates, each dry-run/run difference has a recorded missing-child or adaptive-repick reason; unexplained differences fail
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
