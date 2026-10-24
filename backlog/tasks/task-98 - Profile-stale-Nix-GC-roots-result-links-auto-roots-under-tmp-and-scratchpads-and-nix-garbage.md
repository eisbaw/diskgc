---
id: TASK-98
title: >-
  Profile: stale Nix GC roots (result links, auto roots under /tmp and
  scratchpads) and nix-garbage
status: To Do
assignee: []
created_date: '2026-10-19 12:19'
updated_date: '2026-10-24 08:32'
labels:
  - library
dependencies:
  - TASK-36
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: dead Nix store paths were only ~2 GiB, but 19-74 GiB of the 157 GiB store is held alive by stale auto GC roots: one old result link pins a whole 25.11 system closure (19 GiB with old generations), auto roots under /tmp (mostly agent scratchpads) and ~/.cache, direnv. Deleting a stale result symlink (an out-link nobody uses) then running the Nix GC is the reclaim. Needs: a profile for stale out-link symlinks that are registered auto roots (idle-gated, owned by the user, never the current system or profiles), then the nix-store kind's GC (task-36). Source: read-only gap investigation 2026-10-19 (scratchpad gap/summary.txt).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A profile finds result-style out-links registered under /nix/var/nix/gcroots/auto that point into the store and are idle, never system or user profile links (model tests)
- [ ] #2 Composed with the nix-store GC kind: a dry run reports the store bytes the GC would then free
- [ ] #3 Verified read-only on this machine
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-20 read-only feasibility audit (Nix 2.34.8 host): /nix/var/nix/gcroots/auto has 393 symlink registrations; 392 target extant out-link symlinks into /nix/store, 183 target user-owned result/result-* out-links, 67 target profile paths, and one is dangling. `nix-hash --type sha1 --base32 --flat /dev/stdin` of the diskgc result path matched its auto-root name. Nix source (https://github.com/NixOS/nix/blob/master/src/libstore/gc.cc, addIndirectRoot/findRoots) confirms auto name = Nix32(SHA1(out-link path)); GC follows auto -> out-link -> store. Do not run nix-store --gc --print-roots or another roots query as a read-only audit: findRoots removes dangling auto entries as a side effect. No host links removed.

Implementation dependency/safety design: The current fs profile EntryType and discovery refuse every symlink candidate, and ordinary engine deletion cannot yet express the root registration check. A dedicated no-follow out-link protocol must verify a user-owned result-style symlink under supplied roots, its absolute store target, exact registration name and readlink content under /nix/var/nix/gcroots/auto, exclude /nix/var/nix/profiles and all system/user profile links, check idle/in-use and ownership, then recheck both link identities/content immediately before fd-relative unlink. Avoid unlinking root-owned auto entries directly; Nix cleans a dangling auto entry during its later GC. This needs model and isolated-Nix-store E2E tests, including forged registration, symlink swap, active result, profiles, and alternative NIX_REMOTE.

AC #2 is blocked by TASK-36 (nix-store kind currently rejected by profile loader) and a further what-if estimation problem: before out-links are removed, Nix still treats them as live, so current dead-path queries cannot report the store bytes that a subsequent GC would free. Implement and verify an explicit hypothetical-root closure analysis or revise the AC to report link count plus a separately measured post-removal GC result; do not call a host GC query read-only because Nix root discovery mutates stale registrations. No profile shipped in this audit.

From TASK-118 (2026-10-24): the nix-store kind (designed, docs/safety.md nix-store) never removes out-links or gcroots/auto registrations itself; Nix's root discovery (its pre-GC root check and the GC) unlinks only registrations whose out-link is gone. Removing stale but extant out-links remains this task's protocol. Nix units are unsized (architecture.md, The tally): AC #2's 'store bytes the GC would then free' still needs the hypothetical-root analysis noted above; nix-store's own dry run reports estimated_bytes null and does not run --query --roots.
<!-- SECTION:NOTES:END -->
