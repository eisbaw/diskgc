---
id: TASK-311
title: >-
  e2e cleans up after itself: containers, harness images, VM disks, gcroots,
  disk guard per stage
status: To Do
assignee: []
created_date: '2026-11-02T11:33:00Z'
labels:
  - test
  - infra
  - disk
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-02: 'ensure we tackle these findings ... we need more real-world VM tests, so we need to bake into the e2e tests that we do clean up afterwards.' From COMPASS consult #3 (master c286b3b). Every e2e recipe (e2e, e2e-full, e2e-one, e2e-debian, e2e-docker, VM checks) must leave the host as it found it, except bounded, named debug artefacts. (1) Containers: a passing test leaves none (already); a failed e2e-one keeps its container for debugging but at most the last N (e.g. 5) per worktree, older kept ones removed automatically, with 'just e2e-kept' to list and 'just e2e-kept --drop' to remove; never another program's containers (match the diskgc-e2e- name prefix AND a label the harness sets at create time). (2) Images: after a run, harness images whose tag is not current for ANY live worktree's flake (or older than the current one and unused by a container) are pruned automatically (extend tools/e2e_images.py; TASK-60 deliberately did not auto-prune because worktrees on other inputs would remove each other's images - solve that with a per-image last-used stamp and a grace period, e.g. keep tags used in the last 24 h). (3) VM checks: no persistent VM disks outside the Nix build sandbox; verify the NixOS test drivers leave nothing in /tmp or /run/user/1000. (4) Nix store: e2e results are linked under .gcroots/<name> per worktree; 'just gcroots-drop' already exists - stale gcroots from renamed recipes (e2e-runner-release-01*) are dropped automatically when a recipe no longer exists; worktree removal drops its gcroots. Nix GC itself stays manual (host-wide, Mark's call) but 'just e2e' prints how much a 'nix-store --gc' would free. (5) Disk guard: each heavy stage (VM, mutation, fuzz, e2e-full parts) checks free space against a documented per-stage budget and refuses to start below it with a clear message (never weakens a guard to start); the run prints df before and after each stage and the delta. (6) Tests for the cleanup tool (unit tests with a fake podman/nix like test_e2e_images.py) and a self-check at the end of just e2e that no new diskgc-e2e containers/images/scratch remain beyond the retained set. Only this harness's own artefacts are ever removed.
<!-- SECTION:DESCRIPTION:END -->
