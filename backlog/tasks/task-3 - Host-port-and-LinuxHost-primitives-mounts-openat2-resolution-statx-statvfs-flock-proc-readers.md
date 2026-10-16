---
id: TASK-3
title: >-
  Host port and LinuxHost primitives: mounts, openat2 resolution, statx,
  statvfs, flock, /proc readers
status: To Do
assignee: []
created_date: '2026-10-16 22:39'
updated_date: '2026-10-16 22:49'
labels:
  - wave-1
  - irreversible
  - safety
dependencies:
  - TASK-1
  - TASK-2
references:
  - docs/safety.md
  - docs/architecture.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: all safety logic runs over these primitives; bugs here delete the wrong thing. Define the Host trait in diskgc-core (syscall-shaped file/lock ops, time and blocking ops as port operations) and implement LinuxHost: mountinfo parsing and mount-root resolution with openat2(BENEATH|NO_SYMLINKS|NO_XDEV), statx with attribute-mask checks, statvfs (f_bavail * f_frsize), flock with release only by close, /proc fd/cwd/root/maps/environ readers. Minimum kernel 5.8 check. See docs/safety.md#path-traversal and docs/architecture.md#the-host-port.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A symlink anywhere in a configured path yields unsafe_path; a bind mount of the same filesystem below a root is refused (container tests)
- [ ] #2 statx attributes are used only when their mask bit is set; an unanswerable check yields unsupported_fs
- [ ] #3 Kernel older than 5.8 (simulated) exits 2 at startup
- [ ] #4 openat2 and statx are called as raw syscalls via rustix (no libc wrapper dependency, works with static musl)
- [ ] #5 flock (release only by close) and the /proc readers have container tests
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
