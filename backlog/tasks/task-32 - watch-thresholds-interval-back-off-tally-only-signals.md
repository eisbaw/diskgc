---
id: TASK-32
title: 'watch: thresholds, interval, back-off, --tally-only, signals'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-25 17:43'
labels:
  - wave-3
  - cli
dependencies:
  - TASK-31
  - TASK-117
references:
  - docs/reference.md#watch
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the laptop use case is a long-running service. Implement watch per docs/reference.md#watch: required --target-free, interval polling, reclaim below --min-free, back-off rules (exhausted/misconfig vs backend errors), --tally-only, SIGTERM exit 0, one log line per back-off.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Between the thresholds watch makes zero deletions (container test over time with the virtual clock in model)
- [ ] #2 Back-off timings follow the documented rules (model tests)
- [ ] #3 --tally-only never deletes (deletion oracle) and keeps the snapshot fresh
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Orchestrator 2026-10-23 (design audit): now depends on TASK-117 (watch machine interface design).

Design (TASK-117) settled in 2d6c1ba on branch watch-design: implement per docs/reference.md#watch and the watch scenarios in docs/testing.md; unit restart requirement for TASK-119 in architecture.md Deployment.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
