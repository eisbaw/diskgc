---
id: TASK-116
title: 'Design: survey --json schema and unmatched name families'
status: To Do
assignee: []
created_date: '2026-10-23 16:18'
updated_date: '2026-10-25 20:18'
labels:
  - design
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Design before implementation (audit 2026-10-23 of docs vs PRD). Deliverable is docs text, reviewed by mped-architect, not code. docs/reference.md 'survey' is marked planned and contradicts itself: unmatched name families are 'not part of the JSON contract' (reference.md ~L205) yet User profiles (~L1411) relies on survey --json to show unmatched entries, and the PRD's AI profile-authoring workflow depends on this output. Pin down: JSON envelope and field names/enums, per-profile summary fields, --list candidate records, unmatched family records and whether the family algorithm is the same one index buckets use, exit codes, --timeout / --tally-budget, cost and freshness of the in-use count.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 docs/reference.md survey section has a complete JSON schema, exit codes and flags; the unmatched-families contradiction is resolved
- [ ] #2 docs/testing.md names the survey scenarios (incl. ModelHost YAML cases)
- [ ] #3 mped-architect design review GO recorded in notes
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mark's design decisions (2026-10-23, binding): (1) survey --json is VERSIONED, MAY BREAK: a top-level schema version field; fields may change between minor releases; consumers must check the version. Not frozen like ensure's report. (2) Unmatched name families ARE in the JSON, with per family: pattern, count, total size, newest/oldest age, capped example paths. Resolves the reference.md contradiction in favour of inclusion. (3) The in-use scan is OPT-IN via a flag (e.g. --in-use): default survey reports matches/sizes/ages only; with the flag, one scan adds per-profile in-use counts.

COMPASS round 4 (2026-10-25): relabelled: Mark ruled survey JSON versioned, may break.
<!-- SECTION:NOTES:END -->
