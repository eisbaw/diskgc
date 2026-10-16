---
id: TASK-56
title: 'Journey: one policy across /tmp, Cargo, Docker and Nix'
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-16 22:51'
labels:
  - wave-4
  - journey
dependencies:
  - TASK-35
  - TASK-36
  - TASK-39
references:
  - PRD.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the core value of diskgc is one ordered policy across ecosystems; nothing exercised it end to end. In a container setup with dind and a Nix daemon, fill /tmp, Cargo targets, Docker build cache and dead Nix paths, configure stale and pressure groups across all of them, and run ensure and watch.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Groups run in order across ecosystems and stop at the goal; the deletion oracle agrees
- [ ] #2 Findings are filed as tasks
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
