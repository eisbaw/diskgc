---
id: TASK-30
title: >-
  Index snapshot: atomic write, filesystem naming, single-writer lock,
  validation, --no-index
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
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

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
