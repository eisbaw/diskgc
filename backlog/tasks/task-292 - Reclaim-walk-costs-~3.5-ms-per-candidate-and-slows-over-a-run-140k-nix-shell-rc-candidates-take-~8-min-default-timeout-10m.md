---
id: TASK-292
title: >-
  Reclaim walk costs ~3.5 ms per candidate and slows over a run: 140k
  nix-shell-rc candidates take ~8 min (default timeout 10m)
status: To Do
assignee: []
created_date: '2026-11-01T06:19:00Z'
updated_date: '2026-11-01T08:58:00Z'
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

2026-11-01 implementer: cb5c81c on t292-candidate-cost (not merged). Profile (perf, user-space only, paranoid=2; release glibc dev build; walk container, laptop /tmp shape, WALK_PID_HOST=1, tmpfs): in-use scans were half the wall time (one ~85 ms scan per 64-candidate batch; scan threads used as many cycles as the main thread); main thread: name_present->is_mount_point 29% of user time (12 container-storage marker probes per directory, each comparing every mount point), mountinfo parse 21% (6 reads per candidate); ~920 syscalls per candidate. Fix: found-ahead batches grow 64 -> doubling -> 1,024 (first batch and freshness unchanged); MountedNames (one mount-point lookup per directory, proptested equal to is_mount_point). Measured, interleaved, load 4-8: 200 MiB goal (29,262 cand., 3 rounds): before 2.54-3.60 ms/cand (78-110 s), after 0.99-1.12 (34.5-37.0 s). Whole tier (141,307 cand.): before 3.64 ms/cand, 521 s (mean load 6.2); after 1.47 ms/cand, 215 s (mean load 6.1). The 2.6->4.2 ms/cand drift is core placement, not O(n): user and sys per candidate double together at constant load in windows where the main thread samples on E-cores (cpu_atom PMU 60-80%). Not measured: static musl release candidate after the fix, ext4/real disk, the laptop itself, warm snapshot runs. Follow-ups (not filed): mountinfo read once per check step is now the top main-thread cost (30%, TASK-274); synchronous tally discovery ~5 s fixed; remaining is_mount_point callers 10%. Raw logs: tmp/worktrees/t292/tmp/t292-bench/runs/.
<!-- SECTION:NOTES:END -->
