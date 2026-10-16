---
id: TASK-3
title: >-
  Host port and LinuxHost primitives: mounts, openat2 resolution, statx,
  statvfs, flock, /proc readers
status: To Do
assignee: []
created_date: '2026-10-16 22:39'
updated_date: '2026-10-16 23:12'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: (1) The flake's checks build and run tests for x86_64-unknown-linux-musl (static). The dev shell's just check runs them on gnu. Syscall code is therefore exercised on both libcs. Use rustix with its default linux_raw backend for openat2/statx/statvfs/flock (no libc wrappers). That works the same under static musl. (2) No musl C cross toolchain is configured in the crane derivations. Avoid deps with C code (cc/bindgen). If one is unavoidable, the flake needs a musl stdenv (pkgsCross.musl64 or pkgsStatic). (3) Source filtering: only .rs/.toml under crates/ plus the root Cargo.toml/Cargo.lock are build inputs. Test fixtures in other formats (e.g. .yaml, mountinfo samples) must be added to workspaceFiles in flake.nix, or they are missing in nix flake check while working in the dev shell.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
