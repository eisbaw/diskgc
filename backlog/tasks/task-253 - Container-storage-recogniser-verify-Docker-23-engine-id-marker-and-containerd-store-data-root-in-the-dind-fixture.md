---
id: TASK-253
title: >-
  Container storage recogniser: verify Docker 23+ engine-id marker and
  containerd-store data root in the dind fixture
status: To Do
assignee: []
created_date: '2026-10-30T23:14:00Z'
updated_date: '2026-10-30T23:37:00Z'
labels:
  - safety
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-242 recognises a Docker data root by (image + graph driver dir) or (engine-id + containers), and a containerd root by io.containerd.metadata.v1.bolt / io.containerd.content.v1.content. The vfs and containerd-store layouts are measured by the Docker 29.8.2 dind fixture, but the engine-id + containers pair (needed for a Docker 29 data root with the containerd image store, whose volumes/ and containers/ lie outside containerd/daemon) is from Moby's source, not measured. Add a read-only listing of <DockerRootDir> to the dind fixture and assert container_storage::in_listing recognises it (rootful and, if feasible, rootless Docker).
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
TASK-242 review (2026-10-31): not run before merge. The coordinator's gate was >= 15 GiB free on /; df showed 14.79 GiB, so the read-only ls of <DockerRootDir> in the Docker 29.8.2 dind fixture was skipped. Still open.
<!-- SECTION:NOTES:END -->
