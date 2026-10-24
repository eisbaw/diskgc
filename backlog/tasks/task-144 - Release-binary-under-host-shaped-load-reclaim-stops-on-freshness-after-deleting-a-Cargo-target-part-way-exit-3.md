---
id: TASK-144
title: >-
  Release binary under host-shaped load: reclaim stops on freshness after
  deleting a Cargo target part-way (exit 3)
status: Done
assignee: []
created_date: '2026-10-24 21:09'
updated_date: '2026-10-24 23:30'
labels:
  - shipping
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-143 walk (2026-10-24, walk-diskgc-podman, release-01-candidate /nix/store/hzhjlyqp...-diskgc-release-01-candidate-0.1.0, host load 27-31 on 14 cores, container load: tools/walk/load.py 281 procs/2803 tasks + 2 niced busy loops).

Repro: just walk-up && just walk-reset; in the walk shell: diskgc ensure --min-free 2G --select profile:cargo-target --roots /home/tester/src /home/tester/src

Seen: 3 targets reclaimed, then fw-nrf/target 'deleted part-way: 206 entries', then 'error: the run stops: .../fw-nrf/target/debug/build: in-use scan 300 processes: two scans in a row were older than the freshness bound of 1s once finished (the last took 1.283380149s)'; exit 3 at 1.9 of 2.0 GiB. Rerun on the remaining targets (same load) met the goal (exit 0). 1 of 2 runs failed. Pane dumps: tmp/walks/2026-10-24/42-reclaim-unfiltered.txt (fail), tmp/walks/2026-10-24/43-reclaim-unfiltered-2.txt (pass).

User impact: a laptop owner on a busy machine (the PRD's overnight-agents case) gets exit 3 and a half-deleted target; TASK-115 recorded this residual for load above ~18 (13/14). The walk shows it at host load ~28 with only the host-shaped load inside. Decide: mimalloc (branch task115-mimalloc), more allocation cuts, or the freshness design options; measure with just walk-up.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 DECISION (made by mark-emulator on Mark's delegation; Mark may veto): "never delete on a stale scan. retry with backoff a few times, then skip that candidate and carry on, dont fail the whole run. exit by free-space like everything else." FRESHNESS (1 s) and the fresh-scan rule are unchanged.

2026-10-25 implementer (branch t144, worktree tmp/worktrees/t144; started 00:14 CEST; NOT merged). Commits on t144: 3148d50, 89d8d45, f177641, 61ab198.
RULES (docs/safety.md In use: Freshness): a candidate gets inuse::FRESH_ATTEMPTS=3 attempts at a fresh verdict (inuse::FreshAttempts), shared by the checker's stale scans and the callers' expired clears (clear_and_delete, command action); a Cargo target's parts share the target's one budget. Pauses 250 ms then 1 s before retries, never to or past the deadline. When spent: skip with new closed reason in_use_unknown (Reason::InUseUnknown; reference.md enum). Executor: after 3 candidates in a row of a tier end in_use_unknown (any other outcome resets; per tier), the tier's remaining candidates are skipped in_use_unknown unchecked (no scans). Human summary adds: 'the host was too busy to check N candidate(s) for use (in_use_unknown): kept; run again when the host is less busy'. The two other 'expired twice' run errors (clear_and_delete, command.rs) became the same skip (same class). A scan that FAILS is still an error of the run.
TESTS: model tests in crates/diskgc-model/tests/inuse.rs (No fresh scan section: 3 attempts + pause timing, fresh on 3rd deletes, deadline cut, expired clears share budget, stale+expired+stale = one budget, stale scan kept as latest); vectors crates/diskgc-model/tests/vectors/inuse/busy-host-*.yaml (next candidate reclaimed; tier stops after 3 in a row; a reclaim resets the count); report summary test.
WALK (walk-diskgc-podman, release-01-candidate built from the branch, copied to /usr/local/bin/diskgc-t144; extra non-niced busy loops of user 'other' added inside the container, removed after; evidence tmp/walks/2026-10-25-t144/): BEFORE 0.1 'diskgc ensure --min-free 2G --select profile:cargo-target --roots /home/tester/src /home/tester/src': 01 (8 burners, load 22) rc=3 nothing reclaimed; 03 (load 26) rc=3 after a part-way delete. AFTER (f177641): 02 (load 34) and 04 (load 25): rc=1 'nothing more could be reclaimed', 2 part-way actions (+145 MiB), 3 targets in_use_unknown with detail (scans 1.0-1.7 s), then 'the rest of this tier is skipped unchecked' and 7 more in_use_unknown, summary line shown, 0 errors. 05/06 (3 and 5 extra burners, load 17-19): rc=0 met, 6 reclaimed. 07 final binary (61ab198, 10 burners, host load had dropped to ~21): rc=0 met; the skip path was NOT re-walked on 61ab198 (differs from f177641 only by keeping the stale scan as latest and the e2e probe call sites).
REVIEW: mped-architect one read-only round on 3148d50: SHIP, no blockers; notes fixed in 89d8d45 (per-target budget for Cargo, unreachable!/assert on loop fall-through, combined-budget + counter-reset tests). Not re-reviewed after 89d8d45..61ab198.
GATE: nix develop -c just check exit 0 on 61ab198 content (1048 passed, 0 failed, 1 ignored; log tmp/worktrees/t144/tmp/check-4.log); nix build .#checks.x86_64-linux.e2e-clippy .#checks.x86_64-linux.e2e-tests exit 0 on 61ab198 (first attempt on f177641 failed: diskgc-e2e host_probe.rs called Checker::check; fixed in 61ab198). Not run: just e2e (full), fuzz.
FOLLOW-UPS: (1) a command-action test for boundary-policy refusal then expiry sharing the budget (not written); (2) mixed-outcome walk at a load between 19 and 25 not captured (host load fluctuated); (3) the container's fork-storm probe now counts a stale scan as blocked rather than failing.

Merged to master as ce6a0cd on 2026-10-24T23:30Z (rebased; just check exit 0, 1048 passed). Skip path last walked on f177641 (pre-final); final differs in keeping the stale scan after a skip and e2e test changes.
<!-- SECTION:NOTES:END -->
