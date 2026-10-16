---
id: TASK-16
title: >-
  Selection engine: find patterns, filters, overlap negation and protect, with a
  reference implementation
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:50'
labels:
  - wave-2
  - irreversible
  - config
dependencies:
  - TASK-15
references:
  - docs/reference.md#selection-grammar
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: what gets selected must equal the documented set semantics exactly. Write the reference implementation first, from the examples in docs/reference.md, then the engine: segment-wise find patterns (no **), filter evaluation, !filter, !profile and protect by overlap (ignoring the negated profile's where), parenthesised distribution. Decide here, before the schema freeze, how profiles that find at any depth under a project root work without ** (explicit depths or one root per project), and document it in docs/reference.md. Tests use user profiles in fixtures (the built-in library ships in wave 4).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Property test: eligible set equals the reference implementation for random profiles, expressions and trees
- [ ] #2 Property test: ! and protect never add a candidate; a || b equals b || a
- [ ] #3 profile:tmp-entry && !profile:claude-code-tmp excludes /tmp/claude-1000 as a whole (model test with both written as user profiles in the fixture)
- [ ] #4 The any-depth rule is decided and documented in docs/reference.md
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
