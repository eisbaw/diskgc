---
id: TASK-180
title: >-
  Untriggered autofs point passes the mount screen: a lookup through it can
  trigger a hanging NFS mount
status: To Do
assignee: []
created_date: '2026-10-25 05:33'
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
