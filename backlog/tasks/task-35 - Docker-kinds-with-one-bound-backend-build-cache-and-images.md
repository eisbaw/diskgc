---
id: TASK-35
title: 'Docker kinds with one bound backend: build cache and images'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:51'
labels:
  - wave-4
  - irreversible
  - safety
dependencies:
  - TASK-34
  - TASK-57
references:
  - docs/safety.md#docker-kinds
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: Docker caches are large and the prune commands delete broadly. Implement backend binding per docs/safety.md#docker-kinds (local system socket only, default builder with docker driver, storage roots incl. the containerd store root) used by tally, preflight and execution; build-cache prune with --all and until; dangling and unreferenced image prune; settle containerd-root discovery.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 dind container tests: remote context, rootless socket and non-default builder are skipped with their reasons
- [ ] #2 Volumes and containers are never removed (asserted before/after)
- [ ] #3 The containerd storage root is found with the method from the spike; undeterminable setups are unsupported_backend
- [ ] #4 The dind image is pinned by hash (offline), runs privileged with a tmpfs for /var/lib/docker
- [ ] #5 Killing diskgc while docker prune runs leaves the host lock held until the docker client exits
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
