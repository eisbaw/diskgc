---
id: TASK-163
title: 'Walk driver: up.sh gives every walk container the same uid map'
status: To Do
assignee: []
created_date: '2026-10-25 00:44'
labels:
  - followup
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Gotcha 2026-10-25 (TASK-65 walk) says walk containers must map tester to distinct host uids, but tools/walk/up.sh has no --uidmap option; TASK-146..152 walk started walk-diskgc-docs by hand with --uidmap 0:0:1 --uidmap 1:2:65534 (and gidmap). Add an env/arg to up.sh (e.g. WALK_UID_SHIFT) so a parallel walker gets its own range reproducibly.
<!-- SECTION:DESCRIPTION:END -->
