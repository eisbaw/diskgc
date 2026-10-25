---
id: TASK-184
title: pytest-tmp judges only ~50-150 of ~6400 candidates in 90-120 s on the laptop
status: To Do
assignee: []
created_date: '2026-10-25 05:34'
labels:
  - shipping
  - performance
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-166 host read-only dry runs (2026-10-25, ~3500 processes, load high): ensure --dry-run --select profile:pytest-tmp /tmp judged only about 50-150 of roughly 6400 candidates in 90-120 s; an earlier 300 s run judged 446. pytest-tmp is the main user value of 0.1 (~17.5 GiB on Mark's laptop, TASK-112 step 5 host dry run took 16:35 for 6401 sessions). Profile where per-candidate time goes (in-use scan per candidate vs per batch; pytest liveness facts; pytest-of listing every check) and cut it at the root, within the Freshness rules. Walk with the release artifact on a host-shaped load and many sessions (just walk-up NAME, WALK_TMP_BULK).
<!-- SECTION:DESCRIPTION:END -->
