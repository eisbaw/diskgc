---
id: TASK-244
title: 'ensure: fill the report''s index block once the snapshot defines its source'
status: To Do
assignee: []
created_date: 2026-10-26 08:05
updated_date: '2026-10-31T00:59:00Z'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From TASK-30 (branch t30-snapshot): the snapshot now exists. snapshot::RunIndex::seed knows where a run's index started (a valid snapshot, its write time = age_seconds; or nothing: missing, ignored with a reason, --no-index); it logs it on stderr but does not yet record it for the report. Fill report.index from it (source snapshot|sync, age_seconds, units/blocks from Index::stats, untallied from SyncReport, coverage from per-root complete listings), update insta snapshots and reference.md together. TASK-30's model tests (diskgc-model tests/snapshot.rs) are the natural place to add source assertions.

From TASK-32: watch's JSON lines carry the same index block, null today as in ensure's report. When it is filled: watch records say source: live and age_seconds = the age of the least recently listed selected (profile, root), null while one has not been listed since start (reference.md, Watch JSON lines); the background tally's TallyState::listed_at has that per finder.
<!-- SECTION:NOTES:END -->
