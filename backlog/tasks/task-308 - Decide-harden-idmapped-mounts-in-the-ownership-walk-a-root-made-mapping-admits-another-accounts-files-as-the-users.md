---
id: TASK-308
title: >-
  Decide + harden: idmapped mounts in the ownership walk (a root-made mapping
  admits another account's files as the user's)
status: To Do
assignee: []
created_date: '2026-11-01T20:59:00Z'
labels:
  - needs-mark
  - safety
  - ownership
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-291 review (Codex BLOCKER vs mped recommendation, 2026-11-01). The ownership walk checks the uid the kernel shows through the mount. On an idmapped bind that maps another account's uid (e.g. a service account) to the user's, that account's files are admitted and deletable (the kernel also lets the user unlink them; their processes stay outside the non-root scan). systemd-homed relies on idmapped homes, so refusing every idmapped mount would stop every homed user reclaiming in their home. Merged state: documented limit in safety.md (Ownership of every entry). Mark decides: accept the limit (mped's recommendation: root made the mapping, the kernel already grants the unlink, the in-use blind spot is the PRD's existing 'other users' processes' class), or refuse. Hardening either way: on kernels whose statmount exposes the uid map (STATMOUNT_MNT_UIDMAP, verify 6.15+), mark an entry not_owner when its stored (inverse-mapped) uid is another existing account; admit when it is the user's own or an unallocated/foreign range (what homed uses).
<!-- SECTION:DESCRIPTION:END -->
