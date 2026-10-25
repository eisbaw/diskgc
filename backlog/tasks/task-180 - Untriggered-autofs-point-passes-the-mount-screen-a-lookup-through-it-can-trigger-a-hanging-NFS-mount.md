---
id: TASK-180
title: >-
  Untriggered autofs point passes the mount screen: a lookup through it can
  trigger a hanging NFS mount
status: Done
assignee: []
created_date: '2026-10-25 05:33'
updated_date: '2026-10-25 09:55'
labels:
  - safety
  - mounts
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-178 review. An untriggered systemd automount (x-systemd.automount NFS) shows as autofs in mountinfo; may_be_networked does not list autofs, so screen_mount_chain accepts a root such as /mnt/nas/x and resolve_in / path_is_absent / executor preflight walk through the trigger, mounting NFS; with the server down the lookup hangs. Decide: fail closed on autofs anywhere on the chain in screen_mount_chain, or document as a known limit in docs/safety.md. ModelHost cannot model automount today.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
lane-c (863347a, 15cb544): decided fail closed. may_be_networked refuses fs type autofs (AUTOFS const), so screen_mount_chain and screen_overlap_mounts refuse a root at/below an untriggered automount from mountinfo without lookup; a triggered one is judged by the fs stacked on it (NFS refused, local /boot used). Untriggered local automounts (/boot, /efi via systemd-gpt-auto) are now refused too: accepted fail-closed cost. Unit test resolve.rs; vector config/automount-roots-never-looked-up.yaml (dead autofs mount; mutation without the refusal fails the corpus). safety.md No network filesystems. No container test: rootless Podman cannot mount autofs. Awaiting merge.

Merged to master as 9f317cb on 2026-10-25T09:54Z.
<!-- SECTION:NOTES:END -->
