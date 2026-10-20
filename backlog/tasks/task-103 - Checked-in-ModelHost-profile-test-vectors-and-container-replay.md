---
id: TASK-103
title: Checked-in ModelHost profile test vectors and container replay
status: Done
assignee: []
created_date: '2026-10-20 03:57'
updated_date: '2026-10-20 04:46'
labels: []
dependencies:
  - TASK-14
  - TASK-18
  - TASK-104
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Make each shipped and future profile verifiable from reviewable fake filesystem cases in crates/diskgc-model/tests/models/profiles. A data-driven runner loads YAML models and asserts plan/reclaim evidence without reading host project paths. Replay representative cases in a disposable container running a real Docker Engine (dind); the local docker command is Podman-backed and does not satisfy that proof. Never run diskgc against developer host project paths.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Checked-in YAML vectors cover positive, missing-source, checkout, in-use and age cases for representative profiles; each vector has explicit expected plan or reclaim outcome
- [x] #2 A shared runner executes vectors through ModelHost and proves a deliberately wrong matcher makes a focused test fail
- [x] #3 docs/testing.md explains how to add a vector and run both gates
- [x] #4 A real Docker Engine in an isolated dind container replays representative profile fixtures; assert the engine identity/version and never touch developer host project paths
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Phase A: add checked-in ModelHost YAML vectors and shared runner for profile plan/reclaim with biting negative cases. Phase B: replay representative behavior in disposable containers, identifying Podman-backed docker CLI and adding native Docker Engine dind once harness support exists. Root integrates, runs nix-shell just check and just e2e, gets parallel QA/MPED review before commit.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
User constraint: no testing against real host project paths or live host data. All filesystem behavior must be exercised by checked-in fake-FS vectors and disposable containers. The local docker CLI currently reports Podman 5.8.7 as its engine; native Docker Engine verification requires dind harness work.

TASK-104 owns the native Docker Engine dind fixture required by AC3; Podman-backed docker CLI runs may provide interim evidence but do not close AC3.

2026-10-20 commit 3ae1448: five checked-in YAML vectors, shared ModelHost plan/reclaim runner, wrong-matcher bite, rootless container Python vector replay and docs. Full just e2e 124/124 Nix plus native Docker fixture smoke 1/1. AC4 remains open: native Docker Engine has not yet replayed a profile vector; TASK104 supplied the fixture and a separate implementer is working on replay.

2026-10-20 update: AC4 is now closed by commit 57476a2; the earlier interim note predates native Docker Engine vector replay.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Commit 57476a2: 28 modelable filesystem built-in source names have checked-in YAML vectors with explicit removed/preserved postconditions; shared ModelHost runner enforces coverage and mutation bite. Native Docker Engine 29.8.2 dind replays the checked-in Python profile fixture. Full nix develop -c just e2e passed: 125/125 Nix image tests and 2/2 native Docker tests. QA and MPED reviewed before commit.
<!-- SECTION:FINAL_SUMMARY:END -->
