---
id: TASK-41
title: 'Library: embedded toolchains and build directories, verified'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:55'
labels:
  - wave-4
  - config
  - library
dependencies:
  - TASK-40
references:
  - docs/reference.md#built-in-profile-library
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Zephyr/west, Renode and PlatformIO leave large scratch on embedded developers' machines. Ship zephyr-tmp, west-build, renode-tmp, platformio-cache, cmake-build and pytest-cache, settling how profiles that find at any depth under a project root work without ** (explicit depths or one root per project).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each profile has examples, a golden set and verified_with
- [ ] #2 The profiles use the any-depth rule decided in the selection-engine task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
