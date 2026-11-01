---
id: TASK-269
title: >-
  A protected match removed before a candidate's first overlap screen stops the
  run (exit 3) instead of re-enumerating
status: Done
assignee: []
created_date: '2026-10-31T03:39:00Z'
updated_date: '2026-11-01T02:43:00Z'
labels:
  - bug
  - safety
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Full review 2026-10-31 on master 6f14dd7 (tmp/reviews/codex-full-6f14dd7.md; mped full review in the TASK-266-window notes); static reading, not reproduced. mped SHOULD 2: walk_fs and indexed.rs judge_from_index call run_error when overlaps_checked errs; protected_before_reclaim_inner and tier_policy DeletePolicy::skip re-enumerate with scan_protections instead. TASK-200 says a removed match (cargo clean of a protected target) must not stop the run. Fix: one Reclaim::protected(candidate, stage) used at every stage: on Err re-enumerate, then decide. Model test red first on the discovery and the index path.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Branch t268-overlay-protect (edd2e01), not merged. One Reclaim::protected(candidate, ProtectAt::{FirstScreen,BeforeReclaim}) replaces the four first screens (walk_fs, walk_fs_union, walk_cargo, indexed judge_from_index) and protected_before_reclaim: an unprovable enumerated match leads to a fresh enumeration, then a verdict; only an error there stops the run. Red first: fs_reclaim a_protected_match_removed_before_the_first_screen_does_not_stop_the_run (plain and union walks), index_reclaim ..._from_the_index_..., ensure ..._cargo_targets_first_screen_... (each was exit 3 'cannot prove overlap ... ENOENT'). Not done: the negated first screen (excluded_by in walk_cargo/walk_fs_union) still stops the run on a removed negated match.

Review round: one judgement seam policy::judge (judge_protect/judge_negated, Refresh::{OnError,Always}) for Reclaim first screen + before reclaim, DeletePolicy::skip, negated first screens, plan.rs protect and negated sites; refreshed protect matches kept in an owned Reclaim.protection; plan drops unchecked rows on a protect error. 2df93d2. Red first: negated removal on ensure and dry run (fs union, Cargo), one_removed_protect_match_costs_one_enumeration_not_one_per_candidate (11 vs 23 listings).

2026-11-01: merged with TASK-268/269 after one review round (see verification note).
<!-- SECTION:NOTES:END -->
