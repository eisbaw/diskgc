---
id: TASK-2
title: Container e2e harness with deletion oracle and safety interlock
status: To Do
assignee: []
created_date: '2026-10-16 22:39'
updated_date: '2026-10-16 23:16'
labels:
  - wave-1
  - foundation
  - testing
dependencies:
  - TASK-1
references:
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: destructive tests must never touch the live system, and every later e2e test needs a harness. Build the rootless-Podman harness per docs/testing.md (Container setup, Safety interlock): one container per test with --cap-add SYS_ADMIN and --network=none, harness as container root, diskgc run as uid 1000, other owners via mapped subuids; a base image with toolchains, tools, markers and the tmpfiles-provisioned host lock, tagged by its Nix store hash and reused; binaries in a thin layer; the deletion oracle (whole-container snapshot by path, device, inode and birth time, plus an strace record); the e2e (fast) and e2e-full gates with per-test timing.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 just e2e builds the base image once and reuses it when only code changed (measured: a code-only change does not rebuild the base)
- [ ] #2 diskgc runs as uid 1000 inside the container; a test proves it cannot read a mode-000 directory owned by another mapped uid
- [ ] #3 The deletion oracle catches a deleted object outside the eligible set and a replaced one (same path, new inode, e.g. a recreated lock file), proved with deliberately wrong fixtures; the strace record agrees
- [ ] #4 The interlock refuses outside a container (proved with a do-nothing canary binary); a test touching a path outside its scratch mount aborts
- [ ] #5 Per-test durations are reported; just e2e fails when it exceeds its 5-minute budget; just e2e-full exists for the slow suites
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: (1) diskgc-e2e is a separate Cargo project with its own Cargo.lock. The flake builds it with crane's postUnpack 'cd diskgc-e2e' pattern (see e2eArgs/e2eDeps in flake.nix). Reuse those args for the e2e binaries in the image. They already target static musl, so they run in the Debian image too. (2) Root [profile.*] settings do not apply to diskgc-e2e. Update its lock with cargo update --manifest-path diskgc-e2e/Cargo.toml when workspace crates gain deps. The flake uses --locked and fails loudly if the lock is stale. (3) The justfile recipes e2e, e2e-full and e2e-one are placeholders that print 'NOT RUN ...' and exit 0. e2e and e2e-full run check first. Replace the placeholder lines. (4) Verified on this machine: rootless podman pulls docker.io/library/debian:stable-slim, and the static diskgc runs there with a read-only bind mount. (5) Flakes see only git-tracked files: new harness files must be staged before nix build or check sees them. (6) Container tests cannot run in the Nix sandbox, so they stay out of checks and run from just e2e.

Forward-carried from task-1 review (mped): (a) no musl C toolchain is configured, so prefer pure-Rust dependencies (rustls, no openssl; avoid C-backed crates) or configure TARGET_CC first; (b) add podman (or document the host requirement) and cargo-nextest if used to the dev shell; (c) replace the exit-0 'NOT RUN' placeholders of just e2e / e2e-full with the real harness, so CI can never report green with no e2e tests; (d) move the 'runs on a non-Nix image' check into a Debian container test.
<!-- SECTION:NOTES:END -->
