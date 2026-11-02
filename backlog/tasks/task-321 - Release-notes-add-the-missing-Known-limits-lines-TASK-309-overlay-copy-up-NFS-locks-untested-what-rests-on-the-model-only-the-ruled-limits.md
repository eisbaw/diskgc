---
id: TASK-321
title: >-
  Release notes: add the missing Known limits lines (TASK-309 overlay copy-up,
  NFS locks untested, what rests on the model only, the ruled limits)
status: To Do
assignee: []
created_date: '2026-11-02T15:02:00Z'
labels:
  - release
  - docs
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
COMPASS consult #4 (2026-11-02): a 0.2 release-criteria item (docs/architecture.md, Release procedure) had no carrier task. The draft release notes' Known limits (docs/reference.md) lack: TASK-309 (overlay candidates with lower entries stop part-way, 'changed'; lower bytes not freed anyway); NFS lock paths untested on a real NFS mount (TASK-288; until TASK-314's NFS case); the list of behaviours resting on the model only (from testing.md 'Not covered end-to-end'); the limits Mark rules on in TASK-296/305/308 once ruled. Also remove 'Do not deploy the candidate built from master' only at the RC commit.
<!-- SECTION:DESCRIPTION:END -->
