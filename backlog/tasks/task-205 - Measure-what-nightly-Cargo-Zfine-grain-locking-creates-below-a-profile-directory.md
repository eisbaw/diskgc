---
id: TASK-205
title: >-
  Measure what nightly Cargo -Zfine-grain-locking creates below a profile
  directory
status: Done
assignee: []
created_date: '2026-10-25 09:15'
updated_date: '2026-10-25 21:10'
labels:
  - hardening
  - safety
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up of TASK-167 (mped-architect note). diskgc refuses any .cargo-*lock* name at any depth in a walk; a future per-unit lock named outside that pattern (e.g. 'lock', '.lock') would be recorded and unlinked, breaking 'no Cargo lock file is ever deleted'. The vector's .cargo-unit-lock is hypothetical. Measure the real names with the Cargo measurement/race suites on the pinned nightly with fine-grain locking, and add a vector with the real name.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The lock files a pinned nightly with fine-grain locking creates are measured and recorded in docs/safety.md Lock files
- [x] #2 A model vector uses the real name; the walk or judge_layout refuses or takes it
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision 2026-10-25 by mark-emulator (Mark may veto): measure what nightly-2026-10-16 -Zfine-grain-locking creates below a profile directory BEFORE the 0.1 cut (it is in the test image, cheap); if it creates lock files outside .cargo-*lock*, fail closed on them; either way the release notes state what was measured.

2026-10-25 t205 (2163ff8 on branch t205, not merged): measured nightly-2026-10-16 -Zfine-grain-locking: per-unit <profile>/build/<pkg>/<hash>/.lock (empty, flocked EX/SH), .cargo-build-lock held shared; only other lock-like name rustc incremental s-*.lock (pre-existing). .lock is outside .cargo-*lock*: before the fix the reclaim unlinked it (container test with rule disabled reclaimed the target). Walk now refuses .lock at build/<pkg>/<hash>/.lock by absolute path (not by name: pytest-tmp's pytest-N/.lock). Container test cargo_reclaim_keeps_a_fine_grain_locked_target pins it. Gates: just check exit 0; e2e-clippy, e2e-tests OK; 10 Cargo e2e-one runs PASS. mped-architect: ship, no blockers.

Merged to master as 43a6d54 on 2026-10-25T21:10Z. Real PRD must-never fixed: fine-grain Cargo unit locks build/<pkg>/<hash>/.lock were deleted by Cargo reclaims before this.
<!-- SECTION:NOTES:END -->
