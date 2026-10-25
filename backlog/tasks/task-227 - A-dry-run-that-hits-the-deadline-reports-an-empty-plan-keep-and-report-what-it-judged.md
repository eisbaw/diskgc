---
id: TASK-227
title: >-
  A dry run that hits the deadline reports an empty plan; keep and report what
  it judged
status: Done
assignee: []
created_date: '2026-10-25 16:44'
updated_date: '2026-10-25 17:16'
labels:
  - shipping
  - release-blocker
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-157 host measurements (2026-10-25): a first pytest-tmp dry run on the laptop's /tmp hit the default --timeout 10m and exited 124 with an empty plan and skipped list, discarding 5,834 judged sessions. Decision by mark-emulator (Mark may veto): don't throw judged work away - a dry run that hits the deadline reports every candidate it judged (plan rows), status deadline, exit 124; keep the 10m default; the release notes tell first runs to give more. Check the live run too: a reclaim that hits the deadline must report every action and skip so far. Also look at the 331 future_time sessions seen on the host (why: clock, filesystem timestamps?) and say whether that reason is right.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 t227 (not merged): 221269c fix, bb400c2 review follow-up. Root cause: dry_run_inner sent a passed deadline after the planner to deadline_before_plan, which cleared the plan. Now judged rows are reported (status deadline, 124); eligible rows not yet checked against protect matches are dropped. Live reclaim already kept actions/skips (test added). Host: RC from branch, --timeout 30s pytest-tmp /tmp: exit 124, 348 rows. future_time 331 = sessions with mtime 2027-01-15 (epoch 1.8e9) set by a test suite; ctimes past; reason correct; TASK-228 filed. Review: mped-architect ship, no blockers; notes 1+3 fixed; note 2 open (no model test drives plan_cargo/plan_fs_union into the deadline).

Merged to master as bb400c2 on 2026-10-25T17:16Z.
<!-- SECTION:NOTES:END -->
