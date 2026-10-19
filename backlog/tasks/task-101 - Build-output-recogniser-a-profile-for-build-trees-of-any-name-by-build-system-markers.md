---
id: TASK-101
title: >-
  Build-output recogniser: a profile for build trees of any name, by
  build-system markers
status: To Do
assignee: []
created_date: '2026-10-19 12:29'
updated_date: '2026-10-19 12:29'
labels:
  - library
dependencies:
  - TASK-18
  - TASK-16
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: build directories have too many naming conventions to list (OpenWrt build_dir/staging_dir, buildroot output/, Yocto tmp/, build/, out/, builddir, bazel-*, in-place kernel builds); Mark removed an OpenWrt build_dir by hand on 2026-10-19 that no profile covered. Recognise build output by markers build systems write into their own output dirs, not by name: CMakeCache.txt/CMakeFiles (CMake), build.ninja/.ninja_log (Ninja), meson-private/ (Meson), config.status/config.log (autotools), a bazel-out symlink (Bazel), .built/.stamp_* stamps (OpenWrt/buildroot), saved_tmpdir (Yocto), CACHEDIR.TAG (any). The checkout guard keeps in-source builds; outer match only; idle-gated; intended for pressure tiers first. Name-based heuristics (many .o/.a/.d files, git-ignored, *build*/out names) belong in survey (task-38) as suggestions, never as a deletion rule.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A build-output profile matches directories of any name holding one of the documented markers, outer match only, never one holding .git/.hg/.svn (model tests per build system)
- [ ] #2 Verified read-only on real trees: an OpenWrt tree (build_dir, staging_dir), a buildroot output/, CMake and Meson build dirs; sizes match du
- [ ] #3 survey (task-38) scores large idle dirs as 'looks like build output' with the signals used, and suggests a profile; the scoring never selects candidates for deletion
- [ ] #4 Rebuildability is checked: a build dir whose recorded source tree is missing (or lacks its build file) is never eligible (model test per build system)
- [ ] #5 Each plan entry states the evidence: marker, source anchor, idle age
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Safe-heuristic rule (user, 2026-10-19: 'but a safe heuristic'). A dir is taken only when ALL independent signals agree; any doubt keeps it: (1) built: a build-system marker at its top level; (2) rebuildable: the source it was built from still exists (CMake: CMakeCache.txt's CMAKE_HOME_DIRECTORY holds CMakeLists.txt; Meson/autotools: their recorded source dir; OpenWrt/buildroot: the tree's top-level Makefile next to build_dir/output) - if the source is gone the output may be the only copy and is kept; (3) not a checkout (contains no .git/.hg/.svn, none on its path below the root); (4) idle N days, not in use, owned by the user; (5) outer match only. Names, object-file ratios and git-ignore status are survey signals only, never deletion criteria. Every candidate carries its evidence in the plan (marker, source anchor, idle age). Starts dry-run/pressure-tier only, verified on real trees first.
<!-- SECTION:NOTES:END -->
