---
id: TASK-54
title: >-
  Executor II: cooperative deadlines, overruns, signals, child process groups,
  re-sampling
status: Done
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-21 11:18'
labels:
  - wave-2
  - irreversible
  - safety
dependencies:
  - TASK-20
references:
  - docs/architecture.md#the-reclaim-walk
  - docs/safety.md#deadlines
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: split from the executor task; deadlines and signals decide what happens mid-delete. Implement cooperative deadlines with the overruns report, SIGINT/SIGTERM handling (no new candidate, child process group SIGTERM then SIGKILL after 10 s), host-lock inheritance by children with close-only release, and the up-to-2-s re-sampling after an action.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Exit codes 75, 124, 130 and 143 are produced deterministically in model tests with the virtual clock
- [x] #2 A blocking operation that exceeds the deadline is listed in overruns and no new work starts after it (model test)
- [x] #3 One container test: SIGTERM mid-reclaim exits 143 and leaves no half-processed candidate unreported
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-3: LinuxHost::flock_wait polls LOCK_NB with sleeps (10-100 ms) and handles no signals. The SIGTERM-interruptible host-lock wait (reference.md, watch) needs a signal flag checked between polls, or a port-level cancellation. Host::now() is CLOCK_BOOTTIME.

From task-14: ModelHost has a virtual clock (now starts at UPTIME_AT_START; sleep, flock_wait and advance events move it; timed events at {elapsed: d}). Signals are not modelled because the port has no signal operation yet; when this task adds one, add a deliver-signal event action in diskgc-model load.rs Action.

From task-50 (sandbox gate): starting a prune, GC or command child is a deleting operation (docs/safety.md, Pre-release sandbox gate). Put it on diskgc_core::host::Delete, not Host. In LinuxHost (crates/diskgc-linux/src/host.rs), take a Permit from gate(op, subject) first and pass it to a new *_permitted wrapper that is the only place the child is started; the source scan deleting_syscalls_only_behind_the_gate forbids Command/spawn/fork/exec anywhere else in diskgc-linux. Extend its allowlist by that one wrapper, do not weaken it. A refusal is HostError with cause Cause::SandboxGate (kernel_errno() None): report it as an error, never as a skip or a retry. ModelHost implements the method without a gate.

CORRECTION from task-50's review (supersedes the earlier task-50 note): process-group handling, the SIGTERM/SIGKILL sequence and child waits serve two run operations (docs/architecture.md, The Host port): a query run on Host (ungated, argv from constants) and a deleting run on Delete (gated; prune, GC, every command action). Starting either happens only in its one allowlisted LinuxHost wrapper (scan: crates/diskgc-linux/src/source_scan.rs). Signals and waits on an already started child are not gated.

From task-10: deadlines exist in the executor slice: Request.timeout (default 10m, --timeout 0 = none) becomes one deadline for host lock wait, discovery, tally and reclaim; LockError::DeadlinePassed re-samples (met -> 0, else 75); Stop::DeadlinePassed -> Status::Deadline (124) unless met. Model tests: the_host_lock_held_until_the_deadline_is_75_unless_space_appeared, the_deadline_during_the_reclaim_is_124 (crates/diskgc-model/tests/ensure.rs); e2e ensure_waits_for_a_held_host_lock_until_the_deadline. Missing here: signals (Status::Interrupted exists, never produced), overruns (always empty), the 2 s re-sampling window after an action, child process groups.

2026-10-21 TASK54 AC3 container evidence: added Fast sigterm_mid_reclaim_reports_partial_action_and_stops. The traced real diskgc command action deletes the first candidate file, writes its diskgc parent PID marker, and blocks; harness verifies /proc/PID/exe matches diskgc, sends SIGTERM to that PID, then checks exit 143, JSON interrupted, exactly one reported partial action, first candidate remainder intact, next candidate untouched, and clean deletion oracle armed before the run. nix develop -c just e2e-one sigterm_mid_reclaim_reports_partial_action_and_stops: PASS (599 ms; Nix image). Debian portability, full just e2e, parallel QA/MPED review, and DoD remain pending; no commit.

2026-10-21 implementation audit: overrun reporting now brackets tier discovery, per-candidate tally, candidate reclaim, and post-action statvfs; model tests cover discovery, tally, and reclaim clock jumps. Calls outside those brackets (request measurement/preflight, host-lock acquisition, protection scans, initial/final samples, and post-run in-use scan) are not yet individually reported as overruns. Keep TASK-54 open until full deadline coverage is established and a TERM-ignoring child process-group container test verifies the 10-second SIGKILL bound. The fast SIGTERM container test is being handled separately.

2026-10-21 TASK54 Full-tier stubborn-child evidence: sigterm_stubborn_command_child_gets_killed_after_grace runs a command shell with a TERM handler that writes a marker but continues looping. Harness verifies its parent is real diskgc, sends SIGTERM mid-reclaim, observes TERM marker and no EXIT trap marker, and enforces a broad 9-30 s grace window before completion. JSON is interrupted with exit 143 and one partial action, next candidate untouched; deletion oracle clean. nix develop -c just e2e-one sigterm_stubborn_command_child_gets_killed_after_grace: PASS (10.72 s; Nix Podman container). Full just e2e, parallel QA/MPED review and DoD remain pending; no commit.

2026-10-21 AC1/AC2 focused evidence: timeout clock starts at ensure or grouped-ensure executor entry, before sandbox gate; validation and closed-gate refusal retain exit 2 precedence. An open gate that crosses the deadline records sandbox_gate overrun, takes the required first space sample, then returns 0 if met or 124 if unmet; first-sample failure stays exit 2. Model tests exercise gate, preflight, measurement, host lock, protection, discovery, tally, reclaim, in-use scan and final statvfs overrun stages; 22 signals tests plus 57 ensure tests passed. SIGINT/SIGTERM map to JSON interrupted 130/143 and stop later candidate work. Post-action free-space re-sampling waits up to 2 s. Both deadline and signal child cancellation send process-group TERM, hold the leader waitable for the full 10 s, then send group KILL. Focused Nix container tests passed: SIGTERM partial action 10.37 s and timeout with TERM-ignoring descendant 10.58 s; prior stubborn-child Full-tier test passed 10.72 s before the full-grace adjustment. Timeout starts when the executor is entered; CLI parsing and policy loading precede it. Remaining gate: full just e2e, Debian portability, QA and MPED review, and DoD. No commit.

Final integrated gate passed: nix develop -c just e2e (158/158 Nix image, 7/7 native Docker); Full-tier sigterm_stubborn_command_child_gets_killed_after_grace passed separately 1/1 in a disposable container with clean deletion oracle (10.34 s). Existing short-drain E2E expectation was updated to the specified 10 s cancellation grace. Parallel qa-test-runner and mped-architect reviews found no blockers; Docker query process-group descendant-pipe regression also passed 1/1.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
