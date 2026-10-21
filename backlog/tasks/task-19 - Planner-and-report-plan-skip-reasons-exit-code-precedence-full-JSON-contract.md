---
id: TASK-19
title: >-
  Planner and report: plan, skip reasons, exit-code precedence, full JSON
  contract
status: Done
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-21 11:18'
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
- [x] #1 Aggregation kicks in exactly at the documented thresholds
- [x] #2 Schema and enum tests cover every status value and every skip reason (insta snapshots are added at the contract freeze)
- [x] #3 Every action produces exactly one log line and one actions entry (test)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-8: docs/reference.md's skipped reasons gained future_time (an idle_for filter whose measured last use lies in the future: age unknown, skipped under filter: and !filter: alike). profile::Unknown::skip_reason() gives future_time / unsupported_fs; engine::Reason covers the walk only, so the planner's full reason enum must include future_time and not_idle.

2026-10-21 report contract slice: JSON groups skipped candidates at 21+ per (tier, profile, reason), retaining count and up to 20 distinct examples; first 1,000 actions remain detailed and any remainder becomes one counted summary. Focused nix develop -c cargo test -p diskgc-core report::tests --locked: 10 passed, including exact 20/21 and 1000/1001 boundaries, full documented status/skip enum shape, null fields and exit precedence. AC3 one action -> one stderr log line and actions entry still needs executor/integration assertion. Full just e2e, parallel QA/MPED review and contract-freeze snapshots remain pending; DoD open.

2026-10-21 AC3 candidate: ModelHost test each_complete_or_partial_action_has_one_log_line_and_one_report_row runs completed and injected partial Cargo deletion. It compares each action callback line and JSON actions row by candidate and count; a missing line/row or misattribution fails. Focused nix develop -c cargo test -p diskgc-model --test ensure each_complete_or_partial_action_has_one_log_line_and_one_report_row --locked passed 1/1. Full just e2e and parallel QA/MPED review remain pending before DoD/commit.

Report aggregation/status/action-log contract passed the final nix develop -c just e2e gate (158/158 Nix image, 7/7 native Docker); parallel qa-test-runner and mped-architect reviews found no blockers. Contract-freeze snapshots remain scheduled under TASK-26.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
