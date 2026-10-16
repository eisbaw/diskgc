---
id: TASK-43
title: >-
  Hardening wave 4: kinds and command actions deep review, mutation testing, doc
  sync
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:51'
labels:
  - wave-4
  - hardening
dependencies:
  - TASK-42
  - TASK-56
references:
  - docs/safety.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Docker, Nix and command actions are stable now. Deep review with a safety reviewer, cargo-mutants on backend binding and command execution, docs/safety.md and docs/reference.md sync, deferred findings.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No surviving mutants in backend binding or command preflight, or each justified
- [ ] #2 Deep review findings fixed or filed
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
