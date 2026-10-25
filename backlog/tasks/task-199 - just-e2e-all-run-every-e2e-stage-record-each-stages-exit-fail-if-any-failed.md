---
id: TASK-199
title: >-
  just e2e-all: run every e2e stage, record each stage's exit, fail if any
  failed
status: Done
assignee: []
created_date: '2026-10-25 08:39'
updated_date: '2026-10-25 11:03'
labels:
  - test
  - shipping
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
COMPASS round 3 (2026-10-25): just e2e stops at the first failing stage, so a red stage hides every later stage (47d9bd4: release/Debian/Docker/VMs never ran; 72b376c: two VMs never ran). Add a recipe for async QA that runs all stages (check, flake checks, container suites, release, Debian, Docker, each VM), writes a per-stage exit table, and exits non-zero if any failed. Keep just e2e unchanged. Do with TASK-197 (lane D, diskgc-e2e/justfile).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Merged to master as 5878d68 on 2026-10-25T11:03Z (fast-forward; branch already on master).
<!-- SECTION:NOTES:END -->
