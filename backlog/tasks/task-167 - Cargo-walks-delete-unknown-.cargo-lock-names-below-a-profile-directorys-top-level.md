---
id: TASK-167
title: >-
  Cargo walks delete unknown .cargo-*lock* names below a profile directory's top
  level
status: Done
assignee: []
created_date: '2026-10-25 01:12'
updated_date: '2026-10-25 09:28'
labels:
  - hardening
  - safety
dependencies: []
references:
  - crates/diskgc-core/src/engine/walk.rs
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-13 async QA (b009e59, confirmed by reading engine::walk::admit, not reproduced).
admit() refuses the three known CARGO_LOCK_NAMES at any depth, but an unknown is_cargo_lock_name() match (.cargo-*lock*) only when its parent or grandparent listing holds CACHEDIR.TAG (nested-target shape); cargo_target judge_layout refuses unknown names only directly in the profile directory. So e.g. target/debug/build/<pkg>/<hash>/.cargo-unit-lock or debug/.fingerprint/x/.cargo-lock.bak is recorded and unlinked in reclaim step 3.
Today harmless (no measured Cargo creates such a name; the profile locks are held exclusively). It becomes a real lock-file deletion (PRD What bad looks like: "Any Cargo lock file deleted") the day Cargo adds finer-grained locks, silently. docs/safety.md cargo Reclaim step 3 claims "every walk refuses every Cargo lock name": false today.
Fix the class: in Cargo walks (and arguably every walk) refuse any .cargo-*lock* name at any depth (contains_special). Same blind spot for a user fs profile whose candidate IS a profile/triple dir (e.g. */target/debug).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A model vector: unknown .cargo-*lock* name deep in a profile directory -> target skipped contains_special, nothing removed
- [x] #2 docs/safety.md step 3 and Filesystem candidates state the same rule as the code
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
lane-b 0068fb6 (not merged): every walk refuses any .cargo-*lock* name at any depth, candidate included (contains_special); tag-based parent/grandparent recogniser removed as subsumed (reverses task-5's narrower rule, errs on keeping). Vector builtins/cargo-target-unknown-lock-name-deep.yaml + 2 rows in walk/nested-cargo-target-recogniser.yaml: red on 53a53dc, green on branch; reverting walk.rs fails both vectors + 3 engine tests. just check green (isolated target). Containers green: cargo_reclaim_deletes_only_inside_locked_profile_dirs@1.99.1 and @nightly-2026-10-16, cargo_race_2...@1.99.1, cargo_race_9...@1.99.1. mped-architect: ship, notes addressed. Limit: name-pattern only; a future lock named outside .cargo-*lock* is not recognised (follow-up filed).

Decision 2026-10-25 by mark-emulator (Mark may veto): accept the fail-closed rule (any .cargo-*lock* name at any depth refuses its candidate), reversing task-5's narrower rule; a non-Cargo file with such a name keeping its candidate is the accepted cost (PRD: no Cargo lock file is ever deleted).

Merged to master as b5bcb7a on 2026-10-25T09:28Z.
<!-- SECTION:NOTES:END -->
