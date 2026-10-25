---
id: TASK-175
title: >-
  Walk container: zombies accumulate per walk-reset, and no per-container uid
  map
status: To Do
assignee: []
created_date: '2026-10-25 02:19'
labels:
  - shipping
  - tooling
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in TASK-145. tools/walk/up.sh runs podman without --init, so PID 1 is sleep and never reaps: every just walk-reset leaves the previous holders (webapp, cargo, build script, pytest, sleeps) as zombies; after ~24 resets the container had 443 processes instead of ~306, which inflates in-use scan cost across a measurement series. Also up.sh has no way to pass a distinct --uidmap, which the 2026-10-25 gotcha requires for parallel walk containers (TASK-145 copied up.sh to add --uidmap 1:3:65533). Fix: --init (or reap in reset.sh) and a WALK_UIDMAP knob.
<!-- SECTION:DESCRIPTION:END -->
