---
id: TASK-4
title: Cargo-under-test toolchains and deterministic race helpers in the test image
status: To Do
assignee: []
created_date: '2026-10-16 22:39'
labels:
  - wave-1
  - testing
dependencies:
  - TASK-2
references:
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the Cargo race tests are the load-bearing acceptance signal and must be deterministic. Install pinned rust-overlay toolchains (oldest supported stable, one dated nightly) with a C toolchain under /opt/cargo-under-test in the test image; write the Rust helpers: a lock holder (holds .cargo-lock so Cargo blocks after taking .cargo-build-lock), a build.rs that waits on a control file, a runner wrapper that pauses between compile and run; offline fixture projects without registry dependencies.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A smoke build per toolchain runs first and fails as a toolchain problem, not a race failure
- [ ] #2 Each helper's pause/resume is verified by sampling /proc/locks in a container test
- [ ] #3 No network access is needed by the fixtures (CARGO_NET_OFFLINE=true)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
