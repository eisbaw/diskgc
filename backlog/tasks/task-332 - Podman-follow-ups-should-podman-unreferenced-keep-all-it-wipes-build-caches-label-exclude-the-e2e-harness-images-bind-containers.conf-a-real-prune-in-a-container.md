---
id: TASK-332
title: >-
  Podman follow-ups: should podman-unreferenced keep --all (it wipes build
  caches); label + exclude the e2e harness images; bind containers.conf; a real
  prune in a container
status: To Do
assignee: []
created_date: '2026-11-03T22:42:00Z'
labels:
  - profiles
  - podman
  - needs-mark
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-93 (merged 2026-11-03, dev library). (1) Mark: podman 5.8.7's image prune --all also runs CleanCacheMount(): os.RemoveAll(<TMPDIR|image_copy_tmp_dir|/var/tmp>/buildah-cache-<uid>) with no lock or in-use check; a running RUN --mount=type=cache build loses its cache (sharing=locked does not help; its locks live inside). Keep podman-unreferenced as is with the residual, or drop --all (dangling only). (2) Label the e2e harness images in flake.nix and let the kind take a --filter label!=... exclusion so podman-unreferenced does not prune them. (3) Bind containers.conf (image_copy_tmp_dir decides which cache dir --all removes) and drop-in dirs. (4) A real --all prune in a container (rootless podman in the e2e container) before admission. (5) Pin the prune child's environment in vectors (also Docker's DOCKER_BUILDKIT=0, older gap). (6) Run the Podman query through the preflight-resolved binary. (7) roots: daemon naming for a daemonless backend before the schema freeze. (8) An --allow-root refusal vector.
<!-- SECTION:DESCRIPTION:END -->
