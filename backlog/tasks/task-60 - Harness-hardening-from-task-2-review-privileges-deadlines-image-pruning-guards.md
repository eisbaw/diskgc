---
id: TASK-60
title: >-
  Harness hardening from task-2 review: privileges, deadlines, image pruning,
  guards
status: To Do
assignee: []
created_date: '2026-10-16 23:54'
updated_date: '2026-10-17 23:54'
labels:
  - wave-1
  - hardening
  - testing
dependencies:
  - TASK-2
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: lower-priority findings from the task-2 review, fixed once the harness is stable. (1) Set no-new-privileges and assert CapAmb and CapInh are 0 for the code under test (the bounding set still holds SYS_ADMIN and SYS_PTRACE). (2) The budget does not cover check, the nix build, test listing or image loading; give those deadlines too. (3) Old base-image tags accumulate; prune them (a disk GC tool should not leak disk). (4) Scratch::mount calls sandbox::check() again as a second guard. (5) A clippy disallowed-methods rule for std::fs writes outside scratch.rs. (6) An automated test for e2e-one keeping the container on failure (verified manually only).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each of the six items is implemented with a test or a recorded reason for not doing it
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-59 (hardening candidate): the mandatory-oracle ledger is only as strong as its single entry point. A test could build its own under_test::Ledger, or spawn code under test with std::process::Command and bypass strace and the ledger. Consider extending item (5) (clippy disallowed-methods) to std::process::Command outside under_test.rs and the probes, or making Ledger constructible only by Ctx.

Forward-carried from task-59 review (hardening): (1) The 10 s wall-time check in strace_seccomp_bpf_churn_within_bound may flake under load (the same traced churn measured 0.66-4.96 s on this host). Make it a logged budget or loosen it; the voluntary-context-switch bound (40,010 measured, bound 80,000) is the real test. (2) Enable clippy let_underscore_must_use for diskgc-e2e so that 'let _ = oracle.judge(..)' cannot satisfy the judged-run rule while discarding the Verdict (#[must_use] alone does not catch let _). (3) Document in oracle.rs and docs/testing.md that Unmatched::CreatedDuringRun is a guess: creates are not traced. It is inferred because the nearest known ancestor is a directory, so a removal of a pre-existing object under a path the snapshot never saw (e.g. below a skipped mount) is classified the same way.

From the task-4 review (mped), harness items: H1 mount the Cargo toolchain closures from the host store (closureInfo) instead of baking them into the 2.84 GB base image; pair with item (3), tag pruning. H2 Ctx::setup always bind-mounts /nix/store read-only and the test listing requires the toolchain manifest (/opt/cargo-under-test/toolchains); both break the planned Debian (non-Nix) variant and the own-store Nix-daemon container, so each needs a per-image opt-out. H3 log the CPU count next to the traced-build ratio in cargo_traced_build_within_bound. H4 one builder for the as-tester + env_clear + TESTER_ENV command, now built in four places (cargo_under_test: Cargo::spawn, run_as_tester_untraced; cargo_suite: full_trace; holder: Holder::start). H5 a no-create flag for probe flock-hold (it creates a missing lock file today). H6 bisect the stables 1.91-1.98 for distinct lock behaviours and pin one toolchain per behaviour.

H2 (per-image opt-out of the /nix/store bind mount and the toolchain manifest) moved to task-63, the Debian / static-binary e2e variant, for the 0.1 ensure MVP (2026-10-18).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
