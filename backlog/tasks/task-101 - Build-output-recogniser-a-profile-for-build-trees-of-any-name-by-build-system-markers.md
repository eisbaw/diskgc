---
id: TASK-101
title: >-
  Build-output recogniser: a profile for build trees of any name, by
  build-system markers
status: To Do
assignee: []
created_date: '2026-10-19 12:29'
updated_date: '2026-10-23 15:24'
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

2026-10-20 user testing constraint for remaining work: do not inspect, dry-run, or delete real host project paths. AC2 real-tree verification must use tool-generated trees inside disposable containers; fake filesystem YAML vectors are checked in for model tests. Local docker CLI is Podman-backed, so native Docker Engine proof requires an isolated dind fixture.

Clarification: the existing checked-in ModelHost YAML sketch demonstrates the fixture mechanism; profile-specific checked-in vectors are still being implemented under TASK-103 and must not be claimed complete yet.

2026-10-20 commit 3ae1448: meson-build-anchored shipped with exact generated meson-info source/build JSON, surviving source meson.build, locked-candidate recheck, model mutation bite and real pinned Meson container E2E. Checked-in Meson YAML vector passes the shared ModelHost runner. Full just e2e 124/124 Nix and 1/1 native Docker smoke; QA/MPED GO. TASK101 remains In Progress: generic Ninja has no independent source anchor; Buildroot output can contain .config and deliverable images, and OpenWrt build_dir can contain source edits, so those are survey-only pending sound cleanup protocols. No host project paths tested.

2026-10-20 commit 4cd5197: Autotools source-anchored build profile added for generated GNU Autoconf 2.73 config.status. Strict ac_pwd/srcdir parser, surviving configure.ac/Makefile source, exact candidate binding and locked identity recheck. Checked-in ModelHost vector and 4 focused model tests cover copied output, missing source, checkout, fresh build, and source-disappearance race. Pinned Autoconf disposable-container E2E passed with measured output deletion and oracle clean. Mutation bite: reversing exact ac_pwd comparison caused the copied-output ModelHost test to fail by reclaiming 16 KiB; restored test passed. Full nix develop -c just e2e 126/126 Nix and 2/2 Docker; QA/MPED GO. No host project path was tested per user instruction, so AC2 real-host verification remains unmet. Other build systems and survey scoring remain open.

2026-10-20 safety correction in isolated branch task101-build-profiles-safety: retired marker-only cmake-build and west-build because a surviving source was not required. Existing cmake-build-anchored remains the CMake replacement; west-build-anchored retains the Zephyr directory marker and depth-1..5 build names, and requires the recorded CMake source under the bound root plus a mandatory 3-day idle floor. Explicit selection test rejects retired names. Checked-in ModelHost vector and dry-run tests keep missing and out-of-root source outputs, and plan evidence records marker/source/idle age. Focused disposable-container kernel E2E passed with oracle clean, preserving missing/out-of-root outputs; no real host diskgc run. Full e2e gate and integration review pending; TASK101 remains In Progress because its broad ACs are not complete.

Mutation bite on rebased TASK101 diff, isolated disposable worktree: removed only source_anchor: cmake from west-build-anchored. nix develop -c cargo test -p diskgc-model --test profile_vectors every_checked_in_profile_vector_matches_plan_and_reclaim --locked exited 101. The West vector exposed new missing-source and out-of-root-source candidates in the dry-run plan and lost source evidence on the positive candidate (actual 3 entries versus expected 1). Restored the source anchor; the same targeted test exited 0 (1/1 passed). No host diskgc execution or deletion. Final full e2e gate still pending after this note.

Final safety-correction gate on rebased 9e2ea47 tree: nix develop -c just e2e exited 0 after fmt, clippy, workspace tests and sandboxed Nix checks; 129/129 disposable Nix-container E2E tests and 5/5 native Docker Engine dind E2E tests passed. The focused West case deletes only the source-anchored Zephyr-shaped output; missing-source and out-of-root-source outputs remain and deletion oracle is clean. The positive fixture is Zephyr-shaped, not generated by a real west build. TASK101 remains In Progress: its broader build-system, survey and real-tool verification criteria are still open. Final exact-tree rerun after this note pending.

2026-10-20 TASK101 real-West follow-up in isolated /tmp/diskgc-west-real-fixture: Zephyr 4.3.0 source is flake-locked by exact revision/narHash, West 1.5.0 and GCC/CMake/Ninja/binutils from pinned Nixpkgs. Offline disposable Podman container bind-mounts the immutable Zephyr tree read-only inside a scratch West workspace and invokes west build -b native_sim/native/64 twice, producing real zephyr.elf and CMakeCache.txt with CMAKE_HOME_DIRECTORY:INTERNAL=/tmp/west-app. Focused nix develop -c just e2e-one west_generated_output_uses_surviving_source_anchor passed 1/1: shipped west-build-anchored dry-run reported the fresh rebuildable output as not_idle with CMakeCache/source evidence, omitted equally genuine output after its source CMakeLists.txt was removed, then equivalent no-idle-floor profile reclaimed only the rebuildable output with deletion oracle clean. This verifies West-generated layout but does not satisfy TASK101 broader marker systems, survey scoring or prohibited real-host read-only verification; task stays In Progress. Full exact-tree e2e and independent QA/MPED review pending.

2026-10-20 real-West follow-up final gate on main 94cd295 plus staged fixture: nix develop -c just e2e exited 0; fmt, clippy, workspace tests and sandboxed Nix checks passed; 130/130 Nix-container E2E including the full West-generated native_sim/native/64 case and 5/5 native Docker Engine dind E2E passed. QA and MPED reviews GO. No developer-host diskgc run or real project-path inspection. TASK101 remains In Progress for its broader acceptance criteria. Final exact-tree rerun after this tracker note pending.

2026-10-20 tracker resolution for b68c8b7: the final exact-tree nix develop -c just e2e rerun exited 0 after the preceding tracker note. Formatting, Clippy, workspace tests, sandboxed Nix checks, 130/130 disposable Nix-container E2Es and 5/5 native Docker dind E2Es passed. QA and MPED reviews were GO before that commit. This closes the earlier gate-pending notation; TASK101 remains In Progress for broader acceptance criteria.

Orchestrator 2026-10-23 (COMPASS consult): parked back to To Do to cut WIP. Library breadth waits for the producer-liveness cornerstone (D3: 'idle' = newest mtime/ctime cannot tell an active read-mostly tree from an abandoned one).
<!-- SECTION:NOTES:END -->
