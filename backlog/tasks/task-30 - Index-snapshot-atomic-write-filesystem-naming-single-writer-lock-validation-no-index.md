---
id: TASK-30
title: >-
  Index snapshot: atomic write, filesystem naming, single-writer lock,
  validation, --no-index
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-17 10:59'
labels:
  - wave-3
  - architecture
dependencies:
  - TASK-29
references:
  - docs/architecture.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: ensure on a runner needs a warm index from watch. Implement the snapshot per docs/architecture.md: atomic temp+rename, index-<fs> by UUID or device number, separate never-renamed lock file, version and profiles+roots hash, one-hour freshness, mode 0600, corrupt or mismatched snapshots ignored with a log line.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A second watch for the same filesystem and user exits 2
- [ ] #2 A corrupted or other-user snapshot falls back to synchronous tally and the report says so
- [ ] #3 Config changes to tiers do not invalidate the snapshot; changes to profiles or roots do
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-50's review: the snapshot's temp file plus rename (and the open with O_CREAT/O_WRONLY or O_TRUNC) is a write and a rename under the source scan (crates/diskgc-linux/src/source_scan.rs, GATED), which forbids those outside allowlisted wrappers. watch --tally-only must work anywhere, so these writes are NOT behind the sandbox gate. Decision (docs/architecture.md, The Host port): diskgc's own state writes go through ONE port operation in LinuxHost, relative to the held state-directory fd (openat2 beneath it, write the temp file, renameat over the old snapshot), added to GATED_ALLOWED as an ungated entry with its exact match count. Nothing else in the product may write or rename. The separate lock file is opened read-only for flock, as the host lock is. ModelHost needs the same operation.

From task-50 round 3 (mped), pinning the state-write signature (docs/architecture.md, The Host port):
- It takes no fd or name from the caller, only a typed key (e.g. the filesystem's index key). LinuxHost opens the state directory itself.
- The temp file is made with O_CREAT|O_EXCL, or O_TMPFILE plus linkat; never O_TRUNC.
- The rename targets only index-<fs>. ModelHost checks the same shape.
- Decide who creates index-<fs>.lock: the same state operation, or a second GATED_ALLOWED entry (Gate::Ungated with its reason, exact match count).
- Remember the scan forbids CREATE, WRONLY, TRUNC, renameat etc. outside allowlisted wrappers, and core may not touch std::fs at all.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
