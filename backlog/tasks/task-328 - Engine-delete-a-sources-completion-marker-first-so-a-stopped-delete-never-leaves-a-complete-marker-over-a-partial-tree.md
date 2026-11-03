---
id: TASK-328
title: >-
  Engine: delete a source's completion marker first, so a stopped delete never
  leaves a 'complete' marker over a partial tree
status: To Do
assignee: []
created_date: '2026-11-03T13:12:00Z'
labels:
  - engine
  - safety
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-40 (merged 2026-11-03). node-gyp-headers (has: installVersion) and cargo-registry-src (has: .cargo-ok): the walk deletes in name order; include/ sorts before installVersion, and a .cargo dir or dotfile can sort before .cargo-ok. A delete stopped part-way leaves the marker with content missing, so every native build for that Node version (or that crate's build) fails until the cache is cleaned. Fix: when a source names a completion marker (has:), unlink it first, then the rest. Blocks admission of both profiles. Vectors exist (residual).
<!-- SECTION:DESCRIPTION:END -->
