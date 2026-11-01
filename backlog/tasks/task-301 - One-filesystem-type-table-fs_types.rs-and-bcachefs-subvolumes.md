---
id: TASK-301
title: One filesystem-type table (fs_types.rs) and bcachefs subvolumes
status: To Do
assignee: []
created_date: '2026-11-01T16:39:00Z'
updated_date: '2026-11-01T19:05:00Z'
labels:
  - correctness
  - refactor
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #3 on master fc33d98 (2026-11-01; Codex tmp/reviews/codex-full3.md + mped), static reading. mped SHOULD 3+4: NETWORK_FS_TYPES (name, unknown=local), LOCAL_FILESYSTEMS (magic, unknown=not local), MAPS_BY_INODE, DEVICE_NOT_UNIQUE, STACKED_ON_DIRECTORIES, cow_note btrfs|zfs, state.rs is_btrfs, on_loop's 'btrfs', shared_space's 'zfs' - already drifted (bcachefs in DEVICE_NOT_UNIQUE and LOCAL_FILESYSTEMS but not MAPS_BY_INODE; no CoW note for bcachefs). Fix: one table keyed by mountinfo name with the magic and one flag per question, tests iterating it. bcachefs keeps one st_dev across subvolumes (STATX_SUBVOL instead): the TASK-286 nested-subvolume boundary never fires there and snapshots reuse inode numbers. Fix: add bcachefs to MAPS_BY_INODE (safe direction); use stx_subvol in identity/boundary checks or state the limit.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01 (TASK-299): add to the table: may_show_other_owners (owner-view column), SHOWS_STORED_OWNERS = [overlay, ecryptfs], STACKED_ON_DIRECTORIES, and the 'btrfs' literals in places::may_be_on_loop, diskgc-linux is_btrfs, LinuxHost::btrfs_reader and the model host.
<!-- SECTION:NOTES:END -->
