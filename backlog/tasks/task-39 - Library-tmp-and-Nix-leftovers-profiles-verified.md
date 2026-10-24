---
id: TASK-39
title: 'Library: /tmp and Nix leftovers profiles, verified'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-24 07:45'
labels:
  - wave-4
  - config
  - library
dependencies:
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

2026-10-20 read-only built-in dry runs on this laptop, --min-free 500G, no deletions, full expanded temp-prefix v1 scope: mktemp-dir 3,360 candidates, 3,353 eligible, skips 5 contains_checkout/1 contains_ipc/1 not_owner, 213,622,784 eligible bytes; claude-code-tmp 326 candidates, 296 eligible, skips 9 in_use/16 contains_checkout/1 not_owner/2 contains_ipc/2 contains_special, 1,258,999,808 bytes; chromium-tmp 282 candidates, 270 eligible, 12 contains_ipc, 7,688,192 bytes; node-compile-cache 334 candidates, all eligible, 49,119,232 bytes; playwright-tmp 445 candidates, 444 eligible, 1 in_use, 2,043,904 bytes. For each profile, the three largest extant eligible candidates matched du -s -B1 byte for byte. Every built-in has match/no_match examples, verified_with, exact-definition/golden tests, ModelHost fixtures; cloned type mutations in golden.rs change every match set, and an intentional YAML type mutation was observed red then restored. Claude active sessions without a held scratch descriptor require tier idle policy or protect. nix-shell-rc, nix-shell-tmpdir and tmp-entry remain unshipped; TASK-39 stays To Do.

2026-10-20 RELEASE NO-GO for nix-shell-rc (supersedes the earlier implementation-ready note): Nix uses getpid() from the shell PID namespace in nix-shell-<pid>-<nonce> and nix-develop-<pid>-<nonce>. LinuxHost::new checks only that diskgc and its mounted /proc use the same PID namespace; it does not establish that Nix shells sharing /tmp use it. Real-kernel unshare --user --map-root-user --pid --fork --mount-proc proof: a live sleep task had host PID 3025375 and NSpid 3025375 9, while host /proc/9/status was absent. A live Nix shell in that namespace could create nix-develop-9-<nonce>; the proposed proc_status(Pid(9)) guard would report Gone and allow deletion. Same-namespace dry run and E2E cannot prove cross-namespace safety. Do not ship nix-shell-rc or expose the predicate until a sound all-namespace NSpid/liveness design with fresh pre-deletion recheck and measured cost is implemented and tested. nix-shell-tmpdir and tmp-entry remain unshipped for the separate dormant TMPDIR invisibility reason. TASK39 remains To Do. No host deletion.

Orchestrator 2026-10-24 (COMPASS round 2): dropped the TASK-38 edge. Remaining profiles are already in builtin.yaml; their admission is TASK-122's classification, dormant nix-shell sessions are TASK-112 steps 1 and 4. Shrink to verified_with and golden sets, or close into TASK-122 when it runs.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
