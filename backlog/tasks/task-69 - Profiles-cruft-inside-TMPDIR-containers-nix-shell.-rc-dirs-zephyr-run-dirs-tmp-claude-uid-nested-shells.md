---
id: TASK-69
title: >-
  Profiles: cruft inside TMPDIR containers (nix-shell.*, rc dirs, zephyr run
  dirs, /tmp/claude-{uid}, nested shells)
status: To Do
assignee: []
created_date: '2026-10-18 11:05'
labels:
  - wave-2
  - profiles
  - spike
dependencies: []
references:
  - docs/reference.md#built-in-profile-library
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: task-68's dry run on a real laptop (2026-10-18) found most /tmp cruft one or more levels down, inside directories that are some tool's TMPDIR, not at the top of /tmp: 4,184 of 4,185 eligible pytest-of-* dirs (10.4 GiB) lie in nix-shell.* or tmp.*; renode-<pid> dirs in zephyr-*-run.* and nix-shell.*; node-compile-cache and playwright* only in nix-shell.* (and nix-shell-*-* rc dirs); nested shells put pytest-of-* at depth 3 (nix-shell.X/nix-shell.Y/...); and Claude Code makes /tmp/claude-{uid} the TMPDIR of its tools, so 1,302 nix-shell.* dirs (and tmp*, loose files) live there, out of reach of nix-shell-tmpdir (root /tmp, depth 1). Without `**`, every profile repeats the container patterns (nix-shell.*/X, tmp.*/X, nix-shell-*-*/X, zephyr-*-run.*/X, claude-{uid}/X, nix-shell.*/nix-shell.*/X ...), and each new container family edits every profile (and, for built-ins, renames them).

Decision for Mark: (a) keep spelling containers out per profile (status quo; cheap, explicit, grows); (b) let a profile's roots name other profiles' matches (e.g. `roots: [/tmp, {profile: nix-shell-tmpdir}]`), so a container family is written once; (c) a fixed set of TMPDIR roots in config; (d) accept that the outer container profile (nix-shell-tmpdir, zephyr-tmp) is the unit of deletion and drop the inner rows for /tmp. Evidence and per-profile numbers: task-68 notes and crates/diskgc-core/src/fs_kind/spike-profiles.yaml.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The decision is recorded in docs/reference.md (Profiles or Built-in profile library) and the affected library rows follow it
- [ ] #2 nix-shell.* dirs below /tmp/claude-{uid} are either reachable by a /tmp profile or documented as out of scope
<!-- AC:END -->
