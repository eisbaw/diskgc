---
id: TASK-21
title: >-
  ensure: full CLI contract (config, --select/--roots, --dry-run, --json,
  --allow-root rules)
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:55'
labels:
  - wave-2
  - cli
dependencies:
  - TASK-20
  - TASK-54
references:
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: CI hooks and harnesses depend on the exact flags and exit codes. Complete ensure per docs/reference.md#ensure: config groups, --select/--roots validity rules, --dry-run plan, --json one object always, --allow-root (owners required, command actions refused), size and duration syntax, mount printed in every report.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every flag and rule in docs/reference.md#ensure has a test
- [ ] #2 --dry-run deletes nothing and takes no lock (deletion oracle + lock probe)
- [ ] #3 Bare numbers and month durations are usage errors
- [ ] #4 --allow-root: owners required and resolving to non-system users; the config file and every directory above it root-owned and not writable by others; command actions refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
