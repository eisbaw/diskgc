---
id: TASK-10
title: 'Minimal ensure: thresholds, host lock, exit codes, --select/--roots for Cargo'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 01:15'
labels:
  - wave-1
  - cli
  - irreversible
dependencies:
  - TASK-53
  - TASK-7
  - TASK-50
references:
  - docs/reference.md
  - docs/safety.md#host-lock
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the race tests and the first journey need a runnable binary. Build this as the first slice of the real executor in diskgc-core (later tasks extend it, nothing is thrown away): ensure --min-free/--target-free PATH with --select and --roots, config lookup with minimal version: 1 loading (with --select a config is optional and built-ins are used), size and duration parsing, statvfs measurement, the provisioned host lock with inheritance and close-only release, refusal to run as root without --allow-root, the sandbox gate, and the JSON fields that exist at this stage exactly as specified in docs/reference.md.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Already above --min-free: exit 0 without taking the lock
- [ ] #2 Missing /run/diskgc/lock: exit 2 printing the two tmpfiles lines
- [ ] #3 Exit codes 0, 1 and 2 are produced by container tests (75, 124, 130 and 143 are model-tested in the Executor II task)
- [ ] #4 Running as uid 0 without --allow-root exits 2; bare numbers and month durations are usage errors
- [ ] #5 Every JSON field and enum value emitted matches docs/reference.md (schema and enum test; snapshots come at the contract freeze)
- [ ] #6 A reclaiming ensure outside a container exits 2 naming the sandbox gate (test run outside the harness)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-3: Host lock: reach it as resolve('/run/diskgc', Want::Directory), then open_beneath('lock', Access::Read, Want::Any), then flock_wait(fd, Exclusive, deadline). Release only by dropping the fd (the port has no unlock). To let children inherit it, clear FD_CLOEXEC on that one fd. That needs a port op; host_probe.rs does it with rustix::io::fcntl_setfd for the test. host_flock_release_only_by_close proves that the inherited copy keeps the lock after the parent closes. LinuxHost::flock_wait polls (10-100 ms, not FIFO; limits in safety.md Host lock).

Forward-carried from task-3 (update): the host lock is taken as host.flock_wait(fd, Exclusive, deadline), which consumes the fd and returns WaitLock::Acquired(Locked) or DeadlinePassed(fd). Hold the Locked for the whole reclaim and use locked.fd() to clear FD_CLOEXEC for children. Never flock it again: the port does not allow it.
<!-- SECTION:NOTES:END -->
