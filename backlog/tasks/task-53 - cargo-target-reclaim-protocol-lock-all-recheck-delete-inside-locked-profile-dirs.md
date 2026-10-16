---
id: TASK-53
title: >-
  cargo-target reclaim protocol: lock all, recheck, delete inside locked profile
  dirs
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
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

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
