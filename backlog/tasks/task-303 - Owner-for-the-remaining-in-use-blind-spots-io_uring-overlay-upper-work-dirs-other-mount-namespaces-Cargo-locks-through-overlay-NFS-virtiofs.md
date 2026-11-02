---
id: TASK-303
title: >-
  Owner for the remaining in-use blind spots (io_uring, overlay upper/work dirs,
  other mount namespaces, Cargo locks through overlay, NFS/virtiofs)
status: To Do
assignee: []
created_date: '2026-11-01T16:39:00Z'
updated_date: '2026-11-02T14:14:00Z'
labels:
  - safety
  - inuse
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #3 on master fc33d98 (2026-11-01; Codex tmp/reviews/codex-full3.md + mped), static reading. mped SHOULD 11: the draft release note's 'under review (TASK-169)' points at a closed task; these blind spots have no owner: io_uring registered files, overlay upper/work directories, mounts in other namespaces, Cargo locks taken through an overlay, NFS/virtiofs. Each: state as a documented limit in safety.md (single home) or fix; retarget the release-note line here.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-02 (TASK-315): the draft release notes' 'In-use blind spots under review' line now names TASK-303 instead of the closed TASK-169 (docs/reference.md, Known limits); the limits themselves are still to be stated or fixed here. TASK-169's note lists which of its items were reproduced and which this task carries (items 1 io_uring and overlay upper/work, 2, 3, 6).
<!-- SECTION:NOTES:END -->
