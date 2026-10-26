---
id: TASK-241
title: >-
  Host::executable_file stats absolute paths without the mount screen (command
  preflight PATH walk)
status: To Do
assignee: []
created_date: '2026-10-25 23:15'
labels:
  - safety
  - mounts
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the t182 review (TASK-182). crates/diskgc-linux/src/host.rs Host::executable_file calls std::fs::metadata on absolute paths: it follows symlinks and can trigger an automount or block on a dead network/FUSE mount. Callers: command_action.rs resolve_program, walking diskgc's own PATH and an env wrapper's configured PATH=. Not a regression; outside resolve_in's screen. Fix: screen each candidate directory's mount chain (resolve_in + open_beneath, or screen_mount_chain on a fresh table) before the lookup; ModelHost executable_file uses fixture lookup and is not held to dead: true mounts (TASK-182 note) - make it so. Vector: PATH entry under a dead mount.
<!-- SECTION:DESCRIPTION:END -->
