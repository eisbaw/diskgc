---
id: TASK-11
title: Cargo race tests 1-9 against every supported Cargo version
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 00:22'
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
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
