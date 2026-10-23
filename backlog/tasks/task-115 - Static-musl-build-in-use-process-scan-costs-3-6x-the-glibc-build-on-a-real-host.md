---
id: TASK-115
title: >-
  Static musl build: in-use process scan costs 3-6x the glibc build on a real
  host
status: To Do
assignee: []
created_date: '2026-10-23 15:53'
labels:
  - performance
  - inuse
dependencies: []
references:
  - docs/safety.md#in-use
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured 2026-10-23 for TASK-70 AC3 (read-only, this laptop, 581 processes, load 3-7): one-candidate ensure --dry-run --no-index with a user profile over the repo's tmp/host-dryrun-20261023/scanprobe (cost dominated by one in-use process scan plus startup). Static musl default package (nix build .#packages.x86_64-linux.default, sriswxh4…-diskgc-0.1.0): 2.22/2.86/4.20 s wall, 4.65-8.28 s user + 5.82-10.45 s sys, ~82 MiB max RSS. Same source, cargo --release glibc build: 0.71/0.73/0.72 s wall, ~0.75 s user + ~1.9 s sys. GNU debug build: 2.10-2.54 s. task-68's perf of the static build put the CPU in /proc maps parsing and musl malloc lock contention (__lock 47%, malloc/free 15%) across the parallel /proc readers; TASK-70 scoped but did not do 'cut the scan's allocations (or use a non-global-lock allocator in the static build)'. The shipped artifact is the static musl one, so reclaim throughput on a busy host is bounded by this. Not a safety defect: slower scans only cost time and freshness re-scans.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 perf profile of the static build's scan on a real host recorded; the dominant cost is named
- [ ] #2 The static build's one-candidate dry run on a comparable host is within 1.5x of the glibc build, or the remaining gap is explained and accepted by Mark
- [ ] #3 Change measured on the real host again (read-only) and recorded with command, exit code, process count and load
<!-- AC:END -->
