---
id: TASK-54
title: >-
  Executor II: cooperative deadlines, overruns, signals, child process groups,
  re-sampling
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-18 05:48'
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
- [ ] #1 Exit codes 75, 124, 130 and 143 are produced deterministically in model tests with the virtual clock
- [ ] #2 A blocking operation that exceeds the deadline is listed in overruns and no new work starts after it (model test)
- [ ] #3 One container test: SIGTERM mid-reclaim exits 143 and leaves no half-processed candidate unreported
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-3: LinuxHost::flock_wait polls LOCK_NB with sleeps (10-100 ms) and handles no signals. The SIGTERM-interruptible host-lock wait (reference.md, watch) needs a signal flag checked between polls, or a port-level cancellation. Host::now() is CLOCK_BOOTTIME.

From task-14: ModelHost has a virtual clock (now starts at UPTIME_AT_START; sleep, flock_wait and advance events move it; timed events at {elapsed: d}). Signals are not modelled because the port has no signal operation yet; when this task adds one, add a deliver-signal event action in diskgc-model load.rs Action.

From task-50 (sandbox gate): starting a prune, GC or command child is a deleting operation (docs/safety.md, Pre-release sandbox gate). Put it on diskgc_core::host::Delete, not Host. In LinuxHost (crates/diskgc-linux/src/host.rs), take a Permit from gate(op, subject) first and pass it to a new *_permitted wrapper that is the only place the child is started; the source scan deleting_syscalls_only_behind_the_gate forbids Command/spawn/fork/exec anywhere else in diskgc-linux. Extend its allowlist by that one wrapper, do not weaken it. A refusal is HostError with cause Cause::SandboxGate (kernel_errno() None): report it as an error, never as a skip or a retry. ModelHost implements the method without a gate.

CORRECTION from task-50's review (supersedes the earlier task-50 note): process-group handling, the SIGTERM/SIGKILL sequence and child waits serve two run operations (docs/architecture.md, The Host port): a query run on Host (ungated, argv from constants) and a deleting run on Delete (gated; prune, GC, every command action). Starting either happens only in its one allowlisted LinuxHost wrapper (scan: crates/diskgc-linux/src/source_scan.rs). Signals and waits on an already started child are not gated.

From task-10: deadlines exist in the executor slice: Request.timeout (default 10m, --timeout 0 = none) becomes one deadline for host lock wait, discovery, tally and reclaim; LockError::DeadlinePassed re-samples (met -> 0, else 75); Stop::DeadlinePassed -> Status::Deadline (124) unless met. Model tests: the_host_lock_held_until_the_deadline_is_75_unless_space_appeared, the_deadline_during_the_reclaim_is_124 (crates/diskgc-model/tests/ensure.rs); e2e ensure_waits_for_a_held_host_lock_until_the_deadline. Missing here: signals (Status::Interrupted exists, never produced), overruns (always empty), the 2 s re-sampling window after an action, child process groups.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
