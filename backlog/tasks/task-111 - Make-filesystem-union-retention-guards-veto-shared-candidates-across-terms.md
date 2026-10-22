---
id: TASK-111
title: Make filesystem union retention guards veto shared candidates across terms
status: To Do
assignee: []
created_date: '2026-10-22 13:05'
updated_date: '2026-10-22 13:07'
labels:
  - safety
  - selection
  - irreversible
dependencies:
  - TASK-15
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A filesystem union currently admits a candidate when any positive term passes its per-term Judge. A keep_newest or link/current/installed protection guard on another term can return Protected, yet plan_fs_union and walk_fs_union retain the passing term and may delete that same inode. Retention guards should veto deletion of a shared candidate regardless of another term. Resolve and document guard scope across overlapping source terms, while preserving term-local evidence filters and source action semantics.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A candidate matched by two filesystem union terms is retained when any matching source or positive filter marks it keep_newest, protected by a link/current link, or installed by a manifest, regardless of union order.
- [ ] #2 Dry-run and reclaim use the same guard decision for shared candidates; a disposable-container real-binary fixture proves the protected candidate survives and an unprotected sibling is reclaimed.
- [ ] #3 The documented union semantics distinguish global retention guards from term-local evidence filters, and property/reference tests cover commutativity and guard veto.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
This is a safety prerequisite of TASK-16. Its initial dependency on TASK-16 was reversed after review; TASK-16 remains open until the shared-candidate guard bypass is fixed.
<!-- SECTION:NOTES:END -->
