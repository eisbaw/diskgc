---
id: TASK-99
title: cmake-build misses deep out-of-source build dirs (depth limit)
status: Done
assignee: []
created_date: '2026-10-19 12:19'
updated_date: '2026-10-20 02:03'
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
- [x] #1 cmake-build finds a build dir at depth 5 under its root (model test), still refusing in-source builds and checkouts
- [x] #2 A read-only dry run here finds the retimer out/* build dirs
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-20 MPED implementation draft found 28/28 eligible CMake build dirs in a read-only host plan, 21,942,411,264 estimated bytes total; individual du -sB1 matched each 28/28. Safety review found `has_not: CMakeLists.txt` tests only regular files, so a symlinked in-source CMakeLists.txt could pass. Do not ship this draft unchanged. Add a separate no-entry predicate (absent name required, any file type or read error refuses), then regression for symlink and create-before-reclaim race. TASK101 owns broader source-anchor validation.

2026-10-20 integrated fix: CMake 3.30.5 recorded in surveyed retimer CMakeCache.txt (CMAKE_CACHE_MAJOR/MINOR/PATCH_VERSION and CMAKE_COMMAND). Read-only host plan found 28/28 eligible out-of-source directories at depths through five, 21,942,411,264 estimated bytes; individual du -sB1 matched all 28/28. `lacks_entry: CMakeLists.txt` rejects every entry type, including a dangling symlink, and is rechecked under candidate lock. Explicit null/blank is rejected at load. Model covers depth five, source/checkouts and create-before-reclaim; real-kernel dangling symlink E2E added. No live deletion.

2026-10-20 distinguishing mutation bite (all restored after red runs): remove the depth-five pattern, change type dir→file, drop the required CMakeCache.txt marker, or allow checkouts: `profile::golden::cmake_build_has_a_frozen_five_level_match_set_and_markers` fails in each case. Change `lacks_entry: CMakeLists.txt` to `has_not: CMakeLists.txt`: `shipped_cmake_build_finds_depth_five_and_refuses_sources_and_checkouts` fails because the symlinked source marker becomes eligible. Baseline focused tests and the kernel dangling-symlink E2E pass after restoration.

2026-10-20 shipped in 8e5d907 after full nix develop -c just e2e 105/105; independent QA and MPED architecture GO. Dangling-symlink kernel E2E passed 1/1. TASK101 remains responsible for generalized source-anchor validation.
<!-- SECTION:NOTES:END -->
