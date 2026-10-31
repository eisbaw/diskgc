---
id: TASK-260
title: >-
  protect and !profile miss a candidate under a bind mount of a protected
  subdirectory (overlaps_with compares ancestors only)
status: To Do
assignee: []
created_date: '2026-10-31T01:54:00Z'
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
