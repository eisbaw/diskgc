---
id: TASK-115
title: >-
  In-use scan must finish within the 1 s freshness bound on a busy real host
  (static musl and glibc)
status: In Progress
assignee: []
created_date: '2026-10-23 15:53'
updated_date: '2026-10-24 16:01'
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
- [x] #1 perf profile of the static build's scan on a real host recorded; the dominant cost is named
- [x] #2 The static build's one-candidate dry run on a comparable host is within 1.5x of the glibc build, or the remaining gap is explained and accepted by Mark
- [x] #3 Change measured on the real host again (read-only) and recorded with command, exit code, process count and load
- [x] #4 Per-scan time on this host recorded for static musl and glibc builds before and after, with process count and load
- [x] #5 A reclaim of a disposable candidate inside a disposable container with a host-shaped process load (this laptop's measured shape: ~281 user processes, ~2,800 tasks, plus CPU load from another user), with the shipped static binary, deletes it without a freshness error, 10 reclaims per build (static and glibc) with load recorded; or the freshness design change is decided and implemented with its safety argument in docs/safety.md
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-24 from TASK-112 step 5 (master dcfd410): the framing 'not a safety defect: slower scans only cost time' is wrong once a scan exceeds inuse::FRESHNESS (1 s): Checker::check rescans once and then fails the run, so a reclaim deletes nothing. Measured read-only on the laptop (glibc release build, ~660 processes, 350 of the user, load ~10, one-candidate dry run of a scratch probe, 12 interleaved runs): 1.59 s median without a liveness contract, 1.69 s with pytest's facts (30 runs later: median 1.97 s); strace -c of one scan: ~2.1 M syscalls (statx ~515 k, close ~250 k). The pytest facts add ~2,200 openat2 and ~4,500 pidfd_open. Proposed (mped-architect review of step 5): merge proc_uids into the NSpid status read; batch root lookups per pid (one pidfd, one root fd); dedup fallback lookups by (mnt ns, root mnt_id, dev/ino); under --allow-root skip cmdline and lookups for processes whose uids are not judged; and re-measure with a REAL reclaim in a container at a host-like process count, which can fail on freshness.

Orchestrator 2026-10-24: ESCALATED to High, 0.1 blocker. The description's 'not a safety defect: slower scans only cost time' is WRONG for reclaim: Checker errors the run when two scans in a row finish older than FRESHNESS (1 s, crates/diskgc-core/src/inuse/mod.rs; docs/safety.md 'Freshness'). TASK-112 step 5 measured 1.6-2.0 s per scan with the glibc build on this laptop (~660 processes, load ~10); musl is 3-6x slower. So a real ensure on Mark's laptop would clear nothing, any kind. Containers never showed it (few processes). Approach: (1) prove the failure end to end: a reclaim in a container with --pid=host (sees the host's process table) on a disposable candidate, expect the freshness error; (2) profile where scan time goes (per-process fds/maps/environ, musl malloc lock, Brave/Electron processes with thousands of fds) and cut it (allocations, allocator, avoiding redundant reads, reading only what the selected sources need); (3) only if the bound cannot be met with margin, propose design options for the freshness rule (e.g. age measured per process read rather than from the first read) with their race-window consequences, for an orchestrator/mark-emulator decision — do NOT change the bound on your own. Mark's 2026-10-24 ruling for this task's AC #2 goes to mark-emulator.

2026-10-24 implementer (master ebdffae; proposal branch task115-mimalloc 98952ee, NOT merged). Raw logs: tmp/worktrees/task115-scan/tmp/t115/runs/.

FAILURE PROOF. (a) --pid=host + shifted uidmap (harness shape), static build, reclaim of a disposable probe: exit 1 exhausted, deleted in 93 ms; 595 host processes are 'other users' (one refused read each). A rootless container is a child user namespace, so cap_ptrace_access_check refuses every read of the host user's processes: NO rootless container can reproduce the host's per-process cost, and AC #4 as worded cannot fail. (b) Faithful proxy: own PID ns container, host-shaped tester load (tmp/t115/load.py: 87x24 threads x69 fds x900 maps, 57x10x22x300, 137x1x8; 281 procs, 2810 tasks) + 10 CPU burners of another user, 10 reclaims each (tmp/t115/run-container.sh): master static 10/10 exit 3 'two scans in a row were older than the freshness bound' (last scan 1.84-5.13 s, load 13-19), master glibc 10/10 deleted.

PROFILE (host, read-only; master; ~600 procs, 281 user procs/2873 threads/8481 leader fds/125k map lines, load 5-12). Per-scan (new examples/scan_timing, `just scan-timing`): glibc median 607 ms p95 743; static median 1740 p95 2601. strace of one dry run ~1.75 M syscalls: statx+readlink of fd/N ~744k (42%: every thread re-read the shared CLONE_FILES table); task stat brackets ~30% (open+statx+lseek+2 read+close per stat); anchored listings in 6 separate collects per process. perf static: musl __lock+__unlock ~60%, malloc/free/realloc ~17% of user CPU; futex 600k syscalls, mmap/munmap 85k. Hot allocators: maps line union (Vec per line), environ::paths, EnvPathIndex ancestors, Read::add clones, format! paths.

CHANGES (ebdffae): kcmp(KCMP_FILES) sharing of fd tables inside a round with a bracketed argument (docs/safety.md Threads, new limit 7); held task stat fd + pread; plain listing for maps/environ; Host::proc_links_all; allocation cuts; kcmp refusals -> scan note. Reviews: mped-architect fix-first -> fixed -> GO; qa-test-runner PASS. Gate: just check exit 0, 1034 passed 0 failed 1 ignored.

AFTER (host, interleaved, 20 scans each, read-only, 630-642 procs, load 7-16, tmp/t115/runs/final-interleaved.txt): glibc median 438-898 -> 161-306 ms (p95 171-427); static median 1484-3058 -> 294-688 ms (p95 312-1013). Container reclaim, seccomp unconfined (as the e2e harness; host has no filter), 10x: ebdffae static 8/10 (load 6-13) and 7/10 (load 13-18) deleted, the rest freshness errors (scans 1.01-1.32 s); ebdffae glibc 10/10 (load 15-21); task115-mimalloc static 10/10 (load 11-13). Default podman seccomp refuses kcmp (EPERM): note 'kcmp was refused 2514 times' shown, 6/6 still deleted.

OPEN FOR MARK (AC #2, AC #4): the static build is still 1.5-2.5x glibc and misses freshness under CPU oversubscription because musl's mallocng lock serialises the 8 readers; Part 2 = mimalloc 0.1.52 (vendored C, nixpkgs pkgsCross.musl64 gcc as CC) on branch task115-mimalloc, which closes the container gap (10/10). Decide: (1) adopt mimalloc (supply chain: C under every allocation, MIMALLOC_* env affects behaviour; build: cross gcc, reverses 'no C toolchain'); (2) keep musl malloc and cut allocations further (remaining ~180k allocs/scan, mostly environ path pairs and per-read Vecs; uncertain to close the oversubscription tail); (3) freshness-rule design options were NOT written: the bound is met with margin on the host at load <=16 for glibc and with mimalloc in the container, so step 5 was not triggered. AC #4 wording should change to the host-shaped-load container (or the --pid=host shape is meaningless).

Orchestrator 2026-10-24. Decision by mark-emulator (Mark delegated; Mark may veto): first cut the remaining allocations (~180k per scan) and try fewer parallel reader threads, measured under the host-shaped load; only if the static build's high-load tail remains, adopt mimalloc (branch task115-mimalloc, 98952ee) pinned in the flake, with MIMALLOC_* environment variables ignored or cleared at startup so they cannot change behaviour. AC #4 rewording: the --pid=host rootless container cannot see host processes (child user namespace), so the proof is the host-shaped-load container (281 processes, 2810 threads, 10 busy loops of another user), 10 reclaims per build. Evidence from removed worktrees moved to tmp/evidence/task112-step5 and tmp/evidence/task115-baseline.

2026-10-24 AC #4 reworded (coordinator/mark-emulator decision): the old wording (--pid=host container on this laptop) cannot fail. A rootless container runs in a child user namespace; cap_ptrace_access_check refuses every read of the host user's processes, so the 595 host processes were 'other users' (one refused read each) and a static reclaim deleted in 93 ms (exit 1 exhausted). The faithful proof is a host-shaped load inside the container (tmp/t115/load.py + run-container.sh in the task115 worktree).

2026-10-24 implementer, allocator decision carried out (master 8a44b3b). Raw logs: tmp/worktrees/task115-scan/tmp/t115/runs/ (rr1-3, rr-final*, rss.txt, dryrun-final.txt, allocs-a1.txt).
(1) Allocation cuts: Environ as one byte block with borrowed accessors; proc_fds/proc_links_all dedup without cloned texts; readlink into a stack buffer; preallocation. Allocations/scan (pytest facts) 680k -> 341k (remaining ~155k are environ::paths in the main thread, after the readers are joined, so not contended).
(2) Fewer readers: PROC_READERS=4 worse than 8 (round-robin, same container load: 6/10 deleted vs 9/10), 8 kept.
(3) Round-robin container reclaims in one host-shaped load (inload-rr.sh/run-rr.sh, seccomp unconfined), by load at the run, across 3 series: load 13-18: allocs build 26/26, master ebdffae 3/5, mimalloc 25/25; load >18: 13/14, 12/15, 15/15. No freshness error at 13-18 => mimalloc NOT adopted per the ruling; branch task115-mimalloc (98952ee) kept unmerged for reference. The MIMALLOC_* lock-down was therefore not implemented.
(4) Max RSS (host dry run, ~600 procs): static master 61 MB, 8a44b3b 18 MB, mimalloc build (older code) 159 MB; 30 scans in one process: 102 / 17 / 304 MB. Long-running watch memory -> TASK-134.
AC #5 (new wording): flake static 8a44b3b (/nix/store/2gq91wyd...-diskgc-0.1.0) and cargo --release glibc, round-robin, 10 reclaims each per series: load 11.4-13.5: static 10/10, glibc 10/10; load 13.4-17.4: static 10/10, glibc 10/10. 
AC #2: one-candidate dry run interleaved (bench.sh, 6 each, 602 procs, load 11): static mean 476 ms vs glibc 321 ms = 1.48x (master 2.3-3.4 s vs 0.8 s, ~3x); within 1.5x but at the margin. Per-scan (just scan-timing 20, load 12-13, 598-600 procs): pytest facts static median 224 / glibc 147 ms (p95 289/156); plain 197/132 ms (p95 212/147).
Reviews: mped-architect fix-first (from_entries dropped a trailing empty entry) -> fixed + proptest -> GO; qa-test-runner PASS. Gate just check exit 0, 1035 passed 0 failed 1 ignored; e2e-clippy/e2e-tests exit 0. Not run: just e2e (full fast gate), fuzzing of the environ target (fuzz shell not cached; disk), e2e-full.
Remaining limits: above load ~18 the static build can still miss freshness occasionally (13/14); glibc and mimalloc did not in these samples. Status left In Progress for the orchestrator (all ACs checked; full just e2e not run).
<!-- SECTION:NOTES:END -->
