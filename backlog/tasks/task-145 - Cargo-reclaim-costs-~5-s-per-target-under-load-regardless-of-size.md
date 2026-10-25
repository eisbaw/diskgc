---
id: TASK-145
title: Cargo reclaim costs ~5 s per target under load regardless of size
status: To Do
assignee: []
created_date: '2026-10-24 21:09'
updated_date: '2026-10-25 02:20'
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
TASK-145 (branch t145, 2026-10-25). Where the time goes (strace timeline, tmp/walks/2026-10-25-t145/02-before-strace.txt, 41..43-after-strace.txt): per target one scan under the locks plus one per part checked >=500 ms after the last scan's start; when a scan takes >~450 ms every part (6 per classic profile, 12 for debug+release) costs its own scan, so (parts+1) x scan = 5-13 s per target, vs one scan (~0.2-0.5 s) when scans fit. That is the Freshness rule, unchanged. Root of the scan's load sensitivity: the static musl build's allocator; each /proc read allocated fresh 32/64 KiB and growing maps buffers: ~5,400 mmap/munmap + ~25,000 futex per scan vs ~0 in the glibc build. Fix: per-thread scratch buffer for those reads (crates/diskgc-linux/src/scratch.rs) and no union copy for equal maps passes. After: ~2,000 mmap, ~8,400 futex per scan; scan_timing median 697->455 and 1076->756 ms (load 16-22), CPU per scan ~1.6->1.3 s. Cargo walk, same seed, interleaved: load 13-17 after max 425 ms per target vs before up to 2.1 s; load 18-20 after 2/3 runs all targets <=342 ms, before 1/3; load 24-34 goal met 3/7 after vs 1/7 before, in_use_unknown 18 vs 46. The cliff remains under sustained contention: TASK-173 (protocol decision), TASK-174 (remaining allocator gap), TASK-175 (walk tooling).
<!-- SECTION:NOTES:END -->
