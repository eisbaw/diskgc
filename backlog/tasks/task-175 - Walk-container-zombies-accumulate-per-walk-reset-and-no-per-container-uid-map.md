---
id: TASK-175
title: >-
  Walk container: zombies accumulate per walk-reset, and no per-container uid
  map
status: Done
assignee: []
created_date: '2026-10-25 02:19'
updated_date: '2026-10-25 03:52'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented on branch walk-tools 4e707b8 (unmerged). Evidence tmp/walks/2026-10-25-walk-tools/. Gate: nix develop -c just check exit 0 (1116 passed, 0 failed, 1 ignored; unittest 34 OK). just e2e not run. --init: 5x walk-reset on wt-a stays 303 processes, 0 zombies (19-*); legacy walk-diskgc-podman had 96 zombies of 389. WALK_UIDMAP knob replaced by per-NAME uid slots (see TASK-163).

Merged to master as aaa6c36 on 2026-10-25T03:52Z.
<!-- SECTION:NOTES:END -->
