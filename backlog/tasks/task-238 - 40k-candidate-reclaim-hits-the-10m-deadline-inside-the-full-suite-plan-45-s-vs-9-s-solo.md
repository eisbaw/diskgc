---
id: TASK-238
title: >-
  40k-candidate reclaim hits the 10m deadline inside the full suite (plan 45 s
  vs 9 s solo)
status: To Do
assignee: []
created_date: '2026-10-25 22:15'
labels:
  - test
  - flaky
  - release
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
RC QA on a11e77b (2026-10-25): fs_40k_small_candidates_plan_and_reclaim_scale ended status deadline after 651 s in the full Nix suite; dry plan 44.9 s in that run vs 9.4 s and a 59 s pass solo (load 5, 15-min ~15). Not resolved by the solo pass. Investigate the cause (parallel containers' process churn making scans slow -> rescans? I/O contention?); measure per-candidate time and scans per candidate under a reproduced suite load. Not a safety issue (no wrong deletion). Logs: tmp/e2e-logs-batch/rc2-a11e77b/.
<!-- SECTION:DESCRIPTION:END -->
