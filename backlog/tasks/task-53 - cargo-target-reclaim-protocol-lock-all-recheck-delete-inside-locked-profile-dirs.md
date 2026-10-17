---
id: TASK-53
title: >-
  cargo-target reclaim protocol: lock all, recheck, delete inside locked profile
  dirs
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-17 13:17'
labels:
  - wave-1
  - irreversible
  - safety
dependencies:
  - TASK-9
references:
  - docs/safety.md#cargo-target
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: split from the Cargo kind task; this is the step that deletes. Implement the reclaim protocol in docs/safety.md#reclaim-protocol: take every known lock without waiting, re-list and recheck (last build, ownership walk, process checks), delete .fingerprint first then all but lock files inside each locked profile dir with the per-entry identity check, release.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No lock file is ever unlinked or replaced (deletion oracle by inode plus strace record)
- [ ] #2 target/doc, target/tmp, target/criterion and CACHEDIR.TAG are never touched
- [ ] #3 A new lock file or a newer build appearing between discovery and locking yields changed (model test with an event on the call trace)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-14: the testing.md sketch (crates/diskgc-model/tests/models/sketch/locked-cargo-target.yaml, expect exit 1 and skipped locked) runs through a stand-in Cargo lock check in crates/diskgc-model/tests/sketch.rs. Once this protocol and the executor exist, run the fixture through them and delete the stand-in.

From task-51: the race tests drive single operations through diskgc-e2e-driver (diskgc-e2e/src/driver.rs); add this task's operation (Cargo discovery / reclaim protocol) as a subcommand in driver::run, generic over H: Delete so its unit tests run on ModelHost, with indexed result lines. Every port call it makes is countable by the controlled wrapper (pause/kill points).

From task-5 (deletion engine): delete inside each locked profile directory by running engine::walk + engine::delete per entry (a Candidate is an open parent fd, its mount id and the entry's path): .fingerprint/ first, then every other entry except the lock files (never make a lock file a candidate). The recogniser only looks inside a candidate, so entries of a profile directory are not contains_special. The engine stops at the first identity mismatch (End::Changed) and reports End::Failed { ENOTEMPTY } when Cargo created something after the walk; End::GateRefused is a run error. Call positions are reproducible (names sorted per directory), which race test 6's seeded kill positions can rely on.

From task-5 round 2 (deep gate): the engine now refuses (contains_special) any candidate holding an entry with a known Cargo lock name (.cargo-lock, .cargo-build-lock, .cargo-artifact-lock) anywhere, the candidate itself included, so a profile directory can never be a plain-delete candidate. This task's plan is unaffected: its candidates are the entries of a locked profile directory except the lock files (.fingerprint/, deps/, build/, incremental/, binaries), which hold no lock files in the layouts measured (Cargo 1.90 and 1.96 put them at the profile root). If one ever does, the engine refuses that entry (contains_special), which is the conservative outcome (treat as unsupported_layout here). Candidate::new now takes the parent's &Resolved plus the entry's path.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
