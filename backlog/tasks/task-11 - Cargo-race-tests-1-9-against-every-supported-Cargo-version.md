---
id: TASK-11
title: Cargo race tests 1-9 against every supported Cargo version
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-18 01:33'
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

From task-4 (helpers and toolchains; docs/testing.md, Cargo toolchains under test): (1) Write a Cargo test as cargo_suite::CargoTest (fn(&Ctx, &Toolchain)); it is listed per toolchain as NAME@TOOLCHAIN (in the tier flake.nix cargoUnderTest gives its toolchain: fast on 1.99.1, full on 1.90.0 and nightly-2026-10-16) and the suite runs the toolchain's smoke check first, reporting a broken toolchain as TOOLCHAIN, not FAIL. (2) Fixtures: cargo_under_test::RACE.write(&ctx.scratch, rel) (tester-owned, on /tmp; a build script that waits at a gate, a binary printing RACE.output, a #[test]); SMOKE for a plain project. Built offline from an empty CARGO_HOME (/home/tester/.cargo). (3) Cargo runs: Cargo::new(&toolchain, &project_dir, &["build"]) (cwd = project dir, as diskgc's heuristics expect) .build_gate(&gate) / .runner_gate(ctx, &gate) / .env(k, v), then .spawn(ctx) -> CargoChild (pid, is_running, wait) or .output(ctx). These are untraced fixtures: declare their writes to the oracle (Fixture::concurrent_writer on the target or project dir) when a judged diskgc run overlaps them. (4) Gates: Gate::new(&ctx.scratch, name), gate.wait_until_waiting(REACH_TIMEOUT) -> helper pid, gate.open(&ctx.scratch). Lock holder: holder::Holder::tester(ctx, &["flock-hold", path], &[]) (exclusive; --shared available), released by holder.finish(). (5) /proc/locks: proc_locks::Watched::files(&cargo_under_test::lock_files(profile_dir)?) then .usage(&proc_locks::sample()?, pid) / .wait_for(pid, timeout, what, pred) / .locks_on(&locks). Sample in one read only (proc_locks::sample) - a multi-read sample repeated lines under the parallel suite. (6) Measured lock facts per toolchain are in docs/safety.md Lock files: 1.99.1 and the nightly hold .cargo-lock SHARED (plus .cargo-build-lock and .cargo-artifact-lock exclusive) during a build; with .cargo-lock held exclusively they wait holding .cargo-build-lock (race test 7's state comes for free with the holder). (7) Race test 8 (guidance from the task-4 review, mped): cargo run execs its runner in Cargo's own process, so while the runner gate waits the paused process has Cargo's pid but exe /opt/diskgc-e2e/bin/diskgc-e2e and cwd the project dir; cargo test starts the runner as a child of a still-running cargo. Do NOT use a copy of the wrapper named cargo-*. Assert the cargo test case on the existing heuristic (the parent cargo is alive with its cwd in the project directory). For cargo run through a runner, this task's AC #3 should revise the In use rule in docs/safety.md: a process whose initial environment has CARGO_MANIFEST_DIR inside (ancestor test) the target's project directory is in use. The runner inherits CARGO_MANIFEST_DIR and CARGO on all three pinned toolchains (measured by mped); this also partly covers the build-dir gap in (9). (8) Toolchains are at /opt/cargo-under-test/NAME (bin/) with the linker at /opt/cargo-under-test/cc/bin; Toolchain::env() gives PATH and CARGO_NET_OFFLINE=true. (9) Layout gaps measured in the task-4 review (docs/safety.md, Lock files and Known gaps): nightly-2026-10-16 uses the new build-dir layout by default (no .fingerprint/ or deps/ in the profile; build/<pkg>/<hash>/{fingerprint,out}), so race test 6 (fingerprint-first, half-deleted profile rebuilds) does not exercise what it claims there until task-53 revises step 3 and Last build. With CARGO_BUILD_BUILD_DIR, 1.99.1 and the nightly split the lock set (target/debug: .cargo-artifact-lock exclusive, .cargo-lock shared; build-dir debug/: .cargo-build-lock), both halves get CACHEDIR.TAG, and cargo test runs test binaries from the build-dir; 1.90.0 ignores the setting, so AC #4's build-dir variant is vacuous on 1.90.0 and the test must say so (log it as not applicable) rather than report a pass. (10) Correction to the task-59 note (1) above: --seccomp-bpf does not make a traced Cargo build pay only for the traced calls. strace 7.2 steps every new process and thread with PTRACE_SYSCALL until its first traced syscall, so about a third of a full trace's stops remain; task-4 states the bound relative to a full trace (cargo_traced_build_within_bound).

From task-9 (cargo_target in diskgc-core; docs/safety.md cargo-target rewritten): which race test verifies which decision.
- RT1 (build holds the lock): task-53 step 1 locked; in eligibility already in_use by evidence rule 1 (cargo holds lock-file fds) and rule 3 (cargo in the project dir).
- RT4 (test binary still running): rule 2 (exe text inside the target) + Scan::check on the entry record (exe/map by identity). Fixture support added: the race fixture's #[test] waits at DISKGC_E2E_TEST_GATE (Cargo::test_gate); measured: nightly test binaries run from target/debug/build/<pkg>/<hash>/out/.
- RT5 (new profile between discovery and locking -> changed): Inspected::same_locks (model test a_new_profile_or_a_replaced_target_after_discovery_is_changed). The pause point between discovery and locking needs task-53's driver op; cargo-targets locks nothing.
- RT6 (kill during deletion): last build is now layout-independent (newest mtime/ctime of everything in the profile dirs); fingerprint-first on the per-unit layout is task-53's.
- RT8 (cargo run/test paused between compile and run): rule 5 (the exec'd runner's CARGO_MANIFEST_DIR), rule 4 (LD_LIBRARY_PATH/OUT_DIR inside the target), rule 3 (cargo test's parent cargo). Already shown in_use per toolchain by cargo_running_cargo_keeps_its_target_in_use@*; RT8 adds the protocol, the oracle and that the binary then runs.
- RT9 (unknown lock name): judge_layout -> unsupported_layout (cargo_targets_discovered_and_judged@* shows it on real targets).
- AC #4: with a separate build dir both halves are unsupported_layout by decision (neither reclaimed; vacuous on 1.90.0, which ignores the setting: see cargo_split_build_dir_halves_are_unsupported_layout). With a shared target-dir from .cargo/config.toml outside the project, rule 3 does not see cargo between compile and launch (documented limit in safety.md In use); once the runner/binary starts, LD_LIBRARY_PATH names the target. If a race test shows the gap deleting a binary, record it as the limit, do not paper over it.
- Driver: diskgc-e2e-driver cargo-targets [--idle-for DUR] ROOT... prints every target, its profiles (layout and lock names) and verdict; an in_use detail lists every piece of evidence ('; ').

From task-9 round 2: H1, measure before deciding whether the build directory's half of a split pair can be reclaimed alone (AC #4): which locks Cargo takes in the build dir before writing there, and whether rule 6 (detached targets, any visible cargo blocks) covers cargo test's window. Until measured, both halves stay unsupported_layout. New container tests from task-9: cargo_running_executable_below_a_non_utf8_name_keeps_its_target_in_use@*, cargo_detached_target_is_in_use_while_any_cargo_runs@*.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
