---
id: TASK-120
title: >-
  Design: Docker contract leftovers - provisional names, skip-vs-error split,
  Buildx sizing conflict
status: To Do
assignee: []
created_date: '2026-10-23 16:19'
updated_date: '2026-10-23 21:01'
labels:
  - design
dependencies:
  - TASK-113
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Design before implementation (audit 2026-10-23 of docs vs PRD). Deliverable is docs text, reviewed by mped-architect, not code. docs/architecture.md (~L123) sizes Docker tally units with 'docker buildx du' while docs/safety.md (~L1427) says querying Buildx writes client config during a dry run - these conflict. Action/schema names are provisional until TASK-49; the client-failure skip-vs-error split is TASK-113. Resolve the sizing conflict and finalise names together with TASK-113.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The Buildx sizing conflict is resolved in docs with one source of truth
- [ ] #2 Docker action and schema names are marked final or an explicit list of what TASK-49 may still change
- [ ] #3 mped-architect design review GO recorded in notes
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mark's design decisions (2026-10-23, binding): Docker client failure before any backend query (Podman CLI shim, daemon down, socket EACCES): SKIP WITH A REASON (unsupported_backend / backend_unavailable), the run continues with other tiers, exit stays 0/1 by the free-space outcome; a host without a working Docker is not an error. Supersedes today's exit 3. Applies to TASK-113 too.
<!-- SECTION:NOTES:END -->
