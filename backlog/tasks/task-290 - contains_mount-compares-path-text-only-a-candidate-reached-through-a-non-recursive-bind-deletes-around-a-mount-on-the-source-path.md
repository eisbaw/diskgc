---
id: TASK-290
title: >-
  contains_mount compares path text only: a candidate reached through a
  non-recursive bind deletes around a mount on the source path
status: To Do
assignee: []
created_date: '2026-11-01T05:37:00Z'
updated_date: '2026-11-01T11:25:00Z'
labels:
  - safety
  - irreversible
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. mped SHOULD 9: fs_kind/judge.rs mounted_at_or_inside uses m.mount_point.starts_with(at); a candidate reached through a non-recursive bind does not see a mount on the source path; diskgc deletes the rest of the candidate, then EBUSY on the mount point: a partial deletion of a candidate the PRD says to keep. Fix: compare places (policy::place_of / attachments); model test red first.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: implemented in tmp/worktrees/t267 (branch t267-bind-places) with TASK-267/285; see e3bd4bb.

Details (e3bd4bb, rebased as a8f3236): Judge::place and the eligibility walk refuse mounts within the candidate by place (places::mounts_within); the model answers EBUSY for a mount point through any mount of its filesystem. Vectors walk/{,cargo-}mount-point-inside-through-a-non-recursive-bind; container test a_non_recursive_bind_keeps_a_candidate_holding_a_mount_point.
<!-- SECTION:NOTES:END -->
