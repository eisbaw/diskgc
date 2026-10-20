---
id: TASK-97
title: >-
  Profile: podman/buildah leftovers in /var/tmp (buildah*,
  container_images_storage*)
status: To Do
assignee: []
created_date: '2026-10-19 12:19'
updated_date: '2026-10-20 01:10'
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
- [x] #1 Profile with examples matches /var/tmp/buildah* and /var/tmp/container_images_storage* top-level entries only, idle-gated
- [ ] #2 A file a running podman/buildah holds open is in_use (container test with a held file)
- [x] #3 A read-only dry run on this machine lists them with sizes matching du
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-20 implementation in isolated worktree: generic source min_idle_for floor (3d for containers-image-tmp), exact model test and container fixture prepared. Read-only host dry run listed four eligible /var/tmp directories totaling 73,584,001,024 estimated bytes; each exact byte estimate matched du -sxB1. No host deletion. Container E2E and integration gate pending; leave task To Do until they pass.

Focused container E2E fs_reclaim_image_temp_minimum_idle_and_open_file passed (11.39s): old free spool deleted by oracle, held old Buildah-shaped file in_use, fresh entry not_idle. Fixture holder is a generic process, not a live Podman/Buildah operation; no buildah CLI installed on host. Full integration E2E/review pending.

Final integrated nix develop -c just e2e passed 93/93 on 2026-10-20; QA and MPED reviews GO for the opt-in profile tranche. AC #2 stays open: the container E2E holds a Buildah-shaped file with a generic process, but a live Podman/Buildah operation has not been exercised. No live host deletion.
<!-- SECTION:NOTES:END -->
