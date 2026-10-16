---
id: TASK-40
title: 'Library: language and package caches, verified'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:55'
labels:
  - wave-4
  - config
  - library
dependencies:
  - TASK-39
references:
  - docs/reference.md#built-in-profile-library
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: per-tool caches need the tool's own cleaner pointed at the right directory. Ship pip-cache, uv-cache, go-build-cache, npm-cache, pnpm-store, yarn-cache, jest-cache, ccache, gradle-caches and maven-repo, each command explicitly naming the cache location.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each command action measurably cleans the directory diskgc measured (container test per tool)
- [ ] #2 Each profile has examples, a golden set and verified_with
- [ ] #3 Profiles whose tool cannot be verified offline in the harness (e.g. Gradle, Maven) stay planned, not shipped
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
