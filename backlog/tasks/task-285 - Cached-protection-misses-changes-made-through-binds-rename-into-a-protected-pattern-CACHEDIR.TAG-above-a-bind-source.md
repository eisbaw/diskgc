---
id: TASK-285
title: >-
  Cached protection misses changes made through binds: rename into a protected
  pattern, CACHEDIR.TAG above a bind source
status: To Do
assignee: []
created_date: '2026-11-01T05:37:00Z'
labels:
  - safety
  - irreversible
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. Codex P1, one event: /src/work/sub pre-bound at /alias; protect /src/keep-*; after enumeration rename /src/work -> /src/keep-work; old candidate /alias/cruft keeps path, identity and timestamps, so tier_policy DeletePolicy::skip's cached exclusions permit deleting newly protected data. policy.rs tagged_ancestor uses lexical strip_prefix, so a Cargo tag created above a bind source is missed (mped NIT, multi-event there). Fix: validate exclusion caches against mount-translated ancestry (place_of), including ancestor renames and Cargo tags; model test red first.
<!-- SECTION:DESCRIPTION:END -->
