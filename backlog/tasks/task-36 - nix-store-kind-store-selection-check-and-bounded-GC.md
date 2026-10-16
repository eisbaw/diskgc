---
id: TASK-36
title: 'nix-store kind: store selection check and bounded GC'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:51'
labels:
  - wave-4
  - irreversible
  - safety
dependencies:
  - TASK-34
references:
  - docs/safety.md#nix-store
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Nix stores grow without bound on development machines. Implement nix-store per docs/safety.md#nix-store: store-in-effect check with the exact environment, supported store URLs, nix-store --gc --max-freed, measured effect; container with its own store and daemon.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 GC removes only dead paths in the container store; roots and generations survive
- [ ] #2 NIX_REMOTE pointing elsewhere yields unsupported_store reported as an error
- [ ] #3 Killing diskgc while nix-store runs leaves the host lock held until it exits
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
