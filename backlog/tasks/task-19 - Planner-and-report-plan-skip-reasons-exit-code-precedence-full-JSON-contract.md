---
id: TASK-19
title: >-
  Planner and report: plan, skip reasons, exit-code precedence, full JSON
  contract
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:55'
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

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
