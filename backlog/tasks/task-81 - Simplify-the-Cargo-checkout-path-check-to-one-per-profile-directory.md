---
id: TASK-81
title: Simplify the Cargo checkout path check to one per profile directory
status: Done
assignee: []
created_date: '2026-10-19 01:34'
updated_date: '2026-10-19 12:31'
labels:
  - safety
dependencies:
  - TASK-80
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: task-80 r3 review (mped SHOULD-1). judge_checkout_paths in cargo_target/eligible.rs loops over profile entries, but the result depends only on each profile directory's path (an entry's own name is the walk's). Judging per entry makes it depend on the entry list (a target whose profile dirs were empty when found passes before the locks and is refused only under them), needs three call sites where one suffices per path, and loses fail-closed unsafe_path for a target with no entries. Judge the names from the root down to each profile directory (inclusive) once on the target as found (with judge_layout), call it at the top of walk_target and before the locks in reclaim; same_locks already guarantees the dirs under the locks are the ones found. Safe as is; this removes complexity.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 One as-found check per path (eligibility, reclaim before locks); judge_inspected no longer needs it, or keeps it with a comment saying why
- [x] #2 a_cargo_target_with_a_marker_between_root_and_its_entries_is_refused stays green; a target with empty profile dirs under .git is refused before any lock (new test)
- [x] #3 docs/safety.md says 'down to each profile directory'
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented (task-81 implementer, 2026-10-19): engine::checkout_metadata_down_to(checkouts, root, dir): the path rule for every candidate in dir, dir's own name included (one private helper with checkout_metadata_below; same fail-closed unsafe_path whatever the setting). cargo_target::eligible::judge_checkout_paths(target, checkouts) judges each profile directory of the target as found (no entry loop, no inspected argument). Called once per check: top of walk_target (eligibility, tally) and in reclaim before the locks, now BEFORE judge_layout so eligibility and reclaim give the same reason (an all-empty target in .git is contains_checkout, not empty). Dropped from judge_inspected with a comment: same_locks guarantees the same profile dirs, so the same paths. Behaviour change under refuse: a target with an empty profile dir inside checkout metadata (target/.git/debug holding only lock files, next to a full target/debug) was reclaimed before (no entry under .git to judge) and is now contains_checkout before any lock; nothing under .git was ever deleted, but its lock files were flocked. Under allow nothing changes. Tests: engine unit checkout_metadata_down_to_a_directory_includes_its_own_name (and the same verdict as checkout_metadata_below for an entry of the dir); ensure.rs a_cargo_target_with_an_empty_profile_directory_in_checkout_metadata_is_refused (x/.git/target all empty; target/<M> and target/<M>/debug empty next to a full debug, x .git .hg .svn; dry run, ensure tallied and untallied: contains_checkout, never flocked, nothing unlinked; built-in allow: empty / eligible). Bites: reclaim pre-lock call removed -> new test red (all-empty: empty instead of contains_checkout; mixed, untallied: reclaimed) and the task-80 reclaim test red; walk_target call removed -> 3 red; old per-entry rule restored -> new test red. docs/safety.md: 'down to each profile directory' (No source checkout, eligibility walk, Reclaim protocol step 1). Gate: workspace-fmt/clippy/tests, e2e-fmt/clippy/tests plain + --rebuild all 0; just check 504 passed/1 ignored; just e2e 83/83 (29.5s of 300s).
<!-- SECTION:NOTES:END -->
