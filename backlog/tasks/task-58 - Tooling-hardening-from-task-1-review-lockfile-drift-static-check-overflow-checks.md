---
id: TASK-58
title: >-
  Tooling hardening from task-1 review: lockfile drift, static check, overflow
  checks
status: To Do
assignee: []
created_date: '2026-10-16 23:16'
labels:
  - wave-1
  - hardening
  - tooling
dependencies:
  - TASK-1
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: non-blocking findings from the task-1 review, fixed once the tooling has stabilised. (1) diskgc-e2e/Cargo.lock drifts whenever a workspace crate gains a dependency and only nix flake check notices: add cargo metadata --locked --manifest-path diskgc-e2e/Cargo.toml to just check. (2) The diskgc-static check pipes readelf into grep under pipefail and could false-pass on SIGPIPE; capture output first, and prove it fails on a dynamic binary. (3) Flake tests run in the release profile with overflow checks off; enable overflow-checks = true in [profile.release] for a deletion tool, or run tests in a profile that has them.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 just check fails when diskgc-e2e/Cargo.lock is stale (proved once)
- [ ] #2 The static-binary check fails on a dynamically linked binary (proved once) and does not grep a pipe
- [ ] #3 Release builds have overflow checks on, or tests run with them, with the choice recorded in docs/architecture.md
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
