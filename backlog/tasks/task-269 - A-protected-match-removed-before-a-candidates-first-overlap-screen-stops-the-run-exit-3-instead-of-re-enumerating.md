---
id: TASK-269
title: >-
  A protected match removed before a candidate's first overlap screen stops the
  run (exit 3) instead of re-enumerating
status: To Do
assignee: []
created_date: '2026-10-31T03:39:00Z'
labels:
  - bug
  - safety
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. mped SHOULD 2: walk_fs and indexed.rs judge_from_index call run_error when overlaps_checked errs; protected_before_reclaim_inner and tier_policy DeletePolicy::skip re-enumerate with scan_protections instead. TASK-200 says a removed match (cargo clean of a protected target) must not stop the run. Fix: one Reclaim::protected(candidate, stage) used at every stage: on Err re-enumerate, then decide. Model test red first on the discovery and the index path.
<!-- SECTION:DESCRIPTION:END -->
