---
id: TASK-27
title: 'Hardening wave 2: fuzzing, mutation testing, doc validation, deferred findings'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:50'
labels:
  - wave-2
  - hardening
dependencies:
  - TASK-26
references:
  - docs/testing.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: wave-2 surfaces are frozen; harden them once. cargo-fuzz on the grammar parser and the YAML loader, cargo-mutants on selection and executor, doc validation of docs/reference.md and docs/safety.md, every finding deferred during wave 2.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No surviving mutants in selection semantics, or each justified
- [ ] #2 Each fuzzer (grammar, YAML loader, mountinfo, maps, environ parsers) runs 30 minutes without crashes; crashes found become regression tests
- [ ] #3 fuzz and mutants recipes exist in the justfile
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
