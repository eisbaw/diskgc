---
id: TASK-4
title: Cargo-under-test toolchains and deterministic race helpers in the test image
status: To Do
assignee: []
created_date: '2026-10-16 22:39'
updated_date: '2026-10-17 00:28'
labels:
  - wave-1
  - testing
dependencies:
  - TASK-2
  - TASK-59
references:
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the Cargo race tests are the load-bearing acceptance signal and must be deterministic. Install pinned rust-overlay toolchains (oldest supported stable, one dated nightly) with a C toolchain under /opt/cargo-under-test in the test image; write the Rust helpers: a lock holder (holds .cargo-lock so Cargo blocks after taking .cargo-build-lock), a build.rs that waits on a control file, a runner wrapper that pauses between compile and run; offline fixture projects without registry dependencies.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A smoke build per toolchain runs first and fails as a toolchain problem, not a race failure
- [ ] #2 Each helper's pause/resume is verified by sampling /proc/locks in a container test
- [ ] #3 No network access is needed by the fixtures (CARGO_NET_OFFLINE=true)
- [ ] #4 A traced smoke cargo build per toolchain stays within a stated bound under strace --seccomp-bpf: tracee ptrace stops counted by voluntary_ctxt_switches as in strace_seccomp_bpf_churn_within_bound, plus a generous wall-time bound (carried from task-59 AC#2)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: Pins at scaffold time: rust-overlay ec07af4 (2026-10-16) has stable manifests up to 1.99.1. The product toolchain is set by rust-toolchain.toml only. The Cargo-under-test toolchains are separate: pick them from the same locked rust-overlay input (rust-bin.stable."X.Y.Z" / rust-bin.nightly."YYYY-MM-DD") so they are pinned by flake.lock.

Forward-carried from task-2: (1) Extension point: e2eBaseImage.contents in flake.nix (marked with a task-4 comment). Toolchains are base-image inputs: changing them changes the image tag (its Nix hash), and the runner reloads the image once (podman load of an 80 MB base took about 2 s). (2) Each test gets a fresh tmpfs at /tmp (nosuid,nodev; exec allowed) and runs with --network=none, so fixtures must be offline. Fixture project files that are not .rs/.toml must be added to the flake source filter. (3) Helpers that must hold locks while the code under test runs need a spawn (non-blocking) variant next to under_test::run_as_tester, which blocks until exit. Root-owned helpers are plain std::process::Command children of the harness. /proc/locks sampling from container root works. (4) The harness's records tmpfs (/run/diskgc-e2e) is mode 0700 root, so control files that a uid-1000 process must see belong on the scratch mount.

Forward-carried from task-59: (1) task-59 AC#2 ('a traced cargo build stays within a stated time bound') could not be met without Cargo in the image; it is the new last AC here. Pattern: suite.rs strace_seccomp_bpf_churn_within_bound. Wall time on this shared host varies several-fold (0.66-4.96 s for the same churn), so bound the tracee's own voluntary_ctxt_switches (one per ptrace stop, deterministic: 2 per traced call) and keep wall time as a generous budget only. (2) The snapshot skips read-only mounts but walks the overlay root. Toolchains baked into the image's contents land on the overlay root and would be walked by every snapshot (slow, and keyed without btime). Mount them read-only instead, e.g. a store path bind-mounted :ro like the under-test dir, or a ro bind inside the container, so plan_walk skips them. The log line 'mounts not walked: ...' shows whether it worked. (3) /home/tester is now a fresh tmpfs per test: anything under it (CARGO_HOME, ~/.cargo/config.toml, an offline registry) must be provisioned per test through ctx.home (Scratch), never baked into the image (it would be shadowed). (4) Every traced run of code under test must be judged by an Oracle armed before it, or the test fails (Ctx::teardown). Helpers that are fixtures, not code under test, run untraced via Command. (5) Scratch::mount now takes an owner.

Orchestrator: task-59 stays In Progress until this task demonstrates the traced cargo build within a stated time bound (task-59 AC#2); close task-59 when that AC here passes.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
