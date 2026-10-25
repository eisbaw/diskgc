---
id: TASK-168
title: >-
  Cargo reclaim: judged_last_build is None in production, so 'newer build since
  judged' is never changed
status: To Do
assignee: []
created_date: '2026-10-25 01:13'
updated_date: '2026-10-25 09:14'
labels:
  - hardening
  - safety
dependencies: []
references:
  - crates/diskgc-core/src/cargo_target/reclaim.rs
  - crates/diskgc-core/src/executor.rs
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-13 async QA (b009e59, read-only). executor walk_cargo calls reclaim_checked with judged_last_build: None; only the model test a_newer_build_before_the_locks_are_held_is_changed passes Some. A target rebuilt between tally and lock is not changed; only the idle recheck catches it. docs/safety.md cargo Reclaim step 2 says "or no longer known: changed": not true in ensure. Not a deletion hazard (locks + in-use under the locks hold). Decide: pass the tally last_build, or change the doc to say the idle recheck is the only guard.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 ensure passes the tally's last build, with an executor-level test; or docs/safety.md step 2 is corrected
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
lane-b 0068fb6 (not merged): doc fix chosen (COMPASS). safety.md Reclaim step 2 now says ensure passes no judged_last_build; a rebuild between tally and locks is caught by the idle recheck under locks only when the tier has an idle requirement (built-in cargo-target has none), otherwise only by in-use. Not a deletion hazard (mped-architect concurs): under the locks no Cargo of the lock set builds. Behaviour unchanged.
<!-- SECTION:NOTES:END -->
