---
id: TASK-373
title: >-
  guard_props: a protected build is not removed through an ancestor action whose
  stamps stay
status: To Do
assignee: []
created_date: '2026-11-10T15:27:00Z'
labels:
  - hardening
dependencies: []
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Codex round 6 (2026-11-10): the protection property checks the protected path only as an exact action/eligible path, not whether an ancestor candidate's delete removed it. Deleting an enclosing candidate whole (stamps included) is by design (docs/safety.md); the property should assert that a protected build disappears only together with the directory holding its stamp.
<!-- SECTION:DESCRIPTION:END -->
