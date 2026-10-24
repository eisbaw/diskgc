---
id: TASK-145
title: Cargo reclaim costs ~5 s per target under load regardless of size
status: To Do
assignee: []
created_date: '2026-10-24 21:09'
updated_date: '2026-10-24 22:11'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (tmp/walks/2026-10-24/42-reclaim-unfiltered.txt, tmp/walks/2026-10-24/43-reclaim-unfiltered-2.txt; release-01-candidate, host load ~28 on 14 cores, walk container with the TASK-115 load shape).

Per target: 139 entries / 67 MiB took 5606 ms, 196 entries 5519 ms, 217 entries 5285 ms, 616 entries 6023 ms ('Cargo locks held' about the same). The cost is per target, not per entry: several ~1 s in-use scans per target. A laptop with 50 idle agent-worktree targets needs ~4-5 minutes before the goal, and ensure's default --timeout is 10m; an agent harness waits that long before its task.

Repro: just walk-up; diskgc ensure --min-free 2G --select profile:cargo-target --roots /home/tester/src /home/tester/src (stderr shows per-target ms). Measure scans per target (strace -c or a counter) and see whether one scan can serve the lock-protocol steps of one target, or targets can be batched under one scan within the freshness bound.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Correction from the same walk at 22:11Z (tmp/walks/2026-10-24/15-ci-hook-reclaim.txt): at host load ~5 the same release binary reclaimed 2636/2053/1504-entry targets in 314/278/298 ms each. The ~5 s per target was at host load 27-31 on 14 cores (another implementer's builds), so the cost is load-sensitive (the in-use scans contend), not a fixed 5 s. Lowered to medium; the question is the tail under oversubscription, which overlaps TASK-144.
<!-- SECTION:NOTES:END -->
