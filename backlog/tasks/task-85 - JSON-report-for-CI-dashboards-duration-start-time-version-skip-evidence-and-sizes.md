---
id: TASK-85
title: >-
  JSON report for CI dashboards: duration, start time, version, skip evidence
  and sizes
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
  - docs/reference.md
  - docs/ci-pre-job.sh
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the CI operator journey (task-12), reading the --json report as a fleet dashboard would. Input for the JSON contract freeze (task-26): every field added later is a contract change. What a dashboard needs and the report does not carry:
- how long the run took and when it started (only per-action duration_ms; the example hook measures itself with date +%s, 1 s resolution, and showed "after 0s");
- which diskgc produced it (no version field; a fleet runs several versions during a rollout);
- why a target was kept beyond the enum: `skipped` has reason `in_use` or `locked` only; the process (pid, command, the evidence) is only in the stderr progress line;
- how much space the kept targets hold (no size on `skipped`; `estimated_bytes` exists in `plan` only), so "could have freed X if the build had finished" cannot be shown;
- the age (last build) of what was reclaimed, to tune the idle filters (stderr logs it, as raw epoch seconds).

Decide at the freeze which of these belong in the contract (a field each, null when unknown), not in this task's code alone.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each listed item is decided at the contract freeze (task-26): added to reference.md Output with its type and null rule, or rejected with the reason recorded here
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
task-26 depends on this task: each item is decided at the freeze.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
