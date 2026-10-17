---
id: TASK-54
title: >-
  Executor II: cooperative deadlines, overruns, signals, child process groups,
  re-sampling
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-17 08:25'
labels:
  - wave-2
  - irreversible
  - safety
dependencies:
  - TASK-20
references:
  - docs/architecture.md#the-reclaim-walk
  - docs/safety.md#deadlines
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: split from the executor task; deadlines and signals decide what happens mid-delete. Implement cooperative deadlines with the overruns report, SIGINT/SIGTERM handling (no new candidate, child process group SIGTERM then SIGKILL after 10 s), host-lock inheritance by children with close-only release, and the up-to-2-s re-sampling after an action.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Exit codes 75, 124, 130 and 143 are produced deterministically in model tests with the virtual clock
- [ ] #2 A blocking operation that exceeds the deadline is listed in overruns and no new work starts after it (model test)
- [ ] #3 One container test: SIGTERM mid-reclaim exits 143 and leaves no half-processed candidate unreported
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-3: LinuxHost::flock_wait polls LOCK_NB with sleeps (10-100 ms) and handles no signals. The SIGTERM-interruptible host-lock wait (reference.md, watch) needs a signal flag checked between polls, or a port-level cancellation. Host::now() is CLOCK_BOOTTIME.

From task-14: ModelHost has a virtual clock (now starts at UPTIME_AT_START; sleep, flock_wait and advance events move it; timed events at {elapsed: d}). Signals are not modelled because the port has no signal operation yet; when this task adds one, add a deliver-signal event action in diskgc-model load.rs Action.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
