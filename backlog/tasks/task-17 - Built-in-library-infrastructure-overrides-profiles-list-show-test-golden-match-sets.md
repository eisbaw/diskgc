---
id: TASK-17
title: >-
  Built-in library infrastructure: overrides, profiles list/show/test, golden
  match sets
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:50'
labels:
  - wave-2
  - config
dependencies:
  - TASK-16
references:
  - docs/reference.md#built-in-profile-library
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the library must grow without silently changing meaning. Implement override: true for shadowing built-ins, the profiles list/show/test commands, examples running against an in-memory model, and per-built-in golden match sets over fixtures that fail CI on any change unless the profile was renamed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Shadowing a built-in without override: true is a load error
- [ ] #2 Changing a built-in's pattern without renaming fails the golden-set test (proved on a scratch profile)
- [ ] #3 profiles test exits 1 on a failing example
- [ ] #4 Golden sets also cover the profiles shipped in wave 1 (cargo-target and the idle-* filters)
- [ ] #5 profiles test matches examples on paths in diskgc-core (it does not depend on the test-only model crate)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
