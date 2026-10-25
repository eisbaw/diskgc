---
id: TASK-207
title: >-
  e2e fs_reclaim_100k test counts JSON action rows, not candidates (fails since
  the 1,000-row overflow)
status: To Do
assignee: []
created_date: '2026-10-25 11:37'
labels:
  - shipping
  - testing
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by inuse-pass (2026-10-25): just e2e-one fs_reclaim_100k_entry_tmp_deletes_every_eligible_candidate_and_nothing_else fails '2002 actions for 16000 eligible candidates' with status exhausted, no errors and a clean deletion oracle. 2002 = 2 runs x (1,000 detail rows + 1 overflow row): report::output_actions keeps 1,000 action details and folds the rest into one row with count (25d2a0b, TASK-192). tmp_fixture sums actions.len() instead of each row's count. Tier::Full, so just e2e does not run it. Fix the test to sum count (and check the overflow row's count); not a deletion bug.
<!-- SECTION:DESCRIPTION:END -->
