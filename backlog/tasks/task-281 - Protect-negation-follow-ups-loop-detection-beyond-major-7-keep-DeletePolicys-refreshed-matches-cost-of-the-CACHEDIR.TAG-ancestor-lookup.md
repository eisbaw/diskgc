---
id: TASK-281
title: >-
  Protect/negation follow-ups: loop detection beyond major 7, keep
  DeletePolicy's refreshed matches, cost of the CACHEDIR.TAG ancestor lookup
status: To Do
assignee: []
created_date: '2026-11-01T02:43:00Z'
labels:
  - safety
  - performance
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-268/269 (merged 2026-11-01): (1) loop-backed filesystems are recognised only by device major 7; loop partitions, device-mapper over a loop file (LUKS container file) and btrfs on a loop are documented limits; consider mountinfo's /dev/loopN source; (2) DeletePolicy's own fresh enumerations are not kept (only the first screen and before-reclaim replace the matches); (3) the CACHEDIR.TAG lookup between a Cargo protect/negated root and each candidate costs one resolve per directory, and a non-Cargo CACHEDIR.TAG on that path forces a fresh enumeration every check: measure under TASK-251; (4) the Linux loop_backing_file reader has not seen a configured loop device (rootless containers cannot attach one): a VM test.
<!-- SECTION:DESCRIPTION:END -->
