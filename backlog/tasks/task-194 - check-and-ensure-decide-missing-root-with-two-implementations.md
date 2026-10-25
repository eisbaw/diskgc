---
id: TASK-194
title: check and ensure decide 'missing root' with two implementations
status: Done
assignee: []
created_date: '2026-10-25 05:59'
updated_date: '2026-10-25 09:55'
labels:
  - shipping
  - structure
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the 10-task codebase review at 72b376c (2026-10-25). main.rs:warn_missing_roots (raw openat2 via path_is_absent) vs invocation.rs:refuse_missing_roots (resolve_in over Host). TASK-178 had to be fixed twice; model vectors cannot cover check; reference.md ### check still says 'a stat of each supplied root'; roots failing the mount screen are silently not warned. Fix: make refuse_missing_roots generic over Host, call it from check, delete path_is_absent, update doc. Also widen TASK-187: executor.rs:held reports [] when the scan fails in a live run too.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
lane-c (8261462, 6c1e6ec): invocation.rs root_finding (screen then resolve_in, generic over Host) shared by refuse_missing_roots (ensure) and new pub check_roots (check warnings, exit unchanged); check now also warns for roots on refused mounts; check builds LinuxHost (warns if it cannot start; config-check-sandbox proves it starts in a Nix build sandbox). path_is_absent, read_mountinfo_unheld, free effective_uid deleted. Vector runner runs check_roots for every vector with a loading config + optional expect.check_warnings (missing-tier-root, dead-FUSE protect, automount vectors). Container test fs_protected_dead_fuse_root_is_refused_without_opening_it now runs check on the dead FUSE root (PASS). reference.md ### check fixed. The TASK-187 widening (executor.rs:held reports [] when the scan fails in a live run) was NOT done here: executor.rs is t184's lane. Awaiting merge.

Merged to master as 9f317cb on 2026-10-25T09:54Z.
<!-- SECTION:NOTES:END -->
