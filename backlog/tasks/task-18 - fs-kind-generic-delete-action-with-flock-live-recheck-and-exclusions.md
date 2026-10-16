---
id: TASK-18
title: 'fs kind: generic delete action with flock, live recheck and exclusions'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:50'
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

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
