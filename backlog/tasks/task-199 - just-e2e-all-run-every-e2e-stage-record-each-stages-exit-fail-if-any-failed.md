---
id: TASK-199
title: >-
  just e2e-all: run every e2e stage, record each stage's exit, fail if any
  failed
status: To Do
assignee: []
created_date: '2026-10-25 08:39'
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
