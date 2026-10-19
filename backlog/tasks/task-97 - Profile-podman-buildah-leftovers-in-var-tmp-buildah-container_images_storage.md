---
id: TASK-97
title: >-
  Profile: podman/buildah leftovers in /var/tmp (buildah*,
  container_images_storage*)
status: To Do
assignee: []
created_date: '2026-10-19 12:19'
labels:
  - library
dependencies:
  - TASK-18
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the largest single reclaimable item on the dev laptop: 68.5 GiB in /var/tmp/buildah* and /var/tmp/container_images_storage* (four ~20 GB files from 2026-10-15, owned by the user, no visible process holding them). Podman and buildah leave these temp files behind when a load or build is interrupted (as diskgc's own e2e image load did on 2026-10-19 under ENOSPC). Plain delete is right here (they are not storage, only spool files), gated on in-use and idle. Source: read-only gap investigation 2026-10-19 (scratchpad gap/summary.txt).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Profile with examples matches /var/tmp/buildah* and /var/tmp/container_images_storage* top-level entries only, idle-gated
- [ ] #2 A file a running podman/buildah holds open is in_use (container test with a held file)
- [ ] #3 A read-only dry run on this machine lists them with sizes matching du
<!-- AC:END -->
