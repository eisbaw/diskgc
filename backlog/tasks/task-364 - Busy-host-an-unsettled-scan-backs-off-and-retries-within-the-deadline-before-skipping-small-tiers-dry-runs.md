---
id: TASK-364
title: >-
  Busy host: an unsettled scan backs off and retries within the deadline before
  skipping (small tiers, dry runs)
status: To Do
assignee: []
created_date: '2026-11-06T05:36:00Z'
labels:
  - freshness
  - busy-host
  - '0.2'
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-363 investigation (t360 implementer, 2026-11-06; evidence tmp/evidence-t363/t360-investigation/): an unsettled process scan is retried once after 20 ms, then the candidate is skipped in_use_unknown and never rechecked; TASK-340's back-off only pauses before the NEXT candidate after 3 skips in a row, so a tier with 1-2 candidates never waits (model test two_stale_candidates_in_a_row_cost_no_back_off pins this) and dry runs/plans have no retry at all. Observed: release_candidate_ccache_* ended exhausted after 3.7 s with ~116 s of its 120 s deadline left, on the e2e suite's own churn (median 68 new processes/s, peaks 231). On a real busy laptop (load 20-40) small tiers and dry runs give up on the first churn. Do: retry an unsettled scan with bounded back-off (and/or re-check busy-skipped candidates while backing off) within the deadline, for plans too; keep the guarantee (never delete on a scan older than FRESHNESS, a stale scan clears nothing); deterministic model tests; prove under the e2e fast suite's real parallel churn (the only known reproducer) that the affected release tests pass because the back-off waits it out. Start after TASK-348 merges (same executor area).
<!-- SECTION:DESCRIPTION:END -->
