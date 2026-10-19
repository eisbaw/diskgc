---
id: TASK-99
title: cmake-build misses deep out-of-source build dirs (depth limit)
status: To Do
assignee: []
created_date: '2026-10-19 12:19'
labels:
  - library
dependencies:
  - TASK-16
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: cmake-build lists its depths explicitly (no **, task-16 decision) and looks 3 levels below its roots; with roots ~ and ~/topics it missed 40 out-of-source CMake build dirs under ~/nvidia/retimer/out/* and retimer-sim/firmware/retimer/out/* (19.4 GiB). Decide: more explicit depths, one root per project, or an any-depth form for has:-marked dirs (still outer match only). Source: read-only gap investigation 2026-10-19 (scratchpad gap/summary.txt).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 cmake-build finds a build dir at depth 5 under its root (model test), still refusing in-source builds and checkouts
- [ ] #2 A read-only dry run here finds the retimer out/* build dirs
<!-- AC:END -->
