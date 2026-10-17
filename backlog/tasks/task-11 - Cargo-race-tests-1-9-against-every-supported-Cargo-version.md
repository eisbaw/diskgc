---
id: TASK-11
title: Cargo race tests 1-9 against every supported Cargo version
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 12:01'
labels:
  - wave-1
  - irreversible
  - testing
  - acceptance
dependencies:
  - TASK-10
  - TASK-4
  - TASK-51
references:
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the acceptance signal for the Cargo kind; the PRD says the Cargo kind ships only if these pass. Implement race tests 1-9 from docs/testing.md as container tests using the pinned toolchains and the deterministic helpers.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 All nine tests pass on the pinned stable and nightly toolchains
- [ ] #2 Each test asserts via /proc/locks sampling and the deletion oracle, not only via diskgc's own report
- [ ] #3 If a test exposes a protocol flaw, the protocol in docs/safety.md is revised in the same change (honest failure, not a skipped test)
- [ ] #4 The tests also run with Cargo's separate build-dir and with a shared target-dir set in .cargo/config.toml
- [ ] #5 Race tests 5 and 6 use the pause/kill wrapper at seeded call positions
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-59: (1) Traced runs use strace --seccomp-bpf, so a traced cargo build pays only for the traced unlink/rename calls; task-4 states the bound. (2) Cargo's .cargo-lock recreation is flagged as REPLACED on the tmpfs scratch with the full (dev, ino, btime) key; on the overlay root only (dev, ino). Keep fixture projects on /tmp or /home/tester (both tmpfs). (3) Every traced run (each diskgc invocation; cargo too if it is run traced) must be judged by an Oracle armed before it, or Ctx::teardown fails the test. Concurrent cargo processes that are fixtures belong in Fixture::concurrent_writer. (4) Use suite::Expect::FailWith for deliberately wrong fixtures that must fail. (5) Assert timing with counts (voluntary_ctxt_switches, /proc/locks samples) rather than wall time; this host's wall time varies several-fold under load.

From task-51: the pause/kill mechanism is in place. diskgc-e2e-driver [--pause-at P] [--kill-at P] OP ARGS, P = K (K-th port call) or OP:N (N-th call of a Host method, e.g. unlinkat:2, flock_try:1); harness side ctx.spawn_as_tester -> Spawned::wait_paused / resume / wait (docs/testing.md, Controlled pause and kill points). Absolute positions are stable for a given tree: driver_delete_killed_at_a_host_call calibrates K with a paused run on an identical tree; for race test 6 the final 'done calls=N' line of an unpaused run gives the range to seed from. driver_delete_unlinks_what_replaced_an_entry_while_paused already hits the identity-check-to-unlink window at an exact call; the replace-between-walk-and-unlink test needs the engine's walk record (task-5) and a pause before the identity statx. Cargo operations for races 5 and 6 are new subcommands in driver::run (tasks 9, 53).

From task-51's review (mped): absolute call positions depend on the environment (the number of pids/proc_*/now/sleep calls varies with the processes in the container and deadline paths). Seed race test 6 with OP:N positions (flock_try:N, unlinkat:N), or calibrate from done calls=N in the same container.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
