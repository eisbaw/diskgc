---
id: TASK-42
title: 'Journey: AI-authored profile from survey to ensure'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:51'
labels:
  - wave-4
  - journey
dependencies:
  - TASK-41
  - TASK-35
  - TASK-36
references:
  - docs/reference.md#user-profiles
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the intended authoring workflow end to end. Run survey --json on a container /tmp with an unknown kind of cruft, let an AI agent draft a profile with examples from real paths, run profiles test and survey --list --select, review the list as a human would, then use it in a tier.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Friction in the workflow is filed as tasks
- [ ] #2 The drafted profile passes profiles test, and the human review of survey --list finds no unexpected path (review recorded in the task)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
