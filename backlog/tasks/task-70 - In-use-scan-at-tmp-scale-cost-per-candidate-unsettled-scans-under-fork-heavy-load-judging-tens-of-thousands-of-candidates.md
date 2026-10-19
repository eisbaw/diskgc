---
id: TASK-70
title: >-
  In-use scan at /tmp scale: cost per candidate, unsettled scans under
  fork-heavy load, judging tens of thousands of candidates
status: To Do
assignee: []
created_date: '2026-10-18 11:06'
updated_date: '2026-10-19 23:49'
labels:
  - wave-2
  - spike
  - safety
dependencies: []
references:
  - docs/safety.md#in-use
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: task-68 dry-ran fs profiles on a real laptop (155,000 top-level /tmp entries, ~500 processes / 2,200 threads, load 10 to 90 from agents' builds). Measured:

- One process scan costs ~0.5 s wall unloaded (1.7k pidfd_open, 110k statx), several seconds loaded; perf shows the CPU in /proc maps parsing and musl malloc lock contention (__lock 47%, malloc/free 15%) across the parallel /proc readers.
- With inuse::Checker's reuse (<= FRESHNESS), a scan per candidate made a 328-candidate dry run take 586 s; reusing one scan for 60 s (the spike's DRY_RUN_SCAN_REUSE, dry run only) took it to 7 s. A reclaim cannot do that: each deletion needs a scan <= 1 s old, so candidates spread over a large tree (or found between long walks) each pay a scan.
- Judging alone (no scan per candidate) still ran ~55 candidates/s at nice 19 / ionice idle: nix-shell-rc (105k candidates, 350 MiB) did not finish in 20 min (exit 124); nix-shell-tmpdir (41k) took 18 min. The PRD's time-to-first-deletion is fine, but a reclaim of /tmp at this scale spends its timeout on tiny candidates.
- Under fork-heavy load the scan does not settle (16 listings bringing new pids) and blocks EVERY candidate as in_use for as long as it is reused: 13 of 328 candidates in one run. The spike reuses a blocking scan only within REUSE_LIMIT.

Scope (to decide with task-18 / task-29): batch the fs reclaim so one fresh scan clears every candidate checked within its freshness window (discover, walk, then check many against one scan right before deleting them); retry an unsettled scan after a short pause instead of blocking; cut the scan's allocations (or use a non-global-lock allocator in the static build); decide whether tiny-candidate profiles (nix-shell-rc: 3 KiB each) are worth per-candidate checks or need a cheaper class.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A reclaim of 40k small /tmp candidates on a model of this machine stays within one scan per freshness window, not one per candidate
- [x] #2 An unsettled scan is retried before it blocks candidates; the limit stays documented in docs/safety.md
- [x] #3 Scan cost on the real laptop is measured again after the change and recorded
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-68's review (mped S4): there are now two scan-reuse policies. inuse::Checker clamps reuse to FRESHNESS (the cargo dry run and every reclaim use REUSE_LIMIT, 0.5 s), and fs_kind::plan::Scans reuses a scan for DRY_RUN_SCAN_REUSE (60 s), except a blocking scan (0.5 s). Do: one scan-reuse policy in inuse with a plan mode (no freshness bound, never reusing a blocking scan past REUSE_LIMIT) used by both dry runs, then delete fs_kind::plan::Scans and fix the reference.md --dry-run sentence, which today states the difference (cargo 0.5 s, fs 60 s).

2026-10-20 implementation in isolated task70 worktree (uncommitted): one Checker policy for fs and Cargo dry runs (60 s clear scans, 500 ms blocking scans); deleting Checker reuses a settled scan strictly under the 1 s freshness bound and checks again at clear and delete entry; an unsettled scan waits 20 ms and is retried once before judging. Model tests cover transient/persistent fork growth, 500 ms blocked replacement, exact 1 s boundary, and 40,000 checker calls with a simulated 500 ms scan plus 400 ms candidate work; this is a scan-cadence model, not a full 40,000-directory reclaim. Isolated nix develop -c just check passed on 179ebb3. Real-host scan timing and end-to-end gate remain pending; do not mark AC1/AC3 done from these tests alone.

2026-10-20 verified in isolated task70 worktree: fs_kind::discover plus fs_kind::reclaim completed 40,000 distinct /tmp/entry-* file candidates on the in-memory host model in 43.397 s; exactly 40,000 unlinkat calls and two process scans, with the model clock advanced exactly 1 s after unlink 20,000. This exercises the deletion protocol but omits executor space sampling because ModelHost statvfs recomputes all reachable inodes per candidate and is quadratic at this size. The new deadline model test passes: an unsettled first scan gets its one retry, retry crosses the deadline, and the candidate remains unjudged with exit 124. Isolated nix develop -c just check passed in 62.41 s after correcting that test expectation. Read-only real laptop LinuxHost raw scans in the GNU debug dev build took 431, 393, 411 ms wall (internal scan timing 424, 386, 404 ms); 514-515 processes, six listings each, all settled and unblocked. The raw scan algorithm was not sped up; the change amortizes a scan over candidates within strict freshness. Built-in /tmp profile real-host timing on this patch, integrated e2e, QA and MPED review remain pending. No live deletion. Keep task To Do until integrated gates and review.

2026-10-20 integrated review: task-70 protocol change rebased on 495739b. A 1 s deleting scan reuse caused five Cargo race E2E failures; restored the 500 ms deletion reuse, retaining the strict 1 s deletion freshness bound. The 40,000 candidate direct fs protocol model completes with exactly two scans at a 500 ms virtual clock boundary and 40,000 unlinks; plan scans can reuse for 60 s. Slow blocking scans now reuse for 500 ms from completion, zero reuse requests are respected, a transient unsettled scan retries once after 20 ms, and a final-candidate scan crossing the deadline reports Deadline. Real-host raw scans on the laptop were 431/393/411 ms wall for 514-515 processes; raw scan CPU was not improved. Integrated Nix just e2e passed 90/90; QA and MPED review GO. The 40k model omits executor statvfs and advances only virtual time: it proves scan cadence, not 40k real-host throughput. Per-candidate walk cost and built-in /tmp real-host timing remain open, so TASK-70 remains To Do.
<!-- SECTION:NOTES:END -->
