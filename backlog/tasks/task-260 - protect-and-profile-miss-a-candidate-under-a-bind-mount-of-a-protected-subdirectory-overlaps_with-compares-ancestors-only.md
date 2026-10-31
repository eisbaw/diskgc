---
id: TASK-260
title: >-
  protect and !profile miss a candidate under a bind mount of a protected
  subdirectory (overlaps_with compares ancestors only)
status: To Do
assignee: []
created_date: '2026-10-31T01:54:00Z'
updated_date: '2026-10-31T02:33:00Z'
labels:
  - safety
  - irreversible
  - release
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the mped review of TASK-252 (2026-10-31), static reading; not yet reproduced. executor/policy.rs overlaps_with (release-0.1: executor.rs overlaps_with, same logic) matches a candidate against protect/negated matches by lexical overlap, then by identities of the candidate's ANCESTORS against the protected path's ANCESTORS. A bind mount exposing a SUBDIRECTORY of a protected tree is missed: protect /tmp/gen; tier root /mnt/x is a bind of /tmp/gen/x; candidate /mnt/x/y. Ancestors of /mnt/x/y share only / with those of /tmp/gen (the bind's identity equals /tmp/gen/x, a descendant of the protected path, never compared). Zero events: a static configuration. Unlike hard links, the alias is the same directory, so deletion destroys protected data. Applies to 0.1 (released) as well as master.
Fix direction: a model test first (red); then compare each candidate ancestor's mount (mountinfo root field + device) against protected matches: refuse when the candidate's mount is a bind whose source root lies at or below a protected match on the same device, or check identities in both directions via mountinfo roots. Backport to release-0.1 is Mark's call.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented on branch t260-protect-bind (30e07a0, from 947f3c4): overlaps_in_filesystem translates candidate and matches to (mountinfo device, root field + rest of path) and compares by name, before the ancestor-identity comparison (kept: hard links; it also confirms each compared path's mount live). Fails closed on an unplaceable path. Model: MountSpec.bind_root. Red on master: 6 of 7 new model tests (reclaim deleted /mnt/x/y); the 7th pins that a match containing a mount stops the run. Backport: hand-ported executor.rs patch dry-applies to release-0.1 (af3d503, git apply --cached --check on a temporary index); master's policy.rs diff does not (file absent there); the model hunk has one spec.rs context conflict. Not run: just e2e, containers, VMs.
<!-- SECTION:NOTES:END -->
