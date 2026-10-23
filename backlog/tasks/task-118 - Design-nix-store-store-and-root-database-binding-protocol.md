---
id: TASK-118
title: 'Design: nix-store store and root-database binding protocol'
status: To Do
assignee: []
created_date: '2026-10-23 16:19'
labels:
  - irreversible
  - design
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Design before implementation (audit 2026-10-23 of docs vs PRD). Deliverable is docs text, reviewed by mped-architect, not code. docs/safety.md nix-store says the planned 'nix store info --json' URL check is insufficient and the kind remains unshipped (TASK-36). The PRD lists kind names/schema as hard to walk back while the schema ships as provisional. Design: how diskgc binds one store + its root database (and refuses otherwise), the GC-root/profile/generation never-removed guarantee, --max-freed as a lower bound and overshoot vs the free-space goal, the tally size source for Nix units, Lix/variant handling, test scenarios in a container.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 docs/safety.md nix-store has a complete binding protocol and failure modes
- [ ] #2 docs/reference.md nix-store schema fields are final (not provisional)
- [ ] #3 docs/testing.md names the nix-store scenarios
- [ ] #4 mped-architect design review GO recorded in notes
<!-- AC:END -->
