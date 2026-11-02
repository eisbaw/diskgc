---
id: TASK-312
title: >-
  vm-real-kernel in just e2e, with wall time recorded; root scan across the
  pidfd kernel matrix
status: To Do
assignee: []
created_date: '2026-11-02T11:33:00Z'
labels:
  - test
  - vm
  - real-kernel
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-02: 'ensure we tackle these findings ... we need more real-world VM tests, so we need to bake into the e2e tests that we do clean up afterwards.' From COMPASS consult #3 (master c286b3b). COMPASS: add vm-real-kernel to just e2e (Nix caches the check, so it reruns only when the binary or script changes); record its wall time in verification notes. TASK-306 depended on the kernel version: run the vm-real-kernel root_scan scenario (kernel-thread facts, a root dry run and reclaim) on each kernel of the pidfd matrix (5.8 .. 6.12) plus 6.18, cheaply (a small VM per kernel, not the full scenario set). Depends on the e2e cleanup task (disk).
<!-- SECTION:DESCRIPTION:END -->
