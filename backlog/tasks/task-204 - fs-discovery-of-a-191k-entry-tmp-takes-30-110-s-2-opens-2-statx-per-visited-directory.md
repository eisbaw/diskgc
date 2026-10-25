---
id: TASK-204
title: >-
  fs discovery of a 191k-entry /tmp takes 30-110 s: 2 opens + 2 statx per
  visited directory
status: To Do
assignee: []
created_date: '2026-10-25 09:01'
labels:
  - performance
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-184 (strace of a host dry run of pytest-tmp): temp-prefix discovery visits ~145k directories under /tmp (nix-shell.*, nix-develop-*, nix-shell-*), each statx(name) + openat2(O_PATH) + statx(fd) + openat('.') + 2 getdents + 2 close; ~30 s warm, 75-110 s cold or loaded, before the first candidate is judged. Check whether one openat2 with O_DIRECTORY (RESOLVE_BENEATH|NO_XDEV|NO_SYMLINKS) plus one statx can keep the identity checks (docs/safety.md, Path traversal) and halve the syscalls; measure on the host read-only.
<!-- SECTION:DESCRIPTION:END -->
