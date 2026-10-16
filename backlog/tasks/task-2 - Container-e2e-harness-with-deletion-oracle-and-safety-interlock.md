---
id: TASK-2
title: Container e2e harness with deletion oracle and safety interlock
status: Done
assignee:
  - '@mped-architect'
created_date: '2026-10-16 22:39'
updated_date: '2026-10-16 23:54'
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
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 just e2e builds the base image once and reuses it when only code changed (measured: a code-only change does not rebuild the base)
- [x] #2 diskgc runs as uid 1000 inside the container; a test proves it cannot read a mode-000 directory owned by another mapped uid
- [x] #3 The deletion oracle catches a deleted object outside the eligible set and a replaced one (same path, new inode, e.g. a recreated lock file), proved with deliberately wrong fixtures; the strace record agrees
- [x] #4 The interlock refuses outside a container (proved with a do-nothing canary binary); a test touching a path outside its scratch mount aborts
- [x] #5 Per-test durations are reported; just e2e fails when it exceeds its 5-minute budget; just e2e-full exists for the slow suites
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Shared marker constant: crates/diskgc-linux/src/sandbox-marker (one file, read by Rust include_bytes! and copied byte-for-byte into the image by Nix); diskgc_linux::sandbox::check_markers() for the e2e interlock now and the product gate (task-50) later.
2. diskgc-e2e (separate project): lib modules interlock, scratch (tmpfs at /tmp + per-op mount-id check), snapshot (whole-FS statx walk, btime key with logged fallback), strace (-f -ff -y -xx record, parser), under_test (launch as uid 1000 via strace -u tester), oracle (eligible set + allowlist diff, strace agreement), probe (deliberately misbehaving stand-in for code under test), suite (registry: name, tier fast/full, fn), runner (host: canary refusal, base image reuse via podman image exists, list, parallel podman run per test, durations, budget deadline). Bins: diskgc-e2e (in container), diskgc-e2e-runner (host), diskgc-e2e-canary (do-nothing interlock canary).
3. flake.nix: e2e crane package (static musl), under-test dir (copied binaries, bind-mounted read-only), base image via dockerTools.streamLayeredImage (busybox, strace, passwd/group with tester 1000 + other 1001, marker, /run/diskgc/lock per the tmpfiles lines), tag = Nix hash; runner wrapped with makeBinaryWrapper pointing at image + under-test; e2e unit-test check.
4. justfile: e2e/e2e-full/e2e-one call nix run .#e2e-runner.
5. Tests: trivial, uid1000 vs mode-000 other-owner dir, canary passes in container, outside-scratch abort (symlink escape, .., bind mount inside scratch), oracle catches deleted-outside-set and replaced (unlink+recreate, rename-over) with strace agreement, oracle control (eligible + concurrent writer allowlist clean), full-tier oracle on a large tree.
6. Evidence: timings for base reuse, budget bite with --budget-secs 1, canary refusal on host; docs/testing.md updated where behaviour is now concrete; notes + forward-carries.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: (1) diskgc-e2e is a separate Cargo project with its own Cargo.lock. The flake builds it with crane's postUnpack 'cd diskgc-e2e' pattern (see e2eArgs/e2eDeps in flake.nix). Reuse those args for the e2e binaries in the image. They already target static musl, so they run in the Debian image too. (2) Root [profile.*] settings do not apply to diskgc-e2e. Update its lock with cargo update --manifest-path diskgc-e2e/Cargo.toml when workspace crates gain deps. The flake uses --locked and fails loudly if the lock is stale. (3) The justfile recipes e2e, e2e-full and e2e-one are placeholders that print 'NOT RUN ...' and exit 0. e2e and e2e-full run check first. Replace the placeholder lines. (4) Verified on this machine: rootless podman pulls docker.io/library/debian:stable-slim, and the static diskgc runs there with a read-only bind mount. (5) Flakes see only git-tracked files: new harness files must be staged before nix build or check sees them. (6) Container tests cannot run in the Nix sandbox, so they stay out of checks and run from just e2e.

Forward-carried from task-1 review (mped): (a) no musl C toolchain is configured, so prefer pure-Rust dependencies (rustls, no openssl; avoid C-backed crates) or configure TARGET_CC first; (b) add podman (or document the host requirement) and cargo-nextest if used to the dev shell; (c) replace the exit-0 'NOT RUN' placeholders of just e2e / e2e-full with the real harness, so CI can never report green with no e2e tests; (d) move the 'runs on a non-Nix image' check into a Debian container test.

Progress (task-2 implementer): harness landed in diskgc-e2e (interlock, scratch, snapshot, strace, oracle, under_test, probe, suite, runner) plus diskgc_linux::sandbox (marker constant + check shared with the future product gate). Images by Nix (streamLayeredImage, tag = Nix hash), binaries bind-mounted read-only, runner wrapped by makeBinaryWrapper; just e2e / e2e-full / e2e-one call nix run .#e2e-runner. Tests: sandbox_trivial, tester_cannot_read_other_users_mode000_dir, interlock_canary_passes_in_container, interlock_aborts_outside_scratch, oracle_clean_when_only_eligible_objects_go, oracle_catches_deletion_outside_eligible_set, oracle_catches_replaced_objects (fast); oracle_large_tree (full, 100k files).

Gotchas found (each fixed at the root, not worked around):
(1) strace -u tester fails in rootless Podman: the child setuids before strace attaches, becomes non-dumpable, and PTRACE_SEIZE needs CAP_SYS_PTRACE. Fix: strace (root) runs 'diskgc-e2e as-tester -- PROG', which is traced from the start and drops groups/gid/uid itself (raw per-thread syscalls, single-threaded at that point).
(2) strace -y prints bare fds (unlinkat(3, ...)) when a root tracer reads /proc/<pid>/fd of a uid-1000 tracee without CAP_SYS_PTRACE; relative paths become unresolvable. Fix: --cap-add SYS_PTRACE (container root only; tester has no caps, proved by whoami: CapEff/CapPrm 0). Documented in docs/testing.md Container setup.
(3) strace pads the return column ('...)            = 0'); the parser splits at the first '=' (safe with -xx: no raw '=' in strings).
(4) overlayfs copy-up (deleting a child copies up its parent dirs) keeps the overlay inode number but changes the birth time, which read as REPLACED /home and /home/tester. Fix: btime is not part of the key on overlay (mount id -> fs type from mountinfo); logged as fallback (path, dev, ino). Honest limit: an inode-number reuse on the overlay upper after delete+recreate is invisible to the snapshot there; strace still records it for traced processes. Documented in docs/testing.md Grounding.
(5) Never edit Rust with perl s|...|...| and escaped \| in the pattern: it becomes alternation and silently inserted text at file start. Use the Edit tool.
(6) The host /tmp tests in diskgc-linux sandbox unit tests write under std::env::temp_dir (also fine in the Nix build sandbox).

Orchestrator gate: qa-test-runner re-ran nix flake check (exit 0, cached), just check (9 passed / 0 failed), just e2e x3 (7/7 each, identical, base image reused), just e2e-full (8/8, oracle_large_tree 5.45 s), host canary refused (exit 86). mped-architect: GO; follow-ups filed as task-59 (prerequisite of task-4 and task-51: mandatory oracle, seccomp-bpf strace, tmpfs for /run/diskgc and home, fuse-overlayfs detection, unmatched strace logging, skip read-only mounts) and task-60 (hardening).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Container e2e harness landed (not committed; staged for the flake).

What: diskgc-e2e now holds the in-container harness (interlock, scratch tmpfs with per-operation mount-id check, whole-container snapshot, strace record, deletion oracle, uid-1000 launcher, probes, test registry with fast/full tiers) and the host runner. diskgc_linux::sandbox holds the single marker constant and check. The flake builds a base image (dockerTools.streamLayeredImage: busybox, strace, users tester 1000 and other 1001, marker, /run/diskgc/lock), tagged by its Nix hash. Binaries are bind-mounted read-only. just e2e / e2e-full / e2e-one run nix run .#e2e-runner.

Evidence per AC:
#1: cold run with the image removed: 'not present: loading', loaded in 2.16 s. After a code-only change (comment added to crates/diskgc/src/main.rs and diskgc-e2e/src/probe.rs, then reverted), nix rebuilt only diskgc, diskgc-e2e, under-test and runner. The base image output path stayed /nix/store/b11nb5kq...-stream-diskgc-e2e-base and the runner logged 'already loaded: reused (checked in 16.93ms)'.
#2: tester_cannot_read_other_users_mode000_dir shows Uid/Gid 1000 x4, Groups 1000, CapEff/CapPrm 0, EACCES on the uid-1001 mode-000 dir, plus both controls (own dir readable; root reads the other dir). sandbox_trivial runs diskgc --version through the same launcher.
#3: oracle_catches_deletion_outside_eligible_set flags exactly /tmp/keep/file (tmpfs, btime key) and /home/tester/.profile (overlay, fallback key). oracle_catches_replaced_objects flags the recreated .cargo-lock (ino 7 -> 9) and the renamed-over Cargo.lock (ino 8 -> 10). In both tests the strace record flags the same set. Control oracle_clean_when_only_eligible_objects_go is clean.
#4: the canary run on the host exits 86 (checked by the runner before every run, and by hand). interlock_aborts_outside_scratch: absolute, dotdot, symlink and bind-mount escapes each exit 86 with nothing created; the control case runs. interlock_canary_passes_in_container is the positive control.
#5: the runner prints per-test durations and a slowest-first table. With --budget-secs 2 it killed a running test (TIMEOUT), reported 'BUDGET EXCEEDED' and exited 1. Unit test the_budget_fails_a_run_that_passed_every_test. just e2e-full exists and runs fast + full (oracle_large_tree, 100k files).

Gates: nix flake check -L exit 0 (includes 24 e2e unit tests, 5 sandbox tests); just check exit 0; just e2e exit 0 (7/7 passed, 1.3 s container phase, about 7 s total); just e2e-full exit 0 (8/8 passed, 12.7 s).

Changed from the brief: containers also get --cap-add SYS_PTRACE (needed by strace -y), and the privilege drop happens inside the trace instead of through strace -u (see notes; documented in docs/testing.md). Limits: on the overlay root the snapshot key has no btime, so inode-number reuse there is invisible to the snapshot (strace still sees traced processes). The Debian/non-Nix check (task-1 review item d) is forward-carried to task-46. The keep-on-failure path of e2e-one was checked by hand with a broken under-test dir, not by an automated test.
<!-- SECTION:FINAL_SUMMARY:END -->
