---
id: TASK-314
title: >-
  More real-world VM scenarios: real NFS lock path, TASK-305 overlay-upper loop
  alias, ext4/xfs as separate mounts, Docker overlay2 on btrfs, laptop-scale
  process churn
status: To Do
assignee: []
created_date: '2026-11-02T11:33:00Z'
updated_date: '2026-11-02T15:02:00Z'
labels:
  - test
  - vm
  - real-kernel
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-02: 'ensure we tackle these findings ... we need more real-world VM tests, so we need to bake into the e2e tests that we do clean up afterwards.' From COMPASS consult #3 (master c286b3b). COMPASS gaps still model-only after TASK-304: (1) a real NFS server and client in the VM for TASK-288's per-user lock and index lock (flock on NFS needs a writable fd); (2) TASK-305 item 5, the reverse loop alias through an overlay's upper layer (model red test first, then a VM case); (3) real deletion on ext4 and xfs as separate mounts (not tmpfs/overlay); (4) Docker overlay2 on btrfs (the mixed-layer control with a real dockerd); (5) laptop-scale process churn (fork churn + many processes) to approach TASK-239 on a real kernel; (6) optionally a real snapd if feasible in a NixOS VM, else documented. Each scenario cleans up and stays within the e2e cleanup task's per-stage disk budget.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-02 (COMPASS #4): item (2), the TASK-305 overlay-upper loop alias VM case, is removed from this task: TASK-305 item 5 already builds it in vm-real-kernel (branch t305-overlay-upper-alias).
<!-- SECTION:NOTES:END -->
