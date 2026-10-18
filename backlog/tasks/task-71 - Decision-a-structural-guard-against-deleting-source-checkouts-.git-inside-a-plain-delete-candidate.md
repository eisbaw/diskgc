---
id: TASK-71
title: >-
  Decision: a structural guard against deleting source checkouts (.git inside a
  plain-delete candidate)
status: To Do
assignee: []
created_date: '2026-10-18 11:29'
updated_date: '2026-10-18 12:42'
labels:
  - wave-2
  - spike
  - safety
dependencies: []
references:
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: task-68's dry run of cmake-build as reference.md described it ("directories containing CMakeCache.txt") found, as the largest eligible candidate, ~/topics/cmake_nix_backend/CMake: an in-source CMake build, i.e. a git checkout (.git, CMakeLists.txt, Source/) with uncommitted changes, 5.6 GiB. A tier selecting cmake-build would have deleted the user's work. task-68 fixed the row (has_not: CMakeLists.txt), but the class remains: any marker-file profile (has:) or broad pattern under a project root (cachedir-tagged, a pytest-cache with a wrong depth, a user's `*`) can match a source tree, and the eligibility walk has no notion of "this is someone's work". The PRD accepts that a wrong profile deletes "unused, own files"; whether a VCS checkout should be one is the question.

Decision for Mark: (a) leave it to profiles and examples (status quo, with the has_not fix); (b) a fixed structural recogniser like the Cargo one: a .git (directory, or file as in worktrees and submodules), .hg or .svn anywhere in a plain-delete candidate makes it contains_special (cost: a name check in the walk; misses: caches that hold checkouts, e.g. ~/.cargo/git, Go's module cache, would be kept too); (c) only at the candidate's top level (cheaper, narrower). For (b) or (c): docs/safety.md (The eligibility walk) and the engine's recogniser, with model tests.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The decision (Mark's) is recorded in docs/safety.md or the PRD
- [ ] #2 If a guard is chosen, a model test shows an in-source build (.git and CMakeCache.txt) is never eligible for a plain delete
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
QA's real-binary evidence (task-68 review round 1): proj/insrc2, a directory holding .git and CMakeCache.txt but no CMakeLists.txt, is eligible for cmake-build even with the fixed row (has: CMakeCache.txt, has_not: CMakeLists.txt); and tmp-entry plans as eligible a /tmp directory holding two .git trees. The has_not fix narrows the observed case only; the class needs the guard decided here.
<!-- SECTION:NOTES:END -->
