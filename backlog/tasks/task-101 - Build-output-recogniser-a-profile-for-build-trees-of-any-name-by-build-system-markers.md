---
id: TASK-101
title: >-
  Build-output recogniser: a profile for build trees of any name, by
  build-system markers
status: In Progress
assignee: []
created_date: '2026-10-19 12:29'
updated_date: '2026-10-20 03:52'
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

2026-10-20 TASK101 partial implementation in uncommitted /tmp/diskgc-task101: cmake-build-anchored only. CMakeCache.txt must record one normalized CMAKE_HOME_DIRECTORY strict ancestor within bound root; regular CMakeLists.txt outside candidate is opened without symlinks and its identity rechecked during judgement and under candidate lock. CMakeFiles directory, no CMakeLists in output, three-day idle floor, checkout refusal, depth <=5. Model dry-run and reclaim tests pass; nix develop -c just check passes. Read-only host dry-run on Tracy found profiler/build eligible 181874688 B, equal to du -sB1; tracy/build and capture/build refused nested .git. Plan includes marker/source/age for eligible, null age for skipped with discovery evidence. No host deletion, no Podman, no E2E, no commit. Other TASK101 systems (Meson, Ninja, autotools, Bazel, OpenWrt/buildroot, Yocto, generic CACHEDIR.TAG) and survey scoring remain unimplemented; source-anchor evidence and model/host verification are needed before any of those can safely delete. AC1-5 are not fully met.

2026-10-20 integrated narrow cmake-build-anchored slice in commit 607d025. Full nix develop -c just e2e passed 121/121, including real-kernel anchored CMake guard/reclaim; QA and MPED GO. Locked candidate fd is used for anchor recheck, with model path-swap and source-identity race tests. Read-only Tracy dry run found profiler/build 181874688 B matching du and refused nested checkouts. Task remains In Progress: other build systems, OpenWrt/buildroot read-only verification, survey scoring, and per-system rebuildability tests are not implemented; AC1-5 as a whole remain open. No host deletion.
<!-- SECTION:NOTES:END -->
