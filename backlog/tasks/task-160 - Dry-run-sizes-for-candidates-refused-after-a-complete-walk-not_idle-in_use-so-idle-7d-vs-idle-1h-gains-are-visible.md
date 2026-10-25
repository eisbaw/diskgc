---
id: TASK-160
title: >-
  Dry run: sizes for candidates refused after a complete walk (not_idle,
  in_use), so idle-7d vs idle-1h gains are visible
status: To Do
assignee: []
created_date: '2026-10-24 23:38'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up of TASK-147 (branch t155). The dry run now says what a reclaim would take and whether the goal is reachable, but a kept row has no size: a laptop owner still cannot see what a stricter tier would gain (walk tmp/walks/2026-10-25-t155/23-laptop-dryrun-after.txt: 11 kept, no sizes).
Cargo eligibility (cargo_target::eligibility) refuses not_idle and in_use AFTER walk_target completed, so the tally is complete; it drops it with the Skip. Fix: return the completed tally with such refusals (Skip or a wrapper), put it in plan[].estimated_bytes and the human kept row.
Contract: docs/reference.md says plan estimated_bytes is null for every skipped candidate (rationale: partial walk). That rationale is false for post-walk refusals; changing it is a JSON value change (null -> number) on an irreversible surface: decide, document, and update the contract test. Same for fs_kind judge.
<!-- SECTION:DESCRIPTION:END -->
