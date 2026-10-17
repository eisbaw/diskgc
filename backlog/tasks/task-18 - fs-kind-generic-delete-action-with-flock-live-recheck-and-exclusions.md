---
id: TASK-18
title: 'fs kind: generic delete action with flock, live recheck and exclusions'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 12:50'
labels:
  - wave-2
  - irreversible
  - safety
dependencies:
  - TASK-16
references:
  - docs/safety.md#filesystem-candidates
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: most cruft (/tmp leftovers, caches) is plain files. Wire the fs kind's delete action through the engine and in-use checks: flock the candidate, eligibility walk and in-use under the lock, identity-checked delete; PrivateTmp entries and network filesystems excluded; tmpfs matching behaviour.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The 100k-entry /tmp fixture: every eligible pytest-of-* and nix-shell rc dir is deleted, nothing else (deletion oracle)
- [ ] #2 A file created mid-delete yields an ENOTEMPTY error for that candidate, never a silent retry
- [ ] #3 systemd-private-* entries are never candidates
- [ ] #4 Tests drive the engine with user-profile fixtures (no built-in names before wave 4)
- [ ] #5 A candidate on a network filesystem is never deleted (model test); a socket or FIFO is never deleted (container test)
- [ ] #6 A dead FUSE mount in /tmp does not hang the scan (container test with /dev/fuse and a FUSE server that never answers)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-5 (deletion engine): the fs kind wires engine::walk -> (in-use, task-6) -> engine::delete under the candidate's flock. Not in the engine, still this task's: mountinfo first (a dead FUSE mount point inside a candidate would block the walk's statx; the walk itself only detects mounts by stx_mnt_id/EXDEV), systemd-private-*, network filesystems, .diskgc-keep in directories between the profile root and the candidate (engine::KEEP_MARKER; the walk only looks inside), the idle filter from WalkRecord::newest_change() (None = some time unreported: not idle), protect/negation. The flock fd should be checked against WalkRecord::root().identity.id. make_writable maps to DeleteOptions::make_writable. A file created mid-delete gives End::Failed with ENOTEMPTY (AC #2 here). Sockets/FIFOs are refused by the walk (contains_ipc; container test engine_skips_sockets_and_fifos exists).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
