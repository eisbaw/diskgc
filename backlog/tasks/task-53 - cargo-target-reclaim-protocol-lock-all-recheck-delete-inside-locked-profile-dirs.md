---
id: TASK-53
title: >-
  cargo-target reclaim protocol: lock all, recheck, delete inside locked profile
  dirs
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-17 11:40'
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
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
