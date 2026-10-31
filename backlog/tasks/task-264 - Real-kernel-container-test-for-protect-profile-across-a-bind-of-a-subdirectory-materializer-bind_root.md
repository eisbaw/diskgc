---
id: TASK-264
title: >-
  Real-kernel container test for protect/!profile across a bind of a
  subdirectory (materializer bind_root)
status: To Do
assignee: []
created_date: '2026-10-31T02:53:00Z'
labels:
  - safety
  - test
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-260 review (mped S2): the model gained bind_root (bind of a subdirectory, mountinfo root field) but diskgc-e2e's model_materialize validate refuses it, so no real kernel has produced a subdirectory bind's root field, STATX_ATTR_MOUNT_ROOT on it, or exercised overlaps_in_filesystem. Add materializer support (mount --bind of a subdirectory inside the rootless test container's mount namespace) and a container test: protect /tmp/gen, tier root a bind of /tmp/gen/x, candidate refused; plus the private-propagation inner-mount case and an overlay upperdir case. Needs disk >= 15 GiB.
<!-- SECTION:DESCRIPTION:END -->
