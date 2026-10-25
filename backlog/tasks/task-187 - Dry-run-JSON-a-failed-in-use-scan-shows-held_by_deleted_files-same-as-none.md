---
id: TASK-187
title: >-
  Dry-run JSON: a failed in-use scan shows held_by_deleted_files: [] (same as
  none)
status: To Do
assignee: []
created_date: '2026-10-25 05:34'
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
