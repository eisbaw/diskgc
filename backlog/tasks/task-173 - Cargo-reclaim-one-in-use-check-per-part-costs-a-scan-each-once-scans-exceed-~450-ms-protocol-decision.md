---
id: TASK-173
title: >-
  Cargo reclaim: one in-use check per part costs a scan each once scans exceed
  ~450 ms (protocol decision)
status: To Do
assignee: []
created_date: '2026-10-25 02:19'
labels:
  - shipping
  - safety
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-145 (tmp/walks/2026-10-25-t145/, strace timeline 41-43-after-strace.txt). Per Cargo target the protocol makes one scan under the locks (step 2) and one in-use check per part (step 3: .fingerprint, binaries, .d files, incremental, build, deps; 6 per classic profile). Freshness lets one scan serve only checks made <500 ms after its start, so once a scan takes >~450 ms every part costs its own scan: (parts+1) x scan time, 5-13 s per target at load 18-34 on 14 cores, vs ~0.3 s when scans fit. TASK-145 cut the scan's cost (allocator contention) which moves the cliff but does not remove it. Removing it needs a protocol change that only Mark can decide: e.g. check all remaining parts against one fresh scan and delete them while that scan is <1 s old, or a coarser part granularity. Trade-off: the per-part check is also how diskgc stops early for a waiting Cargo of the same lock set (docs/safety.md cargo-target step 3); safety rests on the .cargo-lock invariant, availability for that Cargo would change.
<!-- SECTION:DESCRIPTION:END -->
