---
id: TASK-254
title: >-
  Container storage recogniser: other engines (LXD/Incus, standalone BuildKit,
  VM disk images) and roots named inside storage
status: To Do
assignee: []
created_date: '2026-10-30T23:14:00Z'
labels:
  - safety
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-242 recognises containers/storage graphroots, Docker data roots and containerd roots by marker names. Not recognised: LXD/Incus storage pools, a standalone buildkitd root (/var/lib/buildkit, ~/.local/share/buildkit: runc-overlayfs/snapshots, cache.db), libvirt/QEMU/VM disk images, Nix store paths. Also the root's own ancestors are not looked at: a root named inside a storage root is taken as meant (same as keep markers). Decide whether any of these needs a structural recogniser or the ancestor check; measure a real layout first.
<!-- SECTION:DESCRIPTION:END -->
