---
id: TASK-81
title: Simplify the Cargo checkout path check to one per profile directory
status: To Do
assignee: []
created_date: '2026-10-19 01:34'
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
- [ ] #1 One as-found check per path (eligibility, reclaim before locks); judge_inspected no longer needs it, or keeps it with a comment saying why
- [ ] #2 a_cargo_target_with_a_marker_between_root_and_its_entries_is_refused stays green; a target with empty profile dirs under .git is refused before any lock (new test)
- [ ] #3 docs/safety.md says 'down to each profile directory'
<!-- AC:END -->
