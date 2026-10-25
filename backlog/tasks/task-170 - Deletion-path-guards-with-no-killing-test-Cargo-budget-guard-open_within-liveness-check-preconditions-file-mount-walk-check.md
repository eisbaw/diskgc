---
id: TASK-170
title: >-
  Deletion-path guards with no killing test (Cargo budget/guard/open_within,
  liveness::check preconditions, file-mount walk check)
status: To Do
assignee: []
created_date: '2026-10-25 01:14'
labels:
  - hardening
dependencies: []
references:
  - crates/diskgc-model/tests
  - docs/testing.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-13 async QA (b009e59, read-only review): guards whose removal no test would notice (by reading; to be confirmed by cargo-mutants once RAM allows, see TASK-13 notes).
Cargo:
- Shared fresh-attempt budget per target: moving FreshAttempts::new into delete_part (one budget per part) passes everything; inuse.rs tests sharing within one candidate only; no Cargo in_use_unknown test at all.
- guard / policy_skip under the Cargo locks and before each part: no Cargo test adds a protect or negated match after locking.
- open_within identity check: nothing replaces build/<pkg>/<hash> between the walk and its fingerprint's deletion.
- Run::protocol zips inspected.profiles with dirs relying on judge_inspected's order: add an assert.
fs / liveness:
- fs_kind::liveness::check: the scan-after-discovery check (scan.started() < found.found_at, the only enforcement of the ordering Owner PID depends on), the missing pid-lookup / missing pytest-facts checks (the only guard against a mis-built deleting checker; Checker::serves compares users and sockets only), and the parent-on-another-device check are all untested.
- engine::walk per-entry mount id on a NON-directory (file bind mount made after the walk's mountinfo read): model cannot express mounts on files (ModelState::mount_on); e2e engine_stops_at_file_bind_mount_after_identity_check covers only the delete-time EBUSY. Without the check: partial delete then EBUSY, instead of contains_mount untouched. See TASK-129 note.
- judge other_fs with a candidate device different from the root's (btrfs nested subvolume): no test.
Design inconsistency to decide with the tests: fs_kind::reclaim may clear with a scan up to REUSE_LIMIT (500 ms) old that started before the candidate's lock and walk; cargo_target::reclaim calls Checker::forget under its locks for exactly this race. Apply one rule, or document why fs differs (safety.md limit 5 covers it generically).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each listed guard has a model vector or test that fails when the guard is removed (bite-checked), or a recorded reason it is defence in depth
- [ ] #2 fs vs Cargo scan-before-lock rule decided and documented
<!-- AC:END -->
