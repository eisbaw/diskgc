---
id: TASK-292
title: >-
  Reclaim walk costs ~3.5 ms per candidate and slows over a run: 140k
  nix-shell-rc candidates take ~8 min (default timeout 10m)
status: To Do
assignee: []
created_date: '2026-11-01T06:19:00Z'
updated_date: '2026-11-01T06:52:00Z'
labels:
  - performance
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-251 measurement (2026-11-01, walk container, host PID ns, laptop /tmp shape 140,380 rc dirs + 6,424 pytest sessions, tmpfs): total reclaim ~3.3-3.5 ms per candidate (~2 ms per-candidate work + an in-use scan of ~0.1 s per 64-candidate batch), rate worsens 2.6 -> 4.2 ms/candidate over a run (uninvestigated). A goal that needs the whole rc tier on Mark's laptop likely exceeds the default --timeout 10m. strace distorts it (scans miss the 1 s bound); perf is not in the dev shell. Profile (add perf or cargo-flamegraph to the dev shell), find the per-candidate cost (protect checks re-reading mountinfo per candidate: TASK-274 Mounts value; container-storage probes TASK-256; CACHEDIR.TAG ancestor lookup TASK-281; scan batch size), and the growth over the run. Also consider: tiny candidates (rc dirs of a few KiB) cost the same as big ones; a per-candidate cost floor makes 140k x 6 KiB a poor trade.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: in progress in tmp/worktrees/t292 (branch t292-candidate-cost).
<!-- SECTION:NOTES:END -->
