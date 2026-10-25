---
id: TASK-187
title: >-
  Dry-run JSON: a failed in-use scan shows held_by_deleted_files: [] (same as
  none)
status: To Do
assignee: []
created_date: '2026-10-25 05:34'
updated_date: '2026-10-25 08:39'
labels:
  - shipping
  - json
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-166 walk: in a dry run where no candidate reaches the in-use step, a failed scan appears only on stderr and the JSON says held_by_deleted_files: [], indistinguishable from 'none held'. JSON contract change: needs a decision (e.g. null when unknown).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision 2026-10-25 by mark-emulator (Mark delegated; Mark may veto): null when unknown, never []. A failed scan gives held_by_deleted_files: null in live and dry runs.
<!-- SECTION:NOTES:END -->
