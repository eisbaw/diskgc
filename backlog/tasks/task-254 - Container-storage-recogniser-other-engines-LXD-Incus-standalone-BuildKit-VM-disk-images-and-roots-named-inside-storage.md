---
id: TASK-254
title: >-
  Container storage recogniser: other engines (runroot, BuildKit, LXD/Incus, VM
  disk images)
status: To Do
assignee: []
created_date: '2026-10-30T23:14:00Z'
updated_date: '2026-10-30T23:37:00Z'
labels:
  - safety
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-242 recognises containers/storage graphroots and separate image stores, Docker data roots (incl. Docker 20 overlay) and containerd roots by marker names, below and above roots (the root's ancestors are asked since the TASK-242 review). Not recognised: the containers/storage runroot (/run/containers/storage, rootless $XDG_RUNTIME_DIR/containers or /tmp/storage-run-$UID: mount points and locks, no layers - check whether anything there is deletable and harmful to delete), a rootless or standalone BuildKit root (~/.local/share/buildkit, /var/lib/buildkit: runc-overlayfs/snapshots, cache.db), LXD/Incus storage pools, libvirt/QEMU VM disk images, Nix store paths. Measure a real layout first; decide which need a recogniser.
<!-- SECTION:DESCRIPTION:END -->
