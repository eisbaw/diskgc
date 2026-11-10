---
id: TASK-372
title: >-
  Nested-build guard: verdict on the verified candidate fd, not a fresh path
  lookup
status: To Do
assignee: []
created_date: '2026-11-10T15:27:00Z'
labels:
  - hardening
dependencies: []
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Codex round 6 (2026-11-10) finding 3: Judge::retention_guard verifies the found object, then version_where calls nested::verdict by pathname. A stamp plus two RENAME_EXCHANGEs around the verdict's open feed it an empty sibling, the guard clears and the stamped build is deleted. Three events, documented limit; hardening: pass the reached fd/identity into verdict. Also from the round-6 implementer: verdict runs once per term per check and re-reads the mount table each time (estimated 5-10x more work than needed, unmeasured); a command-protocol vector for an undecided guard under the lock; a negative control for a separate pytest-tmp tier; vectors pin faults by call count (fragile).
<!-- SECTION:DESCRIPTION:END -->
