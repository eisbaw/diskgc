---
id: TASK-247
title: 'JSON report: count the synchronous tally''s host failures'
status: To Do
assignee: []
created_date: '2026-10-26 08:22'
labels:
  - contract
  - irreversible
dependencies:
  - TASK-31
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Since TASK-31 a host failure (EIO, EMFILE, ENOMEM) in ensure's synchronous tally keeps every tier off the index queue and is only logged (executor/indexed.rs sync_tally); the run error the report carries comes from the tier's own discovery when the failure persists. A transient one leaves no trace in the JSON. Decide whether the report counts it (e.g. in the index block, with TASK-244) and pin it in reference.md and the insta snapshots in the same commit.
<!-- SECTION:DESCRIPTION:END -->
