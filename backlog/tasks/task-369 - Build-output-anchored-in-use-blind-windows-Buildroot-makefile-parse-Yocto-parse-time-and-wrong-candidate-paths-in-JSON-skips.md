---
id: TASK-369
title: >-
  Build-output anchored: in-use blind windows (Buildroot makefile parse, Yocto
  parse-time) and wrong candidate paths in JSON skips
status: To Do
assignee: []
created_date: '2026-11-07T15:33:00Z'
labels:
  - profiles
  - in-use
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-354 findings 3-5: (3) while Make reads Buildroot's makefiles (it runs the compiler then) and between steps nothing in output/ is held, so a reclaim then deletes under a live build; Yocto likely has a similar parse-time window (undocumented). (4) JSON skip entries for a moved/copied Meson output and a Bazel workspace on another fs name the marker file or the workspace, not the output dir. (5) Yocto's in_use path is untested (the task FIFO decides first as contains_ipc; a Python task makes no FIFO). Also (6) pre-existing harness gap: the oracle's strace check ignored diskgc's own unlinks inside allowlisted concurrent-writer paths (t354 removed its use of the allowlists).
<!-- SECTION:DESCRIPTION:END -->
