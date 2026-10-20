---
id: TASK-104
title: Native Docker Engine dind E2E fixture
status: Done
assignee: []
created_date: '2026-10-20 04:00'
updated_date: '2026-10-20 04:22'
labels: []
dependencies:
  - TASK-2
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Add a digest-pinned Docker Engine-in-Docker image and an isolated E2E smoke test so profile and Docker-kind tests can run against a real Docker daemon. The host docker CLI is Podman-backed and cannot prove Docker behavior. The daemon and all state must live inside disposable test containers, with no host Docker socket or developer project path mounts.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A pinned Docker dind image is built or pulled reproducibly and launched as a disposable E2E container with the sandbox interlock intact
- [x] #2 A smoke test asserts the server identifies as Docker Engine, creates a scratch image/container/volume inside the container, and leaves the deletion oracle and host paths untouched
- [x] #3 The daemon storage is isolated in tmpfs or disposable overlay, and the test fails loudly if rootless Podman cannot start dockerd; no Podman-backed CLI is counted as native Docker proof
- [x] #4 The fixture is usable by TASK-35 and TASK-103 profile replay; docs/testing.md describes the exact command and limitations
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-20 commit 3ae1448: digest-pinned Docker Engine 29.8.2 dind fixture, private tmpfs daemon storage, native server identity assertion, offline image/container/volume and clean oracle. nix develop -c just e2e passed 124/124 Nix and 1/1 native Docker; parallel QA/MPED reviews GO. Fixture only; Docker cleanup remains TASK35/57.
<!-- SECTION:NOTES:END -->
