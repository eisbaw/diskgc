---
id: TASK-304
title: >-
  VM test on a real kernel: btrfs on two loop members, an idmapped bind, a
  loop-backed image in a protected match, a live overlay root with snaps
status: Done
assignee: []
created_date: '2026-11-01T17:45:00Z'
updated_date: '2026-11-02T00:41:00Z'
labels:
  - test
  - vm
  - safety
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-299 review (mped S5, 2026-11-01): the Linux side of the loop identity and btrfs membership code has never run on a real kernel: reopen through /proc/self/fd + BTRFS_IOC_FS_INFO / DEV_INFO on real btrfs, /sys/fs/btrfs/<fsid>/devices, idmapped parsing from a live mountinfo, LOOP_GET_STATUS64 on a bound device (root in the VM), losetup -P partitions, the TASK-286 subvolume boundary and FSID snapshot key, bcachefs if the VM kernel has it. Rootless containers cannot create any of these; a NixOS VM test (like vm-release-tmp-reclaim) can. Make it a 0.2 release criterion. Include the TASK-296 configs: an ISO via udisksctl loop-setup as a non-disk user, a snap-like root-owned image, a hidepid /proc, and a reclaim with the goal already met under hidepid (TASK-298).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: in progress in tmp/worktrees/t304 (branch t304-vm-real-kernel).

2026-11-02: merged with the real-kernel VM test after one review round on the product fixes (verification note on the merge).
<!-- SECTION:NOTES:END -->
