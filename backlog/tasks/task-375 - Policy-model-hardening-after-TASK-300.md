---
id: TASK-375
title: Policy/model hardening after TASK-300
status: To Do
assignee: []
created_date: '2026-11-10T21:02:00Z'
labels:
  - hardening
dependencies: []
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-300 reviews (36465cf1): a model event fault aborts the whole vector corpus run instead of failing that vector; the error for moving the root mount is wrong; no end-to-end property 'new staleness == always refresh' over full runs; generator has a small name set, shallow roots, no overlays or propagation; policy_skip (before a command) ignores overlap with an earlier-enumerated match and reads separate mount tables for its protect and negated verdicts; each delete candidate still pays four overlap checks of ~1.3 ms (dropping the before-reclaim one is not exact under a concurrent change: owner's call).
<!-- SECTION:DESCRIPTION:END -->
