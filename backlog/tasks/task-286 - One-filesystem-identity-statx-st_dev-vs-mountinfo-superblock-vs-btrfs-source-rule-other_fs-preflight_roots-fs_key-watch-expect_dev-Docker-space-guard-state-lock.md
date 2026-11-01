---
id: TASK-286
title: >-
  One filesystem identity: statx st_dev vs mountinfo superblock vs btrfs source
  rule (other_fs, preflight_roots, fs_key, watch expect_dev, Docker, space
  guard, state lock)
status: To Do
assignee: []
created_date: '2026-11-01T05:37:00Z'
updated_date: '2026-11-01T15:38:00Z'
labels:
  - correctness
  - btrfs
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. mped SHOULD 1 + Codex 5/6/7: other_fs, preflight_roots, snapshot fs_key (watch::start at.stat.dev), watch expect_dev and docker inspect use st_dev; state lock and space guard use the mountinfo superblock; held_by_deleted_files has its own btrfs rule. On btrfs (Fedora/openSUSE) each subvolume has its own st_dev: roots on a sibling subvolume are skipped as other_fs (exit 3 though space is reclaimable), fs_uuid never matches so fs_key is per subvolume (contradicts reference.md: one watcher serves both; two watchers start, TASK-262). Snapshot guard defeated by a file bind over index-<key>.lock (dir_dev from the lock file's mount) and by a state dir on overlayfs whose upper lives on the watched fs (device inequality is not independent space). Fix: one Mounts/identity value (TASK-274 refactor 1): same_space(a,b) from mountinfo superblock + btrfs source rule, fs_key input, space guard from the state-directory descriptor; conservative guard when the pool cannot be proved independent; model fixture where statx dev != mountinfo dev. Supersedes TASK-262.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: in progress in tmp/worktrees/t286 (branch t286-fs-identity).

2026-11-01 (t286-fs-identity): one rule, home crates/diskgc-core/src/places.rs (space_of, same_space, shared_space), stated once in docs/architecture.md Filesystem matching. Same filesystem = the device mountinfo names for the mount (superblock), never statx st_dev. Used by preflight_roots, fs Found.space / cargo Target.space (Judge and cargo other_fs), Docker storage, watch cycle PATH check (WatchCycle::expect_space), fs_key (Host::fs_uuid takes the MountEntry; btrfs UUID via mountinfo source), snapshot guard (StateLock carries the state directory's fd and mount; guard unless shared_space is None; sampled on the dir fd), held_by_deleted_files (by each file's stx_mnt_id). Model: MountSpec.subvolume (statx dev differs from mountinfo dev). Red-first: 4 model tests + 1 container test (file bind over index-<fs>.lock) failed on adf4d70, pass here. Covers TASK-262 (two watchers on two subvolumes of one btrfs: second exits 2). Follow-ups listed in the report: index records no unit on btrfs (Identity.dev st_dev vs MountKey.dev superblock), bind_view refuses roots on two subvolume mounts, ZFS datasets of one pool are other_fs for each other.

2026-11-01 review round (90bfc91 on t286-fs-identity): subvolume boundaries (contains_mount for a directory on another st_dev without a mount: walk, fs discovery, Cargo inspection/targets), write_guard (sample PATH and state dir; state on a loop-backed fs not written), state_write through the held dir fd, btrfs FSID via BTRFS_IOC_FS_INFO, bcachefs f_fsid confirmation (Space newtype), policy loop images via on_loop, CoW warning, index keeps btrfs units, places/space.rs split. Red-first for 6 model scenarios on 9c68710. Not modelled: btrfs on loop / loop partitions in policy, two filesystems on one device (bcachefs) beyond a unit test.
<!-- SECTION:NOTES:END -->
