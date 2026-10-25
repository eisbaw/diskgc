---
id: TASK-200
title: >-
  protect and !profile trees are enumerated 3x per candidate, once inside the
  freshness window
status: Done
assignee: []
created_date: '2026-10-25 09:01'
updated_date: '2026-10-25 11:37'
labels:
  - shipping
  - performance
  - safety
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by COMPASS (10-task review on 72b376c), confirmed in TASK-184. executor.rs: protected_before_reclaim, the policy under the candidate lock and policy_skip in the clear's freshness window each run a full discovery of every protect (and negated) root, for every fs and Cargo candidate (Cargo: per part inside the 1 s window). Walk 2026-10-25-t184, protect {cargo-target, roots: [/home/tester/src]}: 7,300 dirs 263-617 ms/candidate, 61,000 dirs 2.2-3.3 s/candidate with in_use_unknown skips whose detail blames a busy host though the scan took 252 ms; without protect 5-33 ms. The 0.1 example config protects a Cargo project tree; ~/topics here has 340k directories, so every clear would expire. Model test scans_and_protect_enumerations_do_not_grow_with_candidates (fs_reclaim.rs, #[ignore], red: 7 vs 19 protect-root listings for 2 vs 6 candidates). DECISION NEEDED (the cheap fix weakens a documented and tested rule: docs/safety.md 'rechecked under the candidate lock and immediately before deletion'; command_union_rechecks_new_negated_match_just_before_invocation and two siblings): (1) per-tier match set for delete actions only (a later match inside the candidate is never unlinked; residual: an ancestor that becomes a match), full recheck before command actions; (2) per-tier set everywhere, docs and tests changed; (3) keep re-enumeration, move it out of the freshness window (fixes in_use_unknown, not the cost). Draft of (2): tmp/t184/policy-cache-option-a.diff (unreviewed).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision 2026-10-25 by mark-emulator (Mark delegated; Mark may veto): cache the protect and negated sets per tier for DELETE actions only (delete never unlinks an entry the eligibility walk did not list, so a protected match created later inside the candidate survives); keep the full recheck immediately before a COMMAND action runs (the three tests that pin that stay). Update docs/safety.md's 'rechecked under the candidate lock and immediately before deletion' accordingly. One review round.

2026-10-25 implementer (branch inuse-pass, worktree tmp/worktrees/inuse-pass; started 10:20 CET). Commits ecca5c8 (cache per tier for delete/Cargo, change gate on the walk's newest mtime/ctime; command actions keep the full recheck) and 91a23e6 (review fixes: every directory's times read again after it is listed; 3 s slack; a cached match that vanished falls back to a fresh enumeration instead of stopping the run). The decision's argument (delete never unlinks what the walk did not list) only covered matches made after the walk; the gate covers a match made between the tier's enumeration and the walk. Residuals documented in docs/safety.md (Not protected, not excluded): ancestor becoming a match; a marker-defined match (CACHEDIR.TAG/.cargo-lock) made after the walk inside a walked dir; a bind mount made after the enumeration; an undone wall-clock step; a match made after the walk now ends ENOTEMPTY (candidate error) instead of a protected skip. Model test scans_and_protect_enumerations_do_not_grow_with_candidates un-ignored: red on master (7 vs 19 listings), green. Walk tmp/walks/2026-10-25-inuse-pass (release candidates, WALK_PID_HOST=1, WALK_TMP_BULK=8000, protect cargo-target over 61,924 dirs in /home/tester/src, 20 pytest sessions per run, interleaved, load 6.1-7.6): protect before 2068/4035/2789 ms per deleted candidate with 0/4/2 in_use_unknown; after 12/9/15 ms, none. No protect: before 10/7/9 ms, after 8/7/8 ms. Review: mped-architect one round, fix first (blockers a: dir times read before listing; b: 2 s slack vs 2 s FS granularity) -> fixed in 91a23e6 with tests that fail without the fix. Follow-up: when the gate fires on a large protect, the re-enumeration runs inside the freshness window (as every candidate did before); enumerate once after the walk instead.

Merged to master as 91a23e6 on 2026-10-25T11:37Z. Orchestrator accepted the implementer's safety addition to the mark-emulator decision: full re-listing after the walk when the newest walked change is unknown or within 3 s of the tier listing, or the wall clock jumped (fail-safe; adds checking only).
<!-- SECTION:NOTES:END -->
