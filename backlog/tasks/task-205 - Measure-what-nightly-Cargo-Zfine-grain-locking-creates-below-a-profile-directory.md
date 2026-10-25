---
id: TASK-205
title: >-
  Measure what nightly Cargo -Zfine-grain-locking creates below a profile
  directory
status: To Do
assignee: []
created_date: '2026-10-25 09:15'
labels:
  - hardening
  - safety
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up of TASK-167 (mped-architect note). diskgc refuses any .cargo-*lock* name at any depth in a walk; a future per-unit lock named outside that pattern (e.g. 'lock', '.lock') would be recorded and unlinked, breaking 'no Cargo lock file is ever deleted'. The vector's .cargo-unit-lock is hypothetical. Measure the real names with the Cargo measurement/race suites on the pinned nightly with fine-grain locking, and add a vector with the real name.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The lock files a pinned nightly with fine-grain locking creates are measured and recorded in docs/safety.md Lock files
- [ ] #2 A model vector uses the real name; the walk or judge_layout refuses or takes it
<!-- AC:END -->
