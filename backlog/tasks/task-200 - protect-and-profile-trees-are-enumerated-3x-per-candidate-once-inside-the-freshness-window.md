---
id: TASK-200
title: >-
  protect and !profile trees are enumerated 3x per candidate, once inside the
  freshness window
status: To Do
assignee: []
created_date: '2026-10-25 09:01'
updated_date: '2026-10-25 09:03'
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
<!-- SECTION:NOTES:END -->
