---
id: TASK-10
title: 'Minimal ensure: thresholds, host lock, exit codes, --select/--roots for Cargo'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:54'
labels:
  - wave-1
  - cli
  - irreversible
dependencies:
  - TASK-53
  - TASK-7
  - TASK-50
references:
  - docs/reference.md
  - docs/safety.md#host-lock
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the race tests and the first journey need a runnable binary. Build this as the first slice of the real executor in diskgc-core (later tasks extend it, nothing is thrown away): ensure --min-free/--target-free PATH with --select and --roots, config lookup with minimal version: 1 loading (with --select a config is optional and built-ins are used), size and duration parsing, statvfs measurement, the provisioned host lock with inheritance and close-only release, refusal to run as root without --allow-root, the sandbox gate, and the JSON fields that exist at this stage exactly as specified in docs/reference.md.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->



## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Already above --min-free: exit 0 without taking the lock
- [ ] #2 Missing /run/diskgc/lock: exit 2 printing the two tmpfiles lines
- [ ] #3 Exit codes 0, 1 and 2 are produced by container tests (75, 124, 130 and 143 are model-tested in the Executor II task)
- [ ] #4 Running as uid 0 without --allow-root exits 2; bare numbers and month durations are usage errors
- [ ] #5 Every JSON field and enum value emitted matches docs/reference.md (schema and enum test; snapshots come at the contract freeze)
- [ ] #6 A reclaiming ensure outside a container exits 2 naming the sandbox gate (test run outside the harness)
<!-- AC:END -->
