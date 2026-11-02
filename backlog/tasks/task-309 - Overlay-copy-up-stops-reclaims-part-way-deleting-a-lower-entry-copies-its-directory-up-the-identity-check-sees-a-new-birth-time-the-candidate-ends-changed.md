---
id: TASK-309
title: >-
  Overlay copy-up stops reclaims part-way: deleting a lower entry copies its
  directory up, the identity check sees a new birth time, the candidate ends
  'changed'
status: To Do
assignee: []
created_date: '2026-11-02T00:42:00Z'
labels:
  - product
  - overlay
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the TASK-304 VM test, scenario 8 (2026-11-02): on any overlay, a candidate with lower-layer entries is deleted part-way and ends changed: deleting a lower entry copies its parent directory up, and the walk's identity re-check then sees a new birth time. Safe (the walk stops; the lower layer is untouched) but such candidates are never fully reclaimed. Their lower bytes are not freed anyway (deletion only writes whiteouts), so on overlays reclaiming lower content frees nothing but upper metadata. Options: skip lower-only entries up front (they free nothing) and report them; or treat copy-up of an ancestor directory as an expected identity change on overlays (careful: identity checks are safety checks); or refuse candidates with lower entries as 'frees nothing'. Decide with the space-honesty rule in mind (PRD: no claimed reclaim that frees nothing).
<!-- SECTION:DESCRIPTION:END -->
