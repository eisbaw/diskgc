---
id: TASK-59
title: >-
  Harness follow-ups before toolchains and race tests: mandatory oracle,
  seccomp-bpf strace, tmpfs for /run/diskgc and home
status: Done
assignee:
  - '@mped-architect'
created_date: '2026-10-16 23:54'
updated_date: '2026-10-17 18:23'
labels:
  - wave-1
  - testing
dependencies:
  - TASK-2
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: findings from the task-2 review that would make later e2e results untrustworthy or too slow once Cargo builds run in the containers. (1) The oracle is opt-in: a traced run of diskgc that never calls Oracle::arm/judge passes silently; Ctx must fail any such run. (2) strace without --seccomp-bpf stops the traced process on every syscall; traced Cargo builds will blow the budget; use --seccomp-bpf. (3) Objects the harness creates outside a tmpfs live in the overlay upper layer where inode numbers are reused and birth time is unusable; mount tmpfs via Scratch on /run/diskgc (provision the lock there, as on a real host where /run is a tmpfs) and on /home/tester. (4) BTIME_UNSTABLE_FS_TYPES lists only overlay; detect fuse-overlayfs (podman info) and fail fast instead of producing false REPLACED. (5) Log strace removals that match no snapshot path (symlinked prefixes, .., '(deleted)' suffixes) instead of dropping them. (6) Skip read-only mounts (e.g. toolchains) in the whole-filesystem snapshot to keep it cheap.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A traced diskgc run without an oracle judgement fails the test (proved once)
- [x] #2 strace runs with --seccomp-bpf; a traced cargo build in a container stays within a stated time bound
- [x] #3 /run/diskgc and /home/tester are tmpfs in every container; the lock file is provisioned there
- [x] #4 A non-native overlay storage driver makes the runner fail fast with a clear message
- [x] #5 Unmatched strace removals are logged; read-only mounts are skipped by the snapshot
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Mandatory oracle: Ctx owns a ledger of traced runs; under_test::run_as_tester requires it, every Run carries a judged flag set by Oracle::judge*; judge refuses runs that started before the oracle was armed; suite::run fails a test that returned Ok with unjudged runs. Verdict is #[must_use]. Bite proof: a registry test marked must-fail (suite::Expect) that runs diskgc without judging it; the in-container suite::run passes it only when it fails with the ledger message. Existing tests that ran probes without an oracle now judge them.
2. strace --seccomp-bpf in under_test; detect a refused filter from strace stderr and fail. New fast test strace_overhead_churn: tester creates, stats and deletes 20k files under strace; bound measured with and without seccomp-bpf, recorded in the test and docs; the record must hold all 20k unlinks and the oracle judges the run.
3. Ctx::setup mounts tmpfs via Scratch on /run/diskgc (0755 root, lock 0644 root provisioned through Scratch) and /home/tester (0755 tester, .profile provisioned); Scratch::mount gains an owner. Image keeps the mount points only; the overlay fixture of the oracle test moves to a tester-owned file baked into the image outside both.
4. Runner: podman info (GraphDriverName, GraphOptions); pure check function fails fast unless native overlay without mount_program; in-container Ctx::setup also requires the root mount's fs type to be overlay. Unit tests with fuse-overlayfs/vfs samples; manual bite with a scratch storage.conf if possible.
5. Oracle classifies traced removals that match no before-snapshot path: non-canonical (relative, . or .., ' (deleted)'), through a symlink in the snapshots, or created during the run; suspicious ones logged one by one (capped), created-during-run as a count. Container test with symlinked-prefix and .. removals; unit tests incl. (deleted).
6. Snapshot skips read-only mounts (per-mount or superblock ro) like pseudo ones, but still walks writable mounts nested below them; logged. Unit tests on mountinfo samples.
7. docs/testing.md updated (Container setup, Grounding); gates; notes and forward-carries (task-4, 51, 3, 5, 6, 11).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Progress (task-59 implementer, not committed; changes staged): all six items implemented. Files: diskgc-e2e/src/{under_test,oracle,snapshot,scratch,suite,probe,runner}.rs, flake.nix (image: host lock and tester home no longer baked; /srv/tester/on-overlay added as the overlay-root fixture), docs/testing.md (Grounding, Container setup, Commands requirements).

Evidence per AC:
#1 Mandatory oracle: under_test::Ledger records every traced run (run_as_tester requires it); Oracle::judge_runs marks runs judged and refuses runs that started before the oracle was armed; Ctx::teardown fails a test that returned Ok with unjudged runs; Verdict is #[must_use]. Bite proof runs in every just e2e: oracle_mandatory_unjudged_run_fails_the_test is registered with Expect::FailWith and passes only because teardown fails it ('1 traced run(s) of code under test never judged by the deletion oracle ... [[/opt/diskgc-e2e/bin/diskgc, --version]]'); on the way a late-armed oracle refused the run. If teardown stopped biting, that test would report 'passed: the rule it proves does not bite'. tester_cannot_read_other_users_mode000_dir previously ran 4 probes unjudged; now judged.
#2 strace --seccomp-bpf (STRACE_OPTIONS); a 'strace: ...seccomp...' line on stderr fails the run. New fast test strace_seccomp_bpf_churn_within_bound: tester creates 20,000 files, lstat x10 each, lists, unlinks all (about 14 syscalls per file, 1 traced). Stated bounds: tracee Seccomp_filters == harness + 1; tracee voluntary context switches <= 4 per file (80,000; measured 40,010 every run = 2 stops per traced unlink); wall time <= 10 s (measured 0.66-4.96 s on this loaded host, load average ~12 on 14 cores); record holds all 20,000 unlinks; oracle clean. Bite, with --seccomp-bpf removed temporarily: 600,885 switches, 17.17 s, filter count 1 == 1, all three checks failed (earlier runs without it: 2.9-5.8 s, so wall time alone would not have bitten; the switch count is the load-independent discriminator). NOT DONE: a traced cargo build, since there is no Cargo in the image until task-4. Forward-carried to task-4 as an AC. AC#2 stays unchecked until then or until the orchestrator re-scopes it.
#3 Ctx::setup mounts tmpfs via Scratch on /run/diskgc (0755 root; lock provisioned via Scratch::write_file, 0644 root) and /home/tester (0755 tester, uid/gid mount options; .profile provisioned), in every container. sandbox_trivial checks modes/owners, distinct mount ids, and that /run/diskgc, /run/diskgc/lock, /home/tester, /home/tester/.profile are keyed with birth time. oracle_catches_deletion_outside_eligible_set now flags /tmp/keep/file and /home/tester/.profile (btime key) plus /srv/tester/on-overlay (fallback key), checked per entry.
#4 Runner: podman info GraphDriverName + GraphOptions; check_storage_driver (unit-tested with fuse-overlayfs, vfs, btrfs, zfs samples) fails unless overlay without mount_program. In-container second check: Ctx::setup requires the root mount's fs type to be 'overlay'. Bite on this host with an isolated storage.conf (separate graphroot/runroot in the scratchpad, mount_program = nixpkgs fuse-overlayfs 1.17): the runner stopped after the canary with 'podman storage driver is overlay through a mount program (fuse-overlayfs?) ...', exit 1; a test container run directly on that store failed 'the container's root filesystem is "fuse.fuse-overlayfs", not native overlayfs'. The store was removed afterwards.
#5 Unmatched: oracle::unmatched_removals classifies took-effect traced paths missing from the before snapshot: CreatedDuringRun (counted, 3 examples), NonCanonical (relative, ., .., //, trailing /, checked on bytes because Path::components hides . and //), DeletedDirectory (a ' (deleted)' dir component), SymlinkPrefix(link), NoKnownParent; suspicious ones logged one per line (cap 50 + count). Container test oracle_logs_unmatched_strace_removals: removal via /tmp/link/a, /tmp/real/../real/b and /tmp/keep-link/x; all three logged with reasons; the last (not eligible) is caught by the snapshot only. Read-only mounts: snapshot::plan_walk skips pseudo and ro (per-mount or superblock) mounts by statx mount id and walks writable mounts nested below a skipped ro mount from their own mount points; mounts below a pseudo fs stay unwalked. Unit tests on mountinfo samples (incl. stacked/nested/cyclic); container test snapshot_skips_read_only_mounts (/opt/diskgc-e2e/bin and a ro bind in scratch skipped, the same objects visible via the rw source, nested tmpfs below the ro bind walked). Effect: before-snapshot logs 'mounts not walked: ... /opt/diskgc-e2e/bin (read-only)'.

Gates (this machine, exit codes): nix flake check -L exit 0 (21 s; e2e unit tests 34 passed); nix develop -c just check exit 0 (9 passed, 3.9 s); nix develop -c just e2e exit 0 (11/11, container phase 4.3 s, 12.2 s total, base image reused); nix develop -c just e2e-full exit 0 (12/12, 8.3 s; oracle_large_tree 8.0 s with 102,529-object snapshots in about 1.2 s each).

Gotchas:
(1) strace --seccomp-bpf requires -f; without it strace prints 'strace: --seccomp-bpf cannot be used without -f/--follow-forks, disabling' (even with -qq) and carries on slowly. The harness now fails on such a line.
(2) Wall time is not a reliable discriminator on a shared host: the same traced churn ranged 0.66-4.96 s with the filter and 2.9-17 s without. The tracee's voluntary_ctxt_switches counts ptrace stops deterministically (40,010 for 20,000 traced unlinks).
(3) std::path::Path::components() drops '.' and collapses '//' and a trailing '/'; lexical checks for non-canonical paths must look at the bytes.
(4) mountinfo is not ordered parent-before-child, and the same mount point can be stacked (podman's /sys/devices/virtual/powercap): decide skip by statx mount id, never by path.
(5) A nested writable mount below a read-only one is reachable only from its own mount point once the ro mount is skipped; plan_walk adds those as extra roots.
(6) A mount program shows up in podman info only as GraphOptions.overlay.mount_program; GraphDriverName stays 'overlay'.
(7) The base image tag changed (host lock and home no longer baked); the old tag was removed by hand with podman rmi (pruning is task-60 item 3).

Honest limits: the ledger only covers runs through under_test::run_as_tester with the Ctx ledger; a test could construct its own Ledger or spawn code under test with std::process::Command directly and bypass it (review discipline; task-60's disallowed-methods idea could cover Command too). Suspicious unmatched removals are logged, not failed; the snapshot judges the objects. The ro-mount skip assumes nothing writes to a ro-mounted filesystem's objects that are not also reachable through a walked rw path (documented in docs/testing.md).

Review fix (coordinator, GO with one fix): judge_expectation no longer lets a FailWith bite proof pass when the test body itself failed. Before, teardown appended the unjudged message to the body's error, the expected text still matched, and the body's own checks were masked. Now suite::run passes the body result and the rules result (Ctx::teardown(), the ledger check only) separately. FailWith passes only if the body returned Ok and the rules failed with the expected text; a failing body reports 'its body failed, so the rule was not what failed it' plus the rule message. Unit test expectations_pass_and_fail_as_declared covers all eight combinations. Bite proof: temporarily changed the bite test's run.expect_code(0) to expect_code(3). The test then FAILed: 'bite proof for "never judged by the deletion oracle": its body failed ... exited with Some(0), expected 3 (also: 1 traced run(s) ... never judged ...)'. Reverted. Gates after the fix: just check exit 0 (9 passed); just e2e exit 0 (11/11, 1.4 s container phase); just e2e-full exit 0 (12/12, 3.6 s). Hardening follow-ups from the review (wall-time flake risk, let_underscore_must_use, CreatedDuringRun is inferred) went to task-60.

Orchestrator gate: qa-test-runner re-ran after the self-test fix: nix flake check exit 0; just check 9/9; just e2e 11/11; just e2e-full 12/12; diskgc-e2e unit tests 34/34 incl. expectations_pass_and_fail_as_declared. mped-architect: GO. Committed with the code; task stays In Progress until AC#2's traced-cargo-build half is demonstrated in task-4.

From task-4: AC #2's traced-cargo-build half is demonstrated by the container test cargo_traced_build_within_bound (fast on 1.90.0, full on 1.99.1 and nightly-2026-10-16; diskgc-e2e/src/cargo_suite.rs). Stated bound (docs/testing.md, Cargo toolchains under test): a Cargo build traced by the harness's strace (--seccomp-bpf) costs at most 60% of the voluntary context switches of the same build under the same strace without --seccomp-bpf, measured in the same container; tracee seccomp filters = harness + 1; wall time <= 120 s; oracle clean. Measured ~7,400 of ~20,200 (1.90.0), ~10,000 of ~23,200 (1.99.1). Bite: --seccomp-bpf removed -> 20,205 > 12,124 and the test fails. Not per traced call as the churn test: strace 7.2 steps every new process/thread with PTRACE_SYSCALL until its first traced syscall, so about a third of the full trace's stops remain on a multi-process build.

CORRECTION (task-4 final gate): the tiers changed after the note above. cargo_traced_build_within_bound runs in the fast tier on 1.99.1 and in the full tier on 1.90.0 and nightly-2026-10-16 (cargoUnderTest's explicit tier attribute in flake.nix).

AC #2 demonstrated by task-4's cargo_traced_build_within_bound (committed 5092d1c); task-59 closed.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
