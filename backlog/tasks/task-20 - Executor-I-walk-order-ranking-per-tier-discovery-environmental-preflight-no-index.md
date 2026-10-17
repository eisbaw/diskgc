---
id: TASK-20
title: >-
  Executor I: walk order, ranking, per-tier discovery, environmental preflight
  (no index)
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 12:50'
labels:
  - wave-2
  - irreversible
  - safety
dependencies:
  - TASK-19
references:
  - docs/architecture.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the walk decides order; policy order is a core promise. Extend the executor slice from the minimal ensure: groups in order, ranking ordered/random with seed, per-tier discovery before leaving a tier, environmental preflight of all tiers before the first deletion, misconfigured tiers excluded while others run, stop at the observed goal. Deadlines and signals are the Executor II task.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A pressure tier never runs while an earlier tier has discoverable eligible candidates (model test)
- [ ] #2 A misconfigured tier is excluded and reported while other tiers still run
- [ ] #3 Trace assertion: every tier's environmental preflight runs before the first deletion
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-5 (deletion engine): engine outcomes for the executor: a refused candidate (WalkStop::Skipped) never touched anything; End::Changed/Failed may be partial (removed_entries > 0), so re-sample space after every candidate whatever the outcome; End::GateRefused must stop the run (never retried, never counted as a skip). Deadlines: pass the run/tier deadline to both walk and delete; they check it before every statx/listing and every unlink (cooperative; a blocked syscall overruns).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
