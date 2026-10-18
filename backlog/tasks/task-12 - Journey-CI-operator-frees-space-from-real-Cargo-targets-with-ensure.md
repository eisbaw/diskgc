---
id: TASK-12
title: 'Journey: CI operator frees space from real Cargo targets with ensure'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-18 05:47'
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
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
AC #3 (the same journey on a Debian image with the static binary) moved to task-63, the Debian / static-binary e2e variant, which depends on this task (backlog restructuring for the 0.1 ensure MVP, 2026-10-18).

From task-10 (for the CI-operator journey): the runnable command is diskgc ensure --min-free 50G [--target-free 80G] --select 'profile:cargo-target && filter:idle-1d' --roots "$CI_BUILDS_DIR" "$CI_BUILDS_DIR" [--json]. Needs /run/diskgc/lock (exit 2 prints the two tmpfiles lines); PATH and --roots absolute; a config is optional with --select (version: 1 with profiles only; protect/owners refused, groups ignored). Human mode is silent when nothing was needed; --json always one object. Exit codes 0/1/2/3/75/124 implemented (130/143 not: task-54). Not yet: config tiers, unions, !filter/!profile, other kinds, --allow-root, index (no 'index' field), aggregated skips. A development build only reclaims inside the test containers.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
