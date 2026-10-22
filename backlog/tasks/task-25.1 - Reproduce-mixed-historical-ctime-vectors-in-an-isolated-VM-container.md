---
id: TASK-25.1
title: Reproduce mixed historical ctime vectors in an isolated VM container
status: To Do
assignee: []
created_date: '2026-10-22 08:00'
updated_date: '2026-10-22 08:08'
labels:
  - testing
  - vm
  - differential
dependencies: []
references:
  - docs/testing.md#4-differential-contract-tests
parent_task_id: TASK-25
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-25 prerequisite. The Autotools, CMake, Meson, and containers-image originals mix old eligible and fresh not_idle ctime cohorts. Fresh rootless-container tmpfs gives every inode current kernel ctime; the sandbox clock offset changes only diskgc wall-clock reads. A disposable KVM/QEMU probe proved guest tmpfs inodes retain distinct kernel ctimes after guest CLOCK_REALTIME advances, while host time advanced only one second. Build a Nix-pinned disposable guest and run deleting diskgc exclusively in Podman inside the guest. Stage fixture creation across guest clock changes; preserve the checked-in YAML and independent ModelHost expectation. The containers-image case also requires /var/tmp and process evidence. Keep host-time and host-deletion interlocks explicit.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A reproducible Nix-built QEMU guest boots with Podman, native overlay, a private writable guest disk, no writable host bind mounts, and a verified host-clock isolation probe.
- [ ] #2 The guest container stages old and fresh fixture objects at two guest wall-clock times; actual kernel statx ctimes and expected idle/not_idle thresholds are asserted before the diskgc run.
- [ ] #3 All four original YAMLs pass unchanged independent ProfileVector expectations and ModelHost versus LinuxHost dry plan, reclaim exit/actions/skips/disappeared-path comparisons; the existing strace deletion oracle judges both runs.
- [ ] #4 The guest container preserves the E2E interlock canary, host deletion remains unavailable, and the four VM replays are registered in the exact just e2e gate, which passes.
<!-- AC:END -->
