---
id: TASK-106
title: Per-profile YAML checkout guard override for built-ins
status: To Do
assignee: []
created_date: '2026-10-21 19:54'
labels:
  - configuration
  - safety
  - profiles
dependencies:
  - TASK-80
references:
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Operators need a narrow YAML policy setting that can disable the VCS checkout guard for a named profile while keeping its built-in matcher and action intact. The guard remains enabled unless the effective profile explicitly allows checkouts. This includes npm-cache and uv-cache once those profiles are admitted by the applicable release catalog; an override cannot expand that catalog.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A YAML policy can set checkouts: allow or refuse by profile name without duplicating a built-in profile definition; effective policy is visible in the plan and documented. Omitted setting preserves the built-in default and the global safety default is refuse.
- [ ] #2 Only the checkout guard changes. The profile matcher, root constraints, delete action, age and activity checks, protection markers, ownership, and release catalog restrictions remain effective. Unknown names and conflicting policy entries fail at load time.
- [ ] #3 Disposable-container integration tests cover refused and allowed checkout markers for npm-cache and uv-cache when admitted, plus excluded profiles and other safety gates. No deletion test runs on the developer host.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
