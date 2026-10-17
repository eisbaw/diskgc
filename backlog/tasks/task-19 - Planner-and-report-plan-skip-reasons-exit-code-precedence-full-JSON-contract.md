---
id: TASK-19
title: >-
  Planner and report: plan, skip reasons, exit-code precedence, full JSON
  contract
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 22:45'
labels:
  - wave-2
  - cli
dependencies:
  - TASK-18
references:
  - docs/reference.md#output
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the JSON report is consumed by agent harnesses and frozen once published. Implement the pure planner (index in, plan out) and the full report per docs/reference.md: status values, index block, plan, actions, skipped with aggregation (count/examples, 1,000-action cap), held_by_deleted_files, overruns, errors, null fields on usage errors; insta snapshots with redactions.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Aggregation kicks in exactly at the documented thresholds
- [ ] #2 Schema and enum tests cover every status value and every skip reason (insta snapshots are added at the contract freeze)
- [ ] #3 Every action produces exactly one log line and one actions entry (test)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-8: docs/reference.md's skipped reasons gained future_time (an idle_for filter whose measured last use lies in the future: age unknown, skipped under filter: and !filter: alike). profile::Unknown::skip_reason() gives future_time / unsupported_fs; engine::Reason covers the walk only, so the planner's full reason enum must include future_time and not_idle.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
