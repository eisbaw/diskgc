---
id: TASK-39
title: 'Library: /tmp and Nix leftovers profiles, verified'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-18 12:01'
labels:
  - wave-4
  - config
  - library
dependencies:
  - TASK-38
  - TASK-37
  - TASK-17
references:
  - docs/reference.md#built-in-profile-library
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: these are the profiles the motivating survey found. Ship pytest-tmp, nix-shell-rc, nix-shell-tmpdir, tmp-entry, mktemp-dir, claude-code-tmp, chromium-tmp, playwright-tmp and node-compile-cache with examples, golden sets, verified_with, and real dormant nix-shell / nix develop session tests.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each profile has examples, a golden set and verified_with from a real system
- [ ] #2 Dormant nix-shell and nix develop sessions keep their TMPDIR dirs (container test)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Dogfooding run 2026-10-17 (read-only probe of LinuxHost plus ncdu of $HOME, /tmp, /var/tmp; full report in the session scratchpad dogfood/). Proposed profiles: containers-image-tmp (/var/tmp/buildah*, /var/tmp/container_images_storage*: 68.5G here, single-file blobs left by buildah/podman; only the generic tmp-entry catches them today) and nix-user-cache (~/.cache/nix: tarball-cache, gitv3, eval-cache-v*; 4.1G, refetched on demand). /tmp had 147,557 top-level entries (101k nix-shell rc dirs, 38k nix-shell.*); a full walk took 265 s at idle IO priority, which is the PRD's 100k-entry case. 'pytest-tmp' matches were all nested inside nix-shell temp dirs.

Moved to task-65 for the 0.1 ensure MVP (2026-10-18): pytest-tmp, nix-shell-rc, nix-shell-tmpdir, mktemp-dir, tmp-entry, the proposed containers-image-tmp, and the dormant nix-shell / nix develop container test for the nix-shell profiles (AC #2). Still here: claude-code-tmp, chromium-tmp, playwright-tmp, node-compile-cache, and the proposed nix-user-cache.

From task-68's dry run on the laptop (2026-10-18; numbers and runtimes in task-68 notes; profiles in crates/diskgc-core/src/fs_kind/spike-profiles.yaml):
- pytest-tmp: 4,185 eligible / 10.4 GiB, 4,184 of them one level down; nested shells (depth 3) and rc dirs are not covered (task-69).
- nix-shell-rc: 65k of 105k judged in 20 min, all eligible, 350 MiB: no live shell protects its rc dir (nothing open, no environment names it); tiny value per candidate (task-70).
- nix-shell-tmpdir: 41,145 eligible / 10.96 GiB, 2 in_use (found by TEMPDIR=, not TMPDIR=), 199 contains_ipc. 1,302 more live below /tmp/claude-1000 (task-69).
- claude-code-tmp: reference.md row fixed to /tmp/claude-{uid}/-*/* (295 eligible, 2.7 GiB; the old `*/*` matched 36 more inside TMPDIR dirs). In use: 6, from Claude processes holding background-task output files open. An idle session has nothing open: only the idle filter and protect keep a live session's scratch (as the PRD says).
- chromium-tmp: row fixed: the hidden `.org.chromium.Chromium.*` are files; the bulk (260) are `org.chromium.Chromium.*` directories (Brave and Chromium URL fetcher / unpacker scratch). 257 eligible, 9 MiB, 11 contains_ipc.
- playwright-tmp: row fixed: none top-level; 71 inside nix-shell.* (0.3 MiB).
- node-compile-cache: 328 eligible, 28 MiB.
- mktemp-dir: 658 eligible, 145 MiB, 1 not_owner (root-owned file from sudo). Python's tmpXXXXXXXX (3,457 dirs) not covered (task-16, glob).
- tmp-entry: 79k of 155k entries judged in 20 min (deadline): 3.7 GiB eligible so far; /tmp/claude-1000 is not_owner as a whole because of one root-owned log in one session's scratchpad; 8 contains_special (Cargo targets of agent reviews); .bun-*.node mapped by claude (in_use).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
