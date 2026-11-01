---
id: TASK-303
title: >-
  Owner for the remaining in-use blind spots (io_uring, overlay upper/work dirs,
  other mount namespaces, Cargo locks through overlay, NFS/virtiofs)
status: To Do
assignee: []
created_date: '2026-11-01T16:39:00Z'
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
