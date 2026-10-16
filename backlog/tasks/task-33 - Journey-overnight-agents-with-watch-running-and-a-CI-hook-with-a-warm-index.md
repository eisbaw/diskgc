---
id: TASK-33
title: 'Journey: overnight agents with watch running, and a CI hook with a warm index'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
labels:
  - wave-3
  - journey
dependencies:
  - TASK-32
references:
  - PRD.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the motivating scenario end to end. In a container, run watch while simulated agents fill /tmp and build Cargo targets past the low threshold; separately run ensure in a pre-job hook against a snapshot written by watch --tally-only.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No writer hits ENOSPC while eligible cruft exists; time to first deletion is recorded
- [ ] #2 Findings are filed as tasks
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
