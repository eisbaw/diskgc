---
id: TASK-25.1
title: Reproduce mixed historical ctime vectors in an isolated VM container
status: Done
assignee: []
created_date: '2026-10-22 08:00'
updated_date: '2026-10-22 09:33'
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
- [x] #1 A reproducible Nix-built QEMU guest boots with Podman, native overlay, a private writable guest disk, no writable host bind mounts, and a verified host-clock isolation probe.
- [x] #2 The guest container stages old and fresh fixture objects at two guest wall-clock times; actual kernel statx ctimes and expected idle/not_idle thresholds are asserted before the diskgc run.
- [x] #3 All four original YAMLs pass unchanged independent ProfileVector expectations and ModelHost versus LinuxHost dry plan, reclaim exit/actions/skips/disappeared-path comparisons; the existing strace deletion oracle judges both runs.
- [x] #4 The guest container preserves the E2E interlock canary, host deletion remains unavailable, and the four VM replays are registered in the exact just e2e gate, which passes.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-22 implementation: Nix-built QEMU guest boots a private 4 GiB ext4 disk with rootful Podman native overlay; no writable host bind mounts; /nix/store and E2E binaries are read-only in the guest container. VM driver checks guest elapsed time and host clock isolation. Slim image retains the normal sandbox marker, users, mount points, descriptor and strace; a marker-removed Podman overlay canary exits 86 with an explicit interlock refusal. Four original YAMLs (Autotools, CMake, Meson, containers-image) are unchanged. Each stages old and fresh cohorts using guest CLOCK_REALTIME, asserts statx ctime and mount ID before run (old 1790812800; fresh/model now 1792033200; 259200s idle floor), checks original independent ProfileVector expectation, compares ModelHost/LinuxHost dry and reclaim outcomes, and invokes existing strace deletion oracle. Latest nix build --no-link .#checks.x86_64-linux.vm-historical-ctime exited 0 with four PASS replays. Focused E2E crate Clippy --deny warnings passed, 99/99 unit tests passed. Exact just e2e full gate and parallel QA/MPED review are delegated to parent before commit; AC #4 remains unchecked until gate passes. No host deletion or commit in this slice.

2026-10-22 MPED review found the initial host-clock check observed only the end of each restored replay, so it could miss a transient leaked shift. Added a direct QEMU-driver probe: hold guest CLOCK_REALTIME at 1790812800, read host wall time against a monotonic baseline while guest is still old, and restore guest time in finally. Focused Nix VM check exit 0 after this change; exact just e2e and final QA/MPED re-review pending. No developer-host diskgc run or host deletion.

2026-10-22 final parent gate: exact nix develop -c just e2e exited 0 on final VM probe tree. QA GO: fmt/Clippy/workspace, Nix fast 216/216, Docker fast 7/7, rebuilt VM check with four unchanged historical vectors, marker refusal and direct host-clock observation all passed. Named mped-architect reviewed final ordering and gave GO with no remaining blocker. TASK-25 parent remains In Progress for its broader materializer and generated-model criteria.
<!-- SECTION:NOTES:END -->
