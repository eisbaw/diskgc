---
id: TASK-263
title: >-
  Reclaiming modes under hidepid or a non-initial PID namespace: refuse or give
  in_use_unknown instead of a silently weakened in-use scan
status: To Do
assignee: []
created_date: '2026-10-31T02:34:00Z'
labels:
  - safety
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-119 design review (mped, 2026-10-31): ProtectProc=invisible and PrivatePIDs=yes are what systemd-analyze security recommends; an operator adding them to the system template (CI exposure) or running diskgc in such an environment makes the in-use scan miss processes (hidepid hides newgrp/sg processes; a private PID namespace shows only the unit's own). The code already records both facts at startup (PidNamespace { initial, hidepid }) but uses them only for liveness contracts. Decide: under hidepid on the held /proc, reclaiming modes refuse at start (exit 2 naming the cause) or every candidate is in_use_unknown; for a non-initial PID namespace, a rule that keeps the rootless container test harness working (the e2e containers run in their own PID namespace by design). PRD 'Limits' already names hidepid; this makes the weakening loud instead of silent. Applies to 0.1 too (check the released behaviour first).
<!-- SECTION:DESCRIPTION:END -->
