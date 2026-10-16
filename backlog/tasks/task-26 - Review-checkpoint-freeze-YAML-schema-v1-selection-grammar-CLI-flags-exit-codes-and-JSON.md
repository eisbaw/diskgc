---
id: TASK-26
title: >-
  Review checkpoint: freeze YAML schema v1, selection grammar, CLI flags, exit
  codes and JSON
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:55'
labels:
  - wave-2
  - review
  - irreversible
dependencies:
  - TASK-25
  - TASK-22
  - TASK-55
  - TASK-17
references:
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: these surfaces become a contract once anyone deploys diskgc; this is the last cheap moment to change them. Run mped-architect, a CLI/UX reviewer and a safety reviewer on docs/reference.md against the implementation; resolve findings before freezing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each reviewer's findings are fixed or explicitly accepted with a reason recorded in the task
- [ ] #2 docs/reference.md and the implementation agree (spot-checked by tests for every flag, field and grammar rule)
- [ ] #3 The command-action, Docker and Nix parts of the schema are marked provisional until the release review
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
