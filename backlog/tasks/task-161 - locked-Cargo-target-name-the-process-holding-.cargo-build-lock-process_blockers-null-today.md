---
id: TASK-161
title: >-
  locked Cargo target: name the process holding .cargo-build-lock
  (process_blockers null today)
status: To Do
assignee: []
created_date: '2026-10-24 23:38'
labels:
  - shipping
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-155 description (walk 2026-10-24 dump 26): a target kept as locked says only 'held by another process (a Cargo building, checking, running or testing this target)' and JSON process_blockers is null. Naming the Cargo pid (e.g. from /proc/locks for the lock file inode, or the in-use scan's fds on .cargo-build-lock) would tell the user which build to wait for. The human summary already renders process_blockers as 'used by pid N (cmd)' (report/human.rs), so only the executor/inuse side is missing. Check the protocol: the lookup is diagnostic only and must not change the locked verdict.
<!-- SECTION:DESCRIPTION:END -->
