---
id: TASK-137
title: >-
  Container tests with the real pinned producers for the twelve admitted
  verified heuristics (TASK-122)
status: To Do
assignee: []
created_date: '2026-10-24 18:28'
labels:
  - profiles
  - verification
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: docs/safety.md's admission rule requires, for a verified heuristic, a container test with the real pinned producer wherever the kernel's view matters. The twelve built-ins admitted on 2026-10-24 by mark-emulator for Mark (TASK-122) have ModelHost vectors for their evidence and residuals but no such container test: nix-user-cache (mapped Git packs; a process between reads), go-mod-cache and bun-cache (a run in flight; bun's symlink backend, unprobed), mypy-cache (a run in flight stops with a blocking cache-write error), the four anchored build profiles (a build's cwd; a built program's exe/maps), and the four version profiles (a running version's exe/maps; a superseded extension's lazy load). Their Liveness contracts rows say 'Container test: pending (this task)'.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each of the twelve rows names a container test with the real pinned producer, or a dated waiver by Mark
- [ ] #2 The sync test requires one of: a named container test, pending with a task, or a dated waiver
<!-- AC:END -->
