---
id: TASK-8
title: >-
  Profile model and embedded built-ins (cargo-target, idle filters) with roots
  fixed vs required
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:55'
labels:
  - wave-1
  - config
dependencies:
  - TASK-7
references:
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the first end-to-end path needs profiles to select Cargo targets. Implement the profile schema types (find/where/action/kind/examples), embedded built-in YAML (initially cargo-target and the idle-* filters), roots fixed vs required, and the idle_for measure per kind. The rest of the library comes in wave 4.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A roots: required profile without roots is a load-time error; roots on a fixed-root profile's tier is an error
- [ ] #2 Two idle filters in one term combine to the longest (unit test)
- [ ] #3 Built-in YAML is validated at build time (a broken built-in fails the build)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
