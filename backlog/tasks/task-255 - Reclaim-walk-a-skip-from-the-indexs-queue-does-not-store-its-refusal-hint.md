---
id: TASK-255
title: 'Reclaim walk: a skip from the index''s queue does not store its refusal hint'
status: To Do
assignee: []
created_date: '2026-10-30T23:38:00Z'
labels:
  - wave-3
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen in TASK-30's container walk (ensure_warms_the_snapshot): units skipped keep_marker by the walk's read-only tally come back next run with '0 with a refusal hint', so they are ranked as ordinary units again. docs/architecture.md (Index update after an action) says a skip changes the unit's checked_at and refusal hint. Only ordering is affected (every unit is judged live); check whether the walk should record Refusal hints (and checked_at) on skip, and test it on a second run from the snapshot.
<!-- SECTION:DESCRIPTION:END -->
