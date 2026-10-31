---
id: TASK-60
title: >-
  Harness hardening from task-2 review: privileges, deadlines, image pruning,
  guards
status: Done
assignee: []
created_date: 2026-10-16 23:54
updated_date: '2026-10-31T22:28:00Z'
labels:
  - wave-1
  - hardening
  - testing
dependencies:
  - TASK-2
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: lower-priority findings from the task-2 review, fixed once the harness is stable. (1) Set no-new-privileges and assert CapAmb and CapInh are 0 for the code under test (the bounding set still holds SYS_ADMIN and SYS_PTRACE). (2) The budget does not cover check, the nix build, test listing or image loading; give those deadlines too. (3) Old base-image tags accumulate; prune them (a disk GC tool should not leak disk). (4) Scratch::mount calls sandbox::check() again as a second guard. (5) A clippy disallowed-methods rule for std::fs writes outside scratch.rs. (6) An automated test for e2e-one keeping the container on failure (verified manually only).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Each of the six items is implemented with a test or a recorded reason for not doing it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-59 (hardening candidate): the mandatory-oracle ledger is only as strong as its single entry point. A test could build its own under_test::Ledger, or spawn code under test with std::process::Command and bypass strace and the ledger. Consider extending item (5) (clippy disallowed-methods) to std::process::Command outside under_test.rs and the probes, or making Ledger constructible only by Ctx.

Forward-carried from task-59 review (hardening): (1) The 10 s wall-time check in strace_seccomp_bpf_churn_within_bound may flake under load (the same traced churn measured 0.66-4.96 s on this host). Make it a logged budget or loosen it; the voluntary-context-switch bound (40,010 measured, bound 80,000) is the real test. (2) Enable clippy let_underscore_must_use for diskgc-e2e so that 'let _ = oracle.judge(..)' cannot satisfy the judged-run rule while discarding the Verdict (#[must_use] alone does not catch let _). (3) Document in oracle.rs and docs/testing.md that Unmatched::CreatedDuringRun is a guess: creates are not traced. It is inferred because the nearest known ancestor is a directory, so a removal of a pre-existing object under a path the snapshot never saw (e.g. below a skipped mount) is classified the same way.

From the task-4 review (mped), harness items: H1 mount the Cargo toolchain closures from the host store (closureInfo) instead of baking them into the 2.84 GB base image; pair with item (3), tag pruning. H2 Ctx::setup always bind-mounts /nix/store read-only and the test listing requires the toolchain manifest (/opt/cargo-under-test/toolchains); both break the planned Debian (non-Nix) variant and the own-store Nix-daemon container, so each needs a per-image opt-out. H3 log the CPU count next to the traced-build ratio in cargo_traced_build_within_bound. H4 one builder for the as-tester + env_clear + TESTER_ENV command, now built in four places (cargo_under_test: Cargo::spawn, run_as_tester_untraced; cargo_suite: full_trace; holder: Holder::start). H5 a no-create flag for probe flock-hold (it creates a missing lock file today). H6 bisect the stables 1.91-1.98 for distinct lock behaviours and pin one toolchain per behaviour.

H2 (per-image opt-out of the /nix/store bind mount and the toolchain manifest) moved to task-63, the Debian / static-binary e2e variant, for the 0.1 ensure MVP (2026-10-18).

2026-10-22 gate reliability evidence from TASK-16 extension: exact just e2e fast Nix suite ran 219 cases at 14-way concurrency with one global 300s deadline; 218 passed and cargo_race_6_killed_at_seeded_positions_the_next_build_succeeds@1.99.1 was stopped by that global deadline after completing 106/119 successful seeded kill/recovery cycles, with about 60s of suite time remaining when it started. The suite ended at 301.7s; a west source-anchor case took 146.86s. The same Cargo race case passed alone in 42.23s (one-test suite 46.5s). Forty-two unrelated arch-explorer busy-loop shells were consuming host CPU throughout; no diskgc process or host files were touched. This is a capacity/load diagnosis, not a per-test 60s cap or proven product defect. When hardening harness deadlines and CPU reporting, test and explain suite-budget behavior under external contention; require a later exact gate pass on a suitable host before calling the TASK-16 change verified.

Orchestrator 2026-10-23 (COMPASS consult): raised to High: TASK-13 (on the release path) depends on it.

Orchestrator 2026-10-24 (COMPASS round 2): tmp/worktrees/task60-harness holds 644 lines of uncommitted TASK-60 changes on a base ~40 commits behind master. Compare with HEAD and the tracker before restarting; do not redo or discard it blindly.

2026-10-31 port of WIP 7e4bd72 onto master (branch t60-harness-port, worktree tmp/worktrees/t60; not merged). Per item:
(1) DONE. runner podman_run_args adds --security-opt no-new-privileges for every runtime; probe whoami prints CapInh/CapAmb/NoNewPrivs; tester_cannot_read_other_users_mode000_dir requires all four cap sets 0 and NoNewPrivs 1 (Nix and Debian images pass). Bite: runner without the flag -> that test FAILs naming the flag. Buildah spool test passes under it (its setuid newuidmap copies are now inert); Docker dind (--privileged) fast suite passes.
(2) DONE. Runner setup (canary, podman info x2, image exists/load, listing) runs via output_by: own process group, SIGKILL at the run deadline (suite budget; 15 min setup for one). Unit tests incl. a bite (leader-only kill -> red). Cleanup podman kill/rm bounded and reported. Gate recipes run check under tools/deadline.py 3600 (forwards Ctrl-C; coreutils timeout would orphan cargo); justfile appends max-silent-time = 1800 to NIX_CONFIG (verified: a silent builder fails 'timed out after 3 seconds' with max-silent-time 3). Not bounded: Nix evaluation and substitution.
(3) DONE as an explicit tool, not automatic: just e2e-images [--apply] (tools/e2e_images.py, unit-tested with a fake rmi). Dry run on this host listed one old tag, localhost/diskgc-e2e-base:c55xq8rnqgjks8ndvlycwhjvihlc7wi5 (<= 4.18 GiB). --apply NOT run on the host (host deletion needs Mark's authorization). Runner does not prune itself: parallel worktrees on other inputs would thrash and race.
(4) DONE. Scratch::mount/mount_sized call sandbox::check() first; unit test asserts the refusal message (Nix build sandbox).
(5) DONE. diskgc-e2e/clippy.toml disallows std fs mutators (write, File::create[_new], create_dir[_all], DirBuilder::create, remove_*, rename, copy, hard_link, set_permissions, unix symlink/chown/lchown); scratch.rs allowed; every other use has #[expect(..., reason)]. e2e-clippy went red at each site before the expectations; an unfulfilled expectation also fails. Not covered: OpenOptions::open, rustix, spawned processes.
(6) DONE. Tier::Probe test runner_failure_probe_fails_on_purpose (in no suite; unit test) plus tools/e2e_one_retention.py in e2e and e2e-full: failed one keeps an exited container with the probe message; passing one leaves none; removes only its own containers. Bite: runner removing failed containers -> check red.
Forward-carried items NOT done (follow-ups): std::process::Command outside under_test.rs / Ledger only via Ctx; CHURN_BOUND 10 s wall-time check; let_underscore_must_use; CreatedDuringRun-is-inferred doc; task-4 H1, H3-H6.
<!-- SECTION:NOTES:END -->
