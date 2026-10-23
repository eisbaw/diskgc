---
id: TASK-124
title: >-
  app_not_running: reclaim stale Chromium SingletonLock caches with Owner PID
  and host name
status: To Do
assignee: []
created_date: '2026-10-23 16:33'
labels:
  - profiles
dependencies:
  - TASK-112
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: TASK-74's app_not_running retains every present SingletonLock because a pid may belong to another PID namespace. TASK-112's Owner PID protocol (docs/safety.md Producer liveness) proves a pid gone when diskgc runs in the initial PID namespace. A browser killed without cleanup leaves its lock forever, so its cache is never reclaimed. Optional refinement: when the lock's host part equals this host's name and Owner PID says gone, the lock is stale. Needs a decision on shared home directories across hosts (the host-name check) and a real-browser container test (kill -9 leaves the lock). Low priority: cost is retained cache bytes, never a deletion.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Decision recorded whether stale-lock reclaim is wanted
- [ ] #2 If wanted: model vectors for same-host dead pid (eligible), foreign host (retained), namespace pid alive (retained), nested diskgc namespace (retained)
<!-- AC:END -->
