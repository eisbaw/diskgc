---
id: TASK-117
title: 'Design: watch machine interface, exit codes, reload and signal semantics'
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
Design before implementation (audit 2026-10-23 of docs vs PRD). Deliverable is docs text, reviewed by mped-architect, not code. docs/reference.md 'watch' is planned with intended syntax only. Missing: exit-code table, startup and fatal-error behaviour, whether config is re-read (SIGHUP is not mentioned anywhere in docs/), structured per-reclaim output (PRD: every action produces one log line and one JSON actions entry; watch today logs to stderr only), one-process-per-filesystem rule, snapshot write on signal exit, signal during tally, --tally-only semantics (single-writer lock? snapshot cadence? CI job lifecycle), back-off classification of mixed outcomes and whether --interval sampling continues during back-off.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 docs/reference.md watch section specifies exit codes, --json/event output, reload/SIGHUP, startup/fatal behaviour, --tally-only semantics
- [ ] #2 docs/testing.md names watch scenarios incl. the PRD 'steady between thresholds => zero deletions, duty cycle kept' test
- [ ] #3 mped-architect design review GO recorded in notes
<!-- AC:END -->
