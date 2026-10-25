---
id: TASK-229
title: >-
  Dry-run human summary after a deadline cut says 'would reclaim N of N
  eligible' without saying the plan is partial
status: Done
assignee: []
created_date: '2026-10-25 17:16'
updated_date: '2026-10-25 17:36'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-227: the headline says 'dry run, deadline' but the counts read as complete. Say plainly that only the candidates judged before the deadline are listed and how many were found but not judged.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 t229 (not merged): 442d535. Dry run with status deadline: headline counts prefixed 'judged before the deadline:', empty-eligible verdict 'not reachable by these', plus a line 'the deadline passed: only the candidates judged before it are listed, not any found but not yet judged; run again with a larger --timeout for a complete plan'. Live deadline reclaim: line 'the counts cover only the candidates reached before it; run again, with a larger --timeout, to reclaim more'. No number: the found-but-unjudged count exists only on stderr (planner log 'stopped early: N found but not judged'), not in Report. Keyed on status alone, so a plan complete before a later deadline (held scan) also gets the line. Tests: report::tests::a_dry_run_cut_by_the_deadline_says_its_plan_is_partial, a_reclaim_cut_by_the_deadline_says_its_counts_are_partial (red on master). just check exit 0. Host RC --timeout 30s exit 124, 0 judged (3330 found not judged on stderr); --timeout 120s exit 124, 1672 eligible, 86 kept.

Merged to master as a68abd6 on 2026-10-25T17:36Z.
<!-- SECTION:NOTES:END -->
