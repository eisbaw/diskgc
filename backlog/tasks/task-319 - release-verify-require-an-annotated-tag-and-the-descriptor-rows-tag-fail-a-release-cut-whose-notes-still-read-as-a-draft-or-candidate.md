---
id: TASK-319
title: >-
  release-verify: require an annotated tag and the descriptor row's tag; fail a
  release cut whose notes still read as a draft or candidate
status: To Do
assignee: []
created_date: '2026-11-02T14:33:00Z'
labels:
  - release
  - tooling
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-315 reviews (mped-architect, qa-test-runner, 2026-11-02). (1) tools/release_verify.py resolves refs/tags/v$VERSION^{commit}, so a lightweight tag passes; require git cat-file -t v$VERSION == tag. (2) Nothing checks that the release row's tag in crates/diskgc-core/src/releases.json equals v$VERSION on the tagging commit; release.rs only validates its format when set. (3) flake.nix cutReleaseNotes ships the draft section verbatim: v0.1.0's RELEASE-NOTES.md still said 'Status: draft ... 0.1 is not released until TASK-67 closes', and the current draft says 'Do not deploy the candidate built from master'. For a release version (no -dev), fail the cut on candidate-only wording (a marker the draft carries and the RC commit removes). The 0.2 release criteria (docs/architecture.md) rely on (1)-(3) by hand until this lands.
<!-- SECTION:DESCRIPTION:END -->
