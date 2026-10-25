---
id: TASK-173
title: >-
  Cargo reclaim: one in-use check per part costs a scan each once scans exceed
  ~450 ms (protocol decision)
status: Done
assignee: []
created_date: '2026-10-25 02:19'
updated_date: '2026-10-25 05:20'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision 2026-10-25 (mark-emulator for Mark, delegated): "let one scan serve all parts of a target as long as each part's delete starts inside the 1 s bound. dont touch the 1 s rule. one review round, its the deletion path." Implemented on branch t173 (c675e85): cargo_target::reclaim_checked runs under Checker::reusing_for(FRESHNESS); a kept scan that expires during a part's check or before its delete is replaced without spending a FreshAttempts attempt (generic path, also fs tiers); FRESHNESS, skip-on-stale, the shared 3-attempt budget and the lock protocol unchanged. docs/safety.md Freshness + Reclaim protocol step 3 state the change, why it is safe and the availability cost (waiting Cargo / non-lock processes seen up to 1 s instead of ~0.5 s later). Race tests 2/3/clean -p/split/shared now resume the paused driver only after FRESHNESS (race 2 failed without that: the reused scan served the last part). Review: one read-only mped-architect round, ship, no blockers.

Walk (tmp/walks/2026-10-25-t173/, own container walk-diskgc-t173 with uid slot 19 and --init, TASK-115 load shape, ~293 processes; release-01-candidate: before = master l5vk3 (incl. TASK-145), after = t173 hyydpw; same seed, reset before each run, interleaved before/after). Host load was 29-68, NOT the asked 15-25 (other agents' builds; not controllable). Rounds 11-17: goal met after 4/7, before 2/7. 11 (load 29-32, scans fit): both ~0.3-0.5 s per target. 12 (load 29-39): after 0.7-3.5 s per target, all 6 reclaimed in 8.9 s; before 6.8-10.7 s per target, 3 in_use_unknown, 53.8 s. 13 (load 37): after worse (1 target, 10 in_use_unknown), before met (one target 8.6 s). 14 (load 48-59): both nothing (scans 1.2-2.2 s > FRESHNESS: unchanged rule). 15-17 (load 54-65): after met 2/3 (per target 0.4-7.5 s), before 0/3 (all in_use_unknown). Where scans exceed 1 s neither version can delete (by design); the change helps in the 0.45-1 s band. Noisy at this load; no load-15-25 series was possible today. e2e-one (1.99.1, load 30-65): race 2 failed on the first t173 commit (reused scan served the last part; reclaimed with Cargo waiting); fixed in the harness (resume after FRESHNESS); then race 2, race 2 clean, race 2 split, race 3, shared target dir, race 8, running cargo, reclaim deletes only inside locked dirs: all pass.

Merged to master as 11182b7 on 2026-10-25T05:20Z. Walk ran at load 29-65 (not 15-25): goal met 4/7 after vs 2/7 before; repeat at quieter load is a follow-up.
<!-- SECTION:NOTES:END -->
