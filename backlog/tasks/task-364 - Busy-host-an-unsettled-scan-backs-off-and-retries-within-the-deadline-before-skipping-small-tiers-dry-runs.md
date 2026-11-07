---
id: TASK-364
title: >-
  Busy host: an unsettled scan backs off and retries within the deadline before
  skipping (small tiers, dry runs)
status: Done
assignee: []
created_date: '2026-11-06T05:36:00Z'
updated_date: '2026-11-07T15:30:00Z'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From TASK-348 (d86ee975): classification now lives in the reader (ProcError::Unsettled -> blocked_by Busy; UnknownCause::stays() is the single rule). TRAP: InUse::rescan (the 20 ms retry) and reusable_at still key on the scan-level 'unsettled' field; key them on blocked_by() == Busy instead, or one exiting linker still defeats small tiers and dry runs. Also from TASK-348: a task stuck mid-exit holds a tier to its deadline; consider bounding (skip the rest after the same pid blocks K back-offs).

2026-11-07 orchestrator: stacked branch t364-busy-retry in tmp/worktrees/t364 = master + TASK-317 (a63812dd) + TASK-348 (c842655c, c24b9a89) + TASK-364 + TASK-366, gated by ONE full e2e. Reason: TASK-317's gate failed on release_candidate_pip_cache_* (single-candidate tier, 'process list still growing ... 5 new', host load ~1, serial tail) -- this task's bug; TASK-348 waited on TASK-317; circular.

Merged in the stack ending 8fd85e87d0df6a586c50afcc8f6d9d8dddf4d2df (2026-11-07).

Open for Mark: ensure --dry-run on a busy host can now wait up to --timeout (default 10m) instead of answering at once. Follow-ups: stale-scan skips still wait only after 3 in a row; stuck tracker per tier; pin 'settled scan restarts the 30 s'; idle_age_seconds computed before waits; no container test stages churn on purpose.
<!-- SECTION:NOTES:END -->
