---
id: TASK-23
title: 'Model scenarios from docs/testing.md (races, owners, deadlines, root refusal)'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:50'
labels:
  - wave-2
  - testing
dependencies:
  - TASK-21
references:
  - docs/testing.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the required scenarios pin the honest limits and the race handling. Implement the model scenarios in docs/testing.md that do not need the index: rename or replace during delete, candidate renamed mid-delete, mixed owners, TMPDIR exported after exec, deadline overruns, command actions refused under --allow-root; plus: an interrupted delete leaves directories with fresh mtimes (pin how they are treated by idle filters), and a file with mtime 1 (copied from the Nix store) but a fresh ctime is not idle. The bucket scenario belongs to the index task.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each listed scenario exists as a named fixture and passes
- [ ] #2 Each scenario is shown to fail when the check it covers is removed (bite proof, once)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
