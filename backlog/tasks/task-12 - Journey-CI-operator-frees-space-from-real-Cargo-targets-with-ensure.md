---
id: TASK-12
title: 'Journey: CI operator frees space from real Cargo targets with ensure'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:49'
labels:
  - wave-1
  - journey
dependencies:
  - TASK-11
references:
  - PRD.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: exercise the product as a user end to end before building more. In a container with several real Cargo projects (some idle, one building, one with a running test binary), run diskgc ensure from a pre-job-style script as a CI operator would; read the human output and the JSON.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The busy and running targets survive, idle ones are deleted, the deletion oracle agrees
- [ ] #2 Findings (confusing output, missing information, surprises) are filed as tasks, not fixed silently
- [ ] #3 The same journey also runs on a Debian image with the static binary
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
