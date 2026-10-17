---
id: TASK-4
title: Cargo-under-test toolchains and deterministic race helpers in the test image
status: In Progress
assignee: []
created_date: '2026-10-16 22:39'
updated_date: '2026-10-17 18:01'
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
- [x] #1 A smoke build per toolchain runs first and fails as a toolchain problem, not a race failure
- [x] #2 Each helper's pause/resume is verified by sampling /proc/locks in a container test
- [x] #3 No network access is needed by the fixtures (CARGO_NET_OFFLINE=true)
- [x] #4 A traced smoke cargo build per toolchain stays within a stated bound under strace --seccomp-bpf: tracee ptrace stops counted by voluntary_ctxt_switches as in strace_seccomp_bpf_churn_within_bound, plus a generous wall-time bound (carried from task-59 AC#2)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: Pins at scaffold time: rust-overlay ec07af4 (2026-10-16) has stable manifests up to 1.99.1. The product toolchain is set by rust-toolchain.toml only. The Cargo-under-test toolchains are separate: pick them from the same locked rust-overlay input (rust-bin.stable."X.Y.Z" / rust-bin.nightly."YYYY-MM-DD") so they are pinned by flake.lock.

Forward-carried from task-2: (1) Extension point: e2eBaseImage.contents in flake.nix (marked with a task-4 comment). Toolchains are base-image inputs: changing them changes the image tag (its Nix hash), and the runner reloads the image once (podman load of an 80 MB base took about 2 s). (2) Each test gets a fresh tmpfs at /tmp (nosuid,nodev; exec allowed) and runs with --network=none, so fixtures must be offline. Fixture project files that are not .rs/.toml must be added to the flake source filter. (3) Helpers that must hold locks while the code under test runs need a spawn (non-blocking) variant next to under_test::run_as_tester, which blocks until exit. Root-owned helpers are plain std::process::Command children of the harness. /proc/locks sampling from container root works. (4) The harness's records tmpfs (/run/diskgc-e2e) is mode 0700 root, so control files that a uid-1000 process must see belong on the scratch mount.

Forward-carried from task-59: (1) task-59 AC#2 ('a traced cargo build stays within a stated time bound') could not be met without Cargo in the image; it is the new last AC here. Pattern: suite.rs strace_seccomp_bpf_churn_within_bound. Wall time on this shared host varies several-fold (0.66-4.96 s for the same churn), so bound the tracee's own voluntary_ctxt_switches (one per ptrace stop, deterministic: 2 per traced call) and keep wall time as a generous budget only. (2) The snapshot skips read-only mounts but walks the overlay root. Toolchains baked into the image's contents land on the overlay root and would be walked by every snapshot (slow, and keyed without btime). Mount them read-only instead, e.g. a store path bind-mounted :ro like the under-test dir, or a ro bind inside the container, so plan_walk skips them. The log line 'mounts not walked: ...' shows whether it worked. (3) /home/tester is now a fresh tmpfs per test: anything under it (CARGO_HOME, ~/.cargo/config.toml, an offline registry) must be provisioned per test through ctx.home (Scratch), never baked into the image (it would be shadowed). (4) Every traced run of code under test must be judged by an Oracle armed before it, or the test fails (Ctx::teardown). Helpers that are fixtures, not code under test, run untraced via Command. (5) Scratch::mount now takes an owner.

Orchestrator: task-59 stays In Progress until this task demonstrates the traced cargo build within a stated time bound (task-59 AC#2); close task-59 when that AC here passes.

Implementation (task-4 implementer, not committed; staged). Files: flake.nix (cargoUnderTest list, image /opt/cargo-under-test + manifest, /srv/cargo-smoke mount point), diskgc-e2e/src/{cargo_under_test,cargo_suite,cargo_probe,proc_locks,holder}.rs (new; holder.rs is inuse_suite's Holder moved), suite.rs (per-toolchain listing NAME@TOOLCHAIN, smoke prelude, TOOLCHAIN result, read-only bind of /nix/store), runner.rs (TOOLCHAIN label and summary, container names), under_test.rs (TESTER_ENV, *_with_env variants), probe.rs (dispatch), lib.rs (exit::TOOLCHAIN = 3), diskgc-e2e/fixtures/cargo/{smoke,race}, docs/testing.md (new section "Cargo toolchains under test", Container setup, Commands), docs/safety.md (Lock files: measured 1.99.1 and nightly facts), docs/architecture.md (points at the list).

Toolchains (rust-overlay ec07af4, minimal profiles, plus nixpkgs stdenv.cc as linker): 1.90.0 (oldest stable the docs name; .cargo-lock only), 1.99.1 (newest stable in the lock), nightly-2026-10-16 (newest dated nightly, 1.101.0). Decision: the docs did not fix the range; the PRD's "1.96 nightly" is older than 1.99.1 stable, so the nightly slot goes to the newest nightly for early warning. Image: 80 MB -> 2.84 GB; podman load about 30 s, once per tag change.

Evidence per AC (tests; tier fast on 1.90.0, full on the others):
#1 Smoke runs before every Cargo test's body (suite::Resolved::run); a failure is recorded in Ctx and reported as TOOLCHAIN (exit 3), counted apart by the runner. Tests: cargo_smoke@*, cargo_smoke_reports_a_broken_toolchain_as_a_toolchain_failure (missing cargo -> step "cargo --version"; missing linker -> step "cargo build"; real toolchain passes), unit suite::tests::a_broken_toolchain_is_reported_as_such. Bite: toolchain bin path mutated to NAME-broken -> cargo_smoke@1.90.0 and cargo_lock_holder...@1.90.0 both reported "TOOLCHAIN ... not run, its toolchain is broken: ... failed its smoke check at cargo --version", runner summary "1 test(s) not run because their Cargo toolchain under test failed its smoke check".
#2 /proc/locks: cargo_lock_holder_blocks_a_build_on_cargo_lock@* (build waits for .cargo-lock; holds .cargo-build-lock where present; stays blocked; finishes after release), cargo_build_script_gate_pauses_a_build_holding_its_locks@* (build script is cargo's child; cargo holds every lock file of the profile, waits for none), cargo_runner_gate_pauses_between_compile_and_run_holding_no_lock@* (run and test; nobody holds any lock file; binary runs after the gate). Bites: holder without flock -> "the lock holder (pid 203) holds [], ... expected it to hold .cargo-lock exclusively"; build.rs not waiting -> "the build script ... is gone"; runner not waiting -> "no helper reached gate".
#3 cargo_fixtures_build_without_network@*: only lo in /sys/class/net; smoke and race fixtures build offline; a registry control fails with "offline mode" and no network attempt. Unit: fixture_manifests_declare_no_dependencies, cargo_runs_offline_with_the_toolchain_and_linker_first_on_path. Bites: serde dependency added to the race fixture -> race build exits 101; CARGO_NET_OFFLINE removed -> "the registry control tried the network (["Updating crates.io index", "Could not resolve", "spurious network error"])".
#4 cargo_traced_build_within_bound@*: traced build (harness strace, --seccomp-bpf) <= 60% of the voluntary context switches of the same build under the same strace without --seccomp-bpf, measured in the same container; tracee seccomp filters = harness + 1; wall <= 120 s; oracle clean. Measured: 1.90.0 ~7,400 of ~20,200; 1.99.1 ~10,000 of ~23,200; nightly ~10,300 of ~23,400; untraced 450-750; 24 traced calls. Bite: --seccomp-bpf removed from STRACE_OPTIONS -> "stopped about 20205 times, over the bound 12124" (1.90.0) and 23689 > 14254 (1.99.1); the wall bound alone would not have bitten (0.2 s).

Findings:
(1) strace 7.2 restarts every new process/thread with PTRACE_SYSCALL until its first traced syscall (strace.c: "We don't know if forks/clones have a seccomp filter when they are created"), so --seccomp-bpf leaves ~1/3 of a full trace's stops on a Cargo build; a per-traced-call bound like the churn test's cannot hold. Documented in testing.md.
(2) Cargo 1.99.1 and nightly 1.101 create .cargo-artifact-lock with a plain build and, during a build, hold .cargo-build-lock and .cargo-artifact-lock exclusively and .cargo-lock SHARED; with .cargo-lock held by another process they wait holding .cargo-build-lock. Added to safety.md Lock files. The PRD's "Verified for Cargo 1.90 and a 1.96 nightly" line is left for the human/orchestrator.
(3) cargo run execs the runner in its own process (same pid, the wrapper's exe); cargo test spawns it as a child.
(4) /proc/locks read in several read() calls repeated lines under a parallel run (seq_file restarts by index into the host-wide list); the sampler now reads in one read() and retries/fails otherwise.

Gates (exit codes): just check 0 (275 workspace tests); nix build (then --rebuild) of workspace-tests 0/0 (275), workspace-clippy 0/0, e2e-tests 0/0 (65), e2e-clippy 0/0 (plain build first: --rebuild refuses outputs never built or GC'd); nix flake check -L 0; just e2e 0 twice (52/52, 5.6 s and 6.2 s); just e2e-full 0 (67/67, 9.7 s).

Limits: the image is 2.84 GB (old base tags are not pruned; task-60). Gates are polled files (10 ms). The runner wrapper is diskgc-e2e, so during a `cargo run` pause the paused process is not named cargo (see task-11 note). Helpers time out after 300 s; harness waits 120 s.

Review round (mped GO, conditional S1-S2; S3-S5 done too): S1 measured facts added to docs/safety.md and reproduced on the image: nightly-2026-10-16's default new build-dir layout (no .fingerprint/ or deps/) and the split lock set with CARGO_BUILD_BUILD_DIR (1.90.0 ignores it), as Known gaps under Discovery; forwarded to task-53, task-9, task-11. S2 PRD 'Cargo lock coverage' rewritten to point at safety.md and testing.md, plus the older-than-1.90 statement (also in testing.md). S3 the tier is an explicit attribute of each cargoUnderTest entry (manifest NAME VERSION TIER; a manifest without a fast toolchain fails the listing); fast is now 1.99.1, so the notes above saying 'fast on 1.90.0' are superseded. The lock-holder test logs its .cargo-build-lock check as not applicable on 1.90.0 instead of passing silently. S4 fixture comments name SMOKE.output / RACE.output; testing.md keeps percentages only; the safety.md no-lock-while-running sentence is split by what was measured. S5 task-11 note (7) replaced with mped's guidance (no cargo-* wrapper copy; assert cargo test on the existing heuristic; CARGO_MANIFEST_DIR rule for cargo run via task-11 AC #3), plus a correction of the task-59 seccomp note there. H1-H6 filed in task-60.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
