---
id: TASK-31
title: >-
  Reclaim walk on the index: cheap re-check, stale units, index-is-not-authority
  properties
status: Done
assignee: []
created_date: 2026-10-16 22:41
updated_date: '2026-11-02T14:35:00Z'
labels:
  - wave-3
  - irreversible
  - safety
dependencies:
  - TASK-29
  - TASK-182
  - TASK-196
references:
  - docs/architecture.md
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the index must change speed, never safety. Integrate the index into the reclaim walk: queue per tier, cheap identity re-check, stale skips, per-tier discovery after the queue, live checks unchanged; property tests with randomly stale/partial/corrupted indexes; the stale-index model scenarios from docs/testing.md.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Property: with a corrupted index nothing is deleted that a correct-index run would refuse; the deleted set is within what that run's own discovery finds, with two stated exceptions (docs/testing.md, Property tests): re-matching a unit looks names up instead of listing, so a directory over the listing limit is not refused on the way down (too_many_entries; every per-candidate check still applies), and a host failure first appearing after the synchronous tally can follow deletions of queued candidates (one the tally meets keeps the run off the queue)
- [x] #2 Stale-index scenarios (deep change, new keep file, new Cargo target inside, new protected match) are refused at deletion time
- [x] #3 With an index present, new stale cruft created after the snapshot is deleted before any pressure tier runs
- [x] #4 Time to first deletion on the 100k-entry fixture, cold and warm, is measured and logged (not a pass criterion: no latency target); ensure reaches --min-free and exits 0 in both
- [x] #5 A block member opened by a process after an earlier member of the same block was deleted is skipped as in_use
- [x] #6 A lump (block across finder sets, TASK-28) whose site lies above a tier's bound root is expanded by that tier's own discovery from the deeper of the site and the bound root, never from above the root; every member is still checked live (model test)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Orchestrator 2026-10-23 (design audit): time-to-first-deletion (PRD: 10 s on a warm 100k-entry /tmp) has no defined start/stop events, clock, or JSON report field; define them here and add the measurement to the report. The index-authority property (testing.md ~L471) is deferred to this task.

Forward-carried from TASK-64 (2026-10-23): a flock (or a bound Unix socket) is released when the LAST reference to its open file goes, which can be after close()/exit returned: any /proc/<pid>/fd reader (an in-use scan, diskgc's own included, lsof) holds a transient reference for the length of its syscall (measured up to 0.5 ms on 7.2; about 1 in 60 immediate retries busy under four readers). Never assert a release with one attempt right after a close; wait with a bound (RELEASE_BOUND, docs/testing.md Negative feedback). A held-lock assertion needs no wait.

Correction to the TASK-64 forward-carry above (2026-10-23, measured): which /proc reads hold a file reference depends on the kernel (fs/proc/fd.c): reading /proc/<pid>/fdinfo on every supported kernel, statting or reading an fd link from 5.11, listing the fd directory from 6.7. diskgc's scan lists and stats fd links, so it can delay another program's lock release only from 5.11 on; on 5.8.16 those reads gave no late release in about 1.7 million attempts. A test still waits with RELEASE_BOUND on every kernel (other readers, e.g. lsof, read fdinfo). docs/safety.md Host lock has the table.

Orchestrator 2026-10-24 (COMPASS round 2): takes TASK-108's AC1 (index is never an authority) as its own lock-in test, and the 'stale' skip-reason vector from TASK-127. Measure the 10 s time-to-first-deletion on the shipped static musl binary; TASK-115 (musl scan 3-6x slower) must land before that measurement.

Mark's decisions 2026-10-25 (his own, not the emulator): no latency acceptance (the 10 s first-deletion target is dropped); the reclaim walk acts on whatever the index says, re-checking live; coarse (collapsed) index blocks mean more stat work at delete time, never a weaker check.

Design settled 2026-10-25 in fe54fe6 (branch index-design, not merged): docs/architecture.md The index / Memory budget and collapse / Order within a tier / Refusals and index updates / The tally / reclaim walk; safety.md Discovery; testing.md index scenarios. AC review needed: #3 (10 s first-deletion bound dropped; time is measured and logged only). Index units are re-discovered and re-matched at reclaim (stale otherwise).

AC rewrite: design settled in fe54fe6 + 076c5ec (branch index-design); Mark's decisions 2026-10-25. Old #3 (meets the PRD's 10 s bound) replaced: the bound is dropped.

2026-10-25: AC wording bucket -> block (collapse model, design 076c5ec).

COMPASS round 4 (2026-10-25): 31 no longer waits on TASK-30 (the reclaim walk needs an index, not a saved one; stale/corrupt scenarios use injected indexes). It waits on the sync tally (29) and on the mount-screen unification (182/196 item 1) so the screen lives inside resolve_in before the index adds callers.

Implemented on branch t31 (worktree tmp/worktrees/t31, not merged): e2bb85d e2e: parse the synchronous tally's time after 'tier(s) in' (TASK-31) bb6e2dd Reclaim walk on the index: re-match, stale units, per-tier discovery (TASK-31) . Design: executor/indexed.rs (sync tally before the host lock over indexed tiers; per-tier queue from index::queue::derive; single re-matched by fs_kind::discover_beneath Exact + stored identity, else skip stale and evict; blocks/lumps expanded by discover_beneath Subtree from the deeper of site and bound root; candidates found ahead up to 64 (oldest; best-fit 1) then read-only tally + act_fs (extracted from walk_fs); tier discovery always runs after the queue and passes over judged ids/paths). Index serves fs single-source tiers without !profile: only; Cargo/union/negated tiers discover only (TASK-243). Root-cause fixes found by existing tests: sizing walk statted systemd-private-* names and read EACCES as empty (now Step::Refused contains_special/unreadable); liveness Owner-PID ordering broken by per-candidate re-discovery with reused scans (now FreshAttempts::scan_not_before + find-ahead batches keep TASK-203 reuse). Reason::Stale added (RESERVED_REASONS drops it), vector races/unit-gone-before-rematch-is-stale. 9 count-based vectors/tests retargeted to the call they pin (the tally adds earlier calls). AC tests: AC1 diskgc-modelgen tests/index_authority.rs (64 cases default, ran 3000 green); AC2 index_reclaim stale_index_changes_after_the_tally_are_refused_at_deletion_time + a_seeded_index_that_missed_the_changes...; AC3 new_cruft_after_the_snapshot_is_deleted_before_the_pressure_tier; AC4 e2e fs_reclaim_(100k_entry_)time_to_first_deletion_is_measured_cold_and_warm; AC5 a_block_member_opened_after_an_earlier_member_was_deleted_is_in_use; AC6 a_lump_above_the_bound_root_is_expanded_from_the_root_only. Red->green by mutation: AC1 (restricted walk trusts index path) red; AC2 (keep marker inside not refused) red; AC3 (no discovery after queue) red; AC5 needs all three freshness guards off (reuse limit, is_fresh_at, clear expiry) to go red - defence in depth; AC6 (expand from site) red; find-ahead=1 makes the scan-sharing test red. Gate: nix develop -c just check exit 0 (1256 passed). Container (just e2e-one, PASS): fs_reclaim_time_to_first_deletion_is_measured_cold_and_warm, fs_reclaim_100k_entry_time_to_first_deletion_is_measured_cold_and_warm, fs_reclaim_tmp_fixture_deletes_every_eligible_candidate_and_nothing_else, fs_reclaim_100k_entry_tmp_deletes_every_eligible_candidate_and_nothing_else, fs_reclaim_never_enters_private_tmp_inside_a_candidate, fs_reclaim_never_deletes_a_socket_or_fifo, nix_shell_rc_lifecycle_is_pinned, pytest_lifecycle_is_pinned, pytest_live_basetemp_session_is_kept. AC4 measured (rootless podman container, tmpfs /tmp, under the harness strace -f, dev build, 2026-10-26): 100,001 entries / 10,000 rc candidates: first deletion cold 592 ms (sync tally 570 ms), warm 389 ms (371 ms), both exit 0 at --min-free; 8,000 entries: 125 / 91 ms. Warm = second run (kernel caches); no saved index exists before TASK-30. Not the static musl release binary (TASK-115 note). Full just e2e not run (per instructions). Follow-ups: TASK-243, TASK-244, TASK-245, TASK-246.

2026-10-30: review fix round 82cb9af (host failure in the sync tally keeps every tier off the index queue; listing and sizing failure tests, each red under its own mutation; AC1 property adds keep_newest, 1000 cases). Merged to master (ff) at 82cb9af. Gate: just check exit 0 (1258 passed); e2e-clippy/e2e-tests exit 0; AC4 e2e-one PASS. Follow-up filed: TASK-251 (first deletion waits for sizing the whole first tier). Not run: full e2e (async QA), static musl measurement, mutation of the keep_newest property selection.

2026-11-01 (TASK-251): AC #1's host-failure wording predates the split: a host failure in the tally before the lock (listing, best-fit sizing) keeps every tier off the queue; a failure in the deferred oldest sizing under the lock is an error of the run and can follow deletions of earlier queued candidates (architecture.md, safety.md).

2026-11-02 tracker hygiene (TASK-316): ACs reconciled against evidence.
- AC1 ticked: crates/diskgc-modelgen/tests/index_authority.rs. Verification notes: 82cb9af (merge of TASK-31) 'index_authority property 1000 cases exit 0' with each tally-failure arm red under its own mutation; c0885a2 (TASK-295, order-proof property) 'index_authority 2048 cases pass; bites 1-4 red'. Caveat: the property missed three single-event holes Codex found on 82cb9af (fixed: TASK-252, and TASK-30 in 4b74b5c); its remaining generator gaps are TASK-318.
- AC2, AC3, AC5, AC6 ticked: model tests in crates/diskgc-model/tests/index_reclaim.rs, run by every 'just check' (82cb9af's note: just check exit 0): stale_index_changes_after_the_tally_are_refused_at_deletion_time asserts all four scenarios (deep change -> not_idle, new .diskgc-keep -> keep_marker, new Cargo target -> contains_special, new protected match -> protected) plus a_seeded_index_that_missed_the_changes...; new_cruft_after_the_snapshot_is_deleted_before_the_pressure_tier; a_block_member_opened_after_an_earlier_member_was_deleted_is_in_use; a_lump_above_the_bound_root_is_expanded_from_the_root_only. Mutations that turned each red are in this task's notes (2026-10-26).
- AC4 ticked: diskgc-e2e fs_reclaim_100k_entry_time_to_first_deletion_is_measured_cold_and_warm (Full tier; asserts --min-free reached and exit 0 cold and warm). 82cb9af's verification note records the 100k measurement (2026-10-26: cold 592 ms, warm 389 ms) and the 8k variant re-run on 82cb9af (exit 0, met). Caveat: the 100k run was on the pre-review tree, dev build under strace, not the static musl artifact, and no recorded full tier has run it since (no e2e-full note on master after 0.1).
- DoD #1 not assessed here.

2026-11-02 corrections to the hygiene note above (review): AC4's evidence is better cited as cf60cd6's verification note (TASK-251, after 82cb9af): 'e2e-one TTFD 8k cold 47 ms / warm 40 ms, 100k cold 199 ms / warm 129 ms' (dev build, e2e-one, not a full tier); 82cb9af's note lists the 100k variant under skipped:, with the 2026-10-26 figures. AC1: as the 2026-11-01 note says, AC1's host-failure wording predates TASK-251's split (a failure in the deferred oldest sizing under the lock is an error of the run and can follow deletions); the tick is for the property as it stands.
<!-- SECTION:NOTES:END -->
