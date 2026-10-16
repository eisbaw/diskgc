---
id: TASK-37
title: >-
  Command actions: argv, {path}, cwd, environment, timeouts, process groups,
  root refusal
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:51'
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

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
