---
id: TASK-244
title: 'ensure: fill the report''s index block once the snapshot defines its source'
status: To Do
assignee: []
created_date: '2026-10-26 08:05'
labels:
  - contract
  - irreversible
dependencies:
  - TASK-30
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Since TASK-31 ensure builds and uses an index, but the JSON report's `index` stays null (reference.md says so until TASK-30). Define and fill source (sync|snapshot), age_seconds, units, blocks, untallied, coverage from the run's TallyState and SyncReport (executor/indexed.rs sync_tally logs these today); insta snapshots and reference.md in the same commit.
<!-- SECTION:DESCRIPTION:END -->
