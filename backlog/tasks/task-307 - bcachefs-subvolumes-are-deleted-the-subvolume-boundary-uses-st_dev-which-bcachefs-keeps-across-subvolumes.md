---
id: TASK-307
title: >-
  bcachefs subvolumes are deleted: the subvolume boundary uses st_dev, which
  bcachefs keeps across subvolumes
status: To Do
assignee: []
created_date: '2026-11-01T20:57:00Z'
labels:
  - safety
  - irreversible
  - bug
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the TASK-304 real-kernel VM test (vm-real-kernel, NixOS guest Linux 6.18.56; branch t304-vm-real-kernel a100ac4; logs tmp/worktrees/t304/tmp/t304-logs/run{A,B,C}.log, 3 identical runs). Scenario 7 (bcachefs 1.39.6 out-of-tree module): a nested subvolume has its parent's st_dev (both 1792), so TASK-286's directory st_dev boundary cannot see it; as tester, ensure --min-free 100% /mnt/bcfs reclaims candidate scratch-nested including subvolume inner. Docs promise the rule for btrfs only, but deleting a subvolume is what the boundary exists to prevent. Fix: use STATX_SUBVOL (stx_subvol, Linux 6.10+) in the walk, discovery and Cargo boundaries alongside st_dev; where the kernel doesn't report stx_subvol on a bcachefs (or any fs in TASK-301's 'subvolumes without st_dev' class), refuse candidates on it (fail closed) or document. Related: TASK-301 (bcachefs in MAPS_BY_INODE, snapshots reuse inodes).
<!-- SECTION:DESCRIPTION:END -->
