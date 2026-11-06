---
id: TASK-365
title: 'Take node-modules back out of the 0.2 catalog (Mark: hold it)'
status: In Progress
assignee: []
created_date: '2026-11-06T08:30:00Z'
updated_date: '2026-11-06T10:27:00Z'
labels:
  - release
  - '0.2'
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-06, answering whether 'admit current as-is' covers the npm install --no-save residual: 'No, hold node-modules'. Revert the node-modules part of TASK-359 (de9f2af1): drop its 0.2 label, restore it to the test-only candidate list, update docs (library row, liveness verdict, release notes, Known limits, READMEs), tests and bites. The other five project-tree profiles stay admitted. Start after TASK-360 merges (both touch builtin.yaml and reference.md).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-06 orchestrator: implementer in tmp/worktrees/t365 (branch t365-hold-node-modules, from master cfc12ecc).
<!-- SECTION:NOTES:END -->
