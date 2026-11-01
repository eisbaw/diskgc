---
id: TASK-286
title: >-
  One filesystem identity: statx st_dev vs mountinfo superblock vs btrfs source
  rule (other_fs, preflight_roots, fs_key, watch expect_dev, Docker, space
  guard, state lock)
status: To Do
assignee: []
created_date: '2026-11-01T05:37:00Z'
updated_date: '2026-11-01T13:34:00Z'
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
<!-- SECTION:NOTES:END -->
