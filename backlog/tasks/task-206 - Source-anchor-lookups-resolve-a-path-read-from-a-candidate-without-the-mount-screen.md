---
id: TASK-206
title: >-
  Source-anchor lookups resolve a path read from a candidate without the mount
  screen
status: Done
assignee: []
created_date: '2026-10-25 09:48'
updated_date: '2026-10-26 06:38'
labels:
  - safety
  - mounts
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the lane-c mped-architect review (2026-10-25), predates lane-c. source_anchor.rs resolves the source dir read from CMakeCache.txt (CMAKE_HOME_DIRECTORY), meson-info or config.status with resolve_in, which opens that path's mount point (crossing mounts) before comparing source_dir.mount.id with the candidate's mount. A local build dir whose source is on NFS or under an untriggered automount (/net, NFS /home) can hang that lookup on a dead server (TASK-178/180 class) or mount the share. Fix the class: screen_mount_chain inside resolve_in so no caller can skip it (fits TASK-196's one mount screen); at minimum compare mount_for(source).id before resolve_in. Vector: CMake source under a dead: true mount.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-26 t182 (branch t182: 10eb58f, 11c8a8d, 792bd53; not merged; base master 0f4a2ec). Closed by the screen inside resolve_in (TASK-182). Vector races/cmake-source-mounted-over-after-the-cache-read: an nfs4 mount on the source dir after the cache read; master opens the new mount point (trace #107) and plans other_fs; t182 never opens it and plans unsupported_fs. Close when t182 merges.
<!-- SECTION:NOTES:END -->
