---
id: TASK-24
title: >-
  Property suite: never-delete invariants, exit-code precedence, dry-run
  agreement
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:50'
labels:
  - wave-2
  - testing
dependencies:
  - TASK-23
references:
  - docs/testing.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: invariants must hold for inputs nobody thought of. Implement the property tests in docs/testing.md#3-property-tests (call-trace invariants, owner authorisation, exit-code precedence, narrowed dry-run agreement with explained differences), with shrinking to saved YAML regressions.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The property suite runs in just check with few cases and in just soak with many
- [ ] #2 A planted bug (deleting one unchecked entry) is caught and shrunk to a small regression file
- [ ] #3 Every property listed in docs/testing.md#3-property-tests is implemented
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
