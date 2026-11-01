---
id: TASK-251
title: >-
  ensure: first deletion waits for the synchronous tally to size the whole first
  tier under oldest order
status: Done
assignee: []
created_date: '2026-10-30T22:48:00Z'
updated_date: '2026-11-01T06:52:00Z'
labels:
  - wave-3
  - performance
dependencies:
  - TASK-31
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found walking TASK-31's AC4 e2e (branch t31, 2026-10-30): pass 2 of the synchronous tally sizes every stale single of the first tier before the host lock is taken, and the queue's first deletion comes only after it. Measured (rootless podman, tmpfs /tmp, dev build, under strace): 8,000 entries / 2,000 rc dirs: first deletion 82 ms of which the tally 71 ms (warm 65/54); 100,001 entries / 10,000 rc dirs (2026-10-26): 592/570 ms cold, 389/371 ms warm. Under 'oldest' the order needs no sizes (best-fit does), so on a cold index the time to first deletion is roughly the time to size the tier. The PRD prescribes this pass ('then only the first tier's stale candidates sized'), so this is a design question, not a defect: should 'oldest' tiers skip pass 2 (or size lazily, ahead of the walk) when the goal is far from the whole tier? Related: TASK-245 (reuse the tally's sizing as the read-only tally) removes one of the three walks but not this wait. Not a latency target (Mark 2026-10-25: none); only whether the wait buys anything for 'oldest'.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01 host evidence raises this: /tmp on Mark's laptop holds ~208k entries (68 GiB): nix-shell-rc 140,372 candidates (0.87 GiB, leftovers of every nix develop) + pytest-tmp 6,424 (12.25 GiB). A dry run over both took 418-511 s (default --timeout 10m). A real ensure sizes the whole first tier before the first deletion, so on this host time-to-first-deletion would be minutes; with nix-shell-rc first in a tier, nearly all of it spent on 0.87 GiB of tiny dirs. Measure the real ensure's time-to-first-deletion in a container with this shape (140k small + 6.4k large) and fix (size lazily / oldest-first without full sizing / per-tier budget).

2026-11-01 implementer (branch t251-first-deletion, 842f2f2 rebased on 794e0e1; raw logs in the t251 worktree's tmp/t251-bench/runs/, driver tmp/t251-bench/bench.py).

SETUP. Walk container (tools/walk, rootless Podman, WALK_PID_HOST=1 so the Owner PID contracts permit, host-shaped load 281 procs/4358 tasks + 2 niced burners), /tmp tmpfs 3g, new generator shape WALK_TMP_LAPTOP=140372,6424,54600 (gen.py --tmp-laptop: 195k top-level entries, 140,380 rc dirs incl. the base tree's 8, 6,424 pytest sessions in 54,600 nix-shell.* roots, bytes scaled: rc ~7 KiB, sessions 128 KiB; /tmp 2.3 GiB used). Built-ins only. Config A = tier nix-shell-rc then pytest-tmp (the user story); config B = pytest-tmp then nix-shell-rc (docs/example.yaml order). Dev build = cargo --release glibc (gate on; /.diskgc-test-clock-offset 7200 so fresh fixtures are idle). RC build = nix .#release-candidate (static musl, gate lifted, no offset: fixture aged >= 1 h). Goal A: avail+1075 MiB (whole rc tier + ~740 sessions); goal B: avail+130 MiB (~1,040 sessions). Host: 14 cores, load recorded per run.

BEFORE (a0a970e), single runs: dev A dry run 63.5 s (146,808 rows, load 4.7); dev A reclaim TTFD 9.0 s (sync tally 8.8 s, 140,380 sized), total 535 s, 141,292 deleted (load 5.4-6.9); dev B reclaim TTFD 6.3 s (tally 6.2 s, 6,428 sized), total 11.2 s; rc A dry run 52.8 s (load 2.1); rc A reclaim TTFD 7.9 s (tally 7.7 s), total 499 s, 141,294 deleted (load 3.0-7.3).
AFTER (TASK-251 code), single runs: dev A reclaim TTFD 4.7 s (tally 4.5 s, discovery 4.3 s), total 467 s (load 13.3 at start: a reset ran just before); dev B reclaim TTFD 6.9 s (discovery alone 6.7 s), total 14.2 s; rc A dry run 49.3 s (138,023 rows: fixture after the paired runs); rc A reclaim TTFD 5.0 s, total 443 s for 132,990 deleted (3.33 ms/candidate vs 3.53 before: unchanged within noise), the walk sized 131,595 units in 4.19 s.
PAIRED (interleaved, same fixture, config A, goal +5 MiB, --no-index, ~732 deleted each, 6 per build): dev master TTFD 7.5-11.3 s (median 10.5) vs after 4.3-6.4 s (median 5.6), load 5.3-6.4; rc master 7.8-8.4 s (median 8.0) vs after 4.5-4.7 s (median 4.6), load 3.3-3.8. After, the walk sized 768 units (21-33 ms) instead of 139,646.
e2e fs_reclaim_time_to_first_deletion_is_measured_cold_and_warm (8,000 entries, under strace, 14 tests in parallel): cold 76 ms (tally 53), warm 31 ms (tally 1).

FINDINGS. (1) On tmpfs pass 2 was ~3.3 s of the ~8 s TTFD; discovery (pass 1, every selected tier before the lock, PRD) is now nearly all of it (4.3-6.7 s, noisy). (2) The user story's "minutes to first deletion" overstates TTFD: --tally-budget (default min(30 s, timeout/4)) already capped the tally; on the host TTFD before was at most ~30 s + first checks. (3) The real minutes are total time: ~3.5 ms per candidate in the walk (median gap 2 ms per candidate + one in-use scan of ~0.1 s per 64 found ahead; scans ~53% of a 14,632-candidate stretch), degrading from 2.6 to 4.2 ms/candidate over the run (not investigated). 141k candidates took 467-535 s on tmpfs; on the host (dearer scans, ext4) a goal needing the whole rc tier likely exceeds the default --timeout 10m. strace cannot profile it (scans then miss the 1 s freshness bound and every candidate is skipped); perf is not in the dev shell.

NOT MEASURED: a real disk (ext4/LUKS, cold cache): tmpfs understates the I/O of both sizing and discovery; the read-only host dry run was skipped (the dry-run path is unchanged, and a dry run runs no synchronous tally, so it cannot show TTFD); warm (snapshot) runs; config B with the rc build.

FOLLOW-UPS (not filed): total walk time per candidate on a 140k tier and its degradation (profile with perf, likely before 0.1 for the laptop's /tmp); pass 1 before the first deletion (needs a PRD decision); TASK-245 now applies to the deferred sizing too (sizing and the read-only tally walk the same tree back to back); a VM (vm-cold-scale-like) reclaim of the laptop shape for disk numbers.

2026-11-01 review round (Codex + mped on 842f2f2), fixed in dd4df65 on t251-first-deletion (rebased on b525560): stored keys from an earlier run are re-read from the unit's root before the lazy merge (Codex 1, red first: two seeded cases deleted a-2 instead of a-1); the deferred work's budget window opens before the mount table read (Codex 2, red first); architecture.md/safety.md split the TASK-31 queue rule (tally before the lock) from deferred-sizing failures (run errors, possibly after earlier queued deletions; Codex 3/mped S2; TASK-31's AC #1 wording predates this); the deferral survives a root dropped at preflight and an unused one is logged (mped S1, red first); queue merge moved to executor/indexed/tier_queue.rs with a proptest (two mutants caught). Gates: nix develop -c just check exit 0; just e2e-one fs_reclaim_time_to_first_deletion_is_measured_cold_and_warm PASS (cold 47 ms, tally 32 ms, under strace) and fs_reclaim_100k_entry_time_to_first_deletion_is_measured_cold_and_warm PASS (cold 199 ms, tally 181 ms; warm 129 ms). Full just e2e not rerun (no flake or e2e code changed).

2026-11-01: merged (ff) after one review round. Remaining time problem is per-candidate cost: TASK-292.
<!-- SECTION:NOTES:END -->
