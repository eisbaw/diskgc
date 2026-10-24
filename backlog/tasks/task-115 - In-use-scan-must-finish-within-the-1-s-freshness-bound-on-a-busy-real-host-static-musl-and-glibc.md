---
id: TASK-115
title: >-
  In-use scan must finish within the 1 s freshness bound on a busy real host
  (static musl and glibc)
status: To Do
assignee: []
created_date: '2026-10-23 15:53'
updated_date: '2026-10-24 13:04'
labels:
  - performance
  - inuse
dependencies: []
references:
  - docs/safety.md#in-use
priority: high
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
- [ ] #4 A reclaim inside a container with --pid=host on this laptop deletes a disposable candidate with the shipped static binary (no freshness error), repeated 10 times with load recorded; or the freshness design change is decided and implemented with its safety argument in docs/safety.md
- [ ] #5 Per-scan time on this host recorded for static musl and glibc builds before and after, with process count and load
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-24 from TASK-112 step 5 (master dcfd410): the framing 'not a safety defect: slower scans only cost time' is wrong once a scan exceeds inuse::FRESHNESS (1 s): Checker::check rescans once and then fails the run, so a reclaim deletes nothing. Measured read-only on the laptop (glibc release build, ~660 processes, 350 of the user, load ~10, one-candidate dry run of a scratch probe, 12 interleaved runs): 1.59 s median without a liveness contract, 1.69 s with pytest's facts (30 runs later: median 1.97 s); strace -c of one scan: ~2.1 M syscalls (statx ~515 k, close ~250 k). The pytest facts add ~2,200 openat2 and ~4,500 pidfd_open. Proposed (mped-architect review of step 5): merge proc_uids into the NSpid status read; batch root lookups per pid (one pidfd, one root fd); dedup fallback lookups by (mnt ns, root mnt_id, dev/ino); under --allow-root skip cmdline and lookups for processes whose uids are not judged; and re-measure with a REAL reclaim in a container at a host-like process count, which can fail on freshness.

Orchestrator 2026-10-24: ESCALATED to High, 0.1 blocker. The description's 'not a safety defect: slower scans only cost time' is WRONG for reclaim: Checker errors the run when two scans in a row finish older than FRESHNESS (1 s, crates/diskgc-core/src/inuse/mod.rs; docs/safety.md 'Freshness'). TASK-112 step 5 measured 1.6-2.0 s per scan with the glibc build on this laptop (~660 processes, load ~10); musl is 3-6x slower. So a real ensure on Mark's laptop would clear nothing, any kind. Containers never showed it (few processes). Approach: (1) prove the failure end to end: a reclaim in a container with --pid=host (sees the host's process table) on a disposable candidate, expect the freshness error; (2) profile where scan time goes (per-process fds/maps/environ, musl malloc lock, Brave/Electron processes with thousands of fds) and cut it (allocations, allocator, avoiding redundant reads, reading only what the selected sources need); (3) only if the bound cannot be met with margin, propose design options for the freshness rule (e.g. age measured per process read rather than from the first read) with their race-window consequences, for an orchestrator/mark-emulator decision — do NOT change the bound on your own. Mark's 2026-10-24 ruling for this task's AC #2 goes to mark-emulator.
<!-- SECTION:NOTES:END -->
