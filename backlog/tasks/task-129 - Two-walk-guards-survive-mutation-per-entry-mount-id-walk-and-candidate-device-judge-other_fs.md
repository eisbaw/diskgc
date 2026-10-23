---
id: TASK-129
title: >-
  Two walk guards survive mutation: per-entry mount id (walk) and candidate
  device (judge other_fs)
status: To Do
assignee: []
created_date: '2026-10-23 22:30'
labels:
  - verification
  - safety
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-121 bite run (notes there): disabling engine/walk.rs's 'mnt_id != self.candidate.mount_id' check (M7) or fs_kind/judge.rs place()'s 'found.identity.id.dev != self.dev' check (M17) fails no vector. M7 is backed up by RESOLVE_NO_XDEV/EXDEV when the walk descends into a mounted directory (races/mount-made-during-the-locked-walk.yaml is still killed by that layer); only a mount on a non-directory (a file bind mount) would need the mnt_id check alone, and the host model can mount only on directories. M17 seems unreachable: a root on another filesystem is skipped per root as other_fs earlier, and a candidate on another device below a root is a mount (contains_mount). Decide per check: extend the host model (mount events on files) and add a vector, or prove the check redundant and document it as defence in depth (a mutation-testing exclusion with the reason).
<!-- SECTION:DESCRIPTION:END -->
