---
id: TASK-37
title: >-
  Command actions: argv, {path}, cwd, environment, timeouts, process groups,
  root refusal
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-17 10:59'
labels:
  - wave-4
  - irreversible
  - safety
dependencies:
  - TASK-34
references:
  - docs/safety.md#command-actions
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: special removal needs the tool's own cleaner, and command execution is the most dangerous surface. Implement command actions per docs/safety.md#command-actions and docs/reference.md#actions: preflight resolution of every executable, {path} substitution without a shell, fchdir working directory, environment variables, per-tier timeout with SIGTERM/SIGKILL of the process group, host-lock inheritance, refusal under --allow-root.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The candidate's safety checks run before the command (a test with an in-use candidate never runs the command)
- [ ] #2 A missing tool makes the profile missing, not an error
- [ ] #3 Non-zero exit is an error in the report and exit 3 when the goal is not met
- [ ] #4 The command's working directory is the checked candidate (same device and inode, asserted by the test command)
- [ ] #5 Killing diskgc while a command runs leaves the host lock held until the command exits
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-50 (sandbox gate): starting a prune, GC or command child is a deleting operation (docs/safety.md, Pre-release sandbox gate). Put it on diskgc_core::host::Delete, not Host. In LinuxHost (crates/diskgc-linux/src/host.rs), take a Permit from gate(op, subject) first and pass it to a new *_permitted wrapper that is the only place the child is started; the source scan deleting_syscalls_only_behind_the_gate forbids Command/spawn/fork/exec anywhere else in diskgc-linux. Extend its allowlist by that one wrapper, do not weaken it. A refusal is HostError with cause Cause::SandboxGate (kernel_errno() None): report it as an error, never as a skip or a retry. ModelHost implements the method without a gate.

CORRECTION from task-50's review (supersedes the earlier task-50 note): child processes are of two kinds (docs/architecture.md, The Host port): a query run on Host (ungated, argv from constants only) and a deleting run on Delete (gated). Command actions ALWAYS go through the deleting run on Delete, so they are refused outside a test container until v1, whatever the command does. Their preflight resolution of executables reads the filesystem and needs no child. The deleting run's LinuxHost wrapper takes a Permit from gate(op, subject) and is the one allowlisted Command/fork/exec site for it in the scan in crates/diskgc-linux/src/source_scan.rs (GATED_ALLOWED). A gate refusal is Cause::SandboxGate and is reported as an error of the action, never a skip.

From task-50 round 3 (mped): never drop Command from the scan's patterns to get a domain name through. The pattern is already the path process::Command (std's Command can only be named that way, since import renames and globs are refused), so an Action::Command variant in core passes. A control in source_scan.rs (command_include_and_syscall_match_only_their_real_forms) proves it. The deleting run's wrapper in diskgc-linux is the one GATED_ALLOWED entry for it, marked Gate::Permit.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
