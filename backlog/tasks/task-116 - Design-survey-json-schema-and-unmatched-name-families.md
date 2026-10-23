---
id: TASK-116
title: 'Design: survey --json schema and unmatched name families'
status: To Do
assignee: []
created_date: '2026-10-23 16:18'
labels:
  - irreversible
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
