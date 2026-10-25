---
id: TASK-207
title: >-
  e2e fs_reclaim_100k test counts JSON action rows, not candidates (fails since
  the 1,000-row overflow)
status: Done
assignee: []
created_date: '2026-10-25 11:37'
updated_date: '2026-10-25 21:28'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
gates a5ccbca (unmerged): reclaimed_count sums each action row's count and validates the fold shape (<=1,000 distinct eligible detail rows with count 1, then one last null-candidate summary row); unit tests in fs_reclaim_suite::tests. Evidence: nix develop -c just e2e-one fs_reclaim_100k_entry_tmp_deletes_every_eligible_candidate_and_nothing_else exit 0 (152.9 s); fast sibling exit 0 (41.5 s); e2e-clippy/e2e-tests/e2e-fmt flake checks exit 0.

Merged to master as a11e77b on 2026-10-25T21:28Z.
<!-- SECTION:NOTES:END -->
