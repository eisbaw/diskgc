---
id: TASK-3
title: >-
  Host port and LinuxHost primitives: mounts, openat2 resolution, statx,
  statvfs, flock, /proc readers
status: To Do
assignee: []
created_date: '2026-10-16 22:39'
updated_date: '2026-10-17 00:22'
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

Forward-carried from task-2: (1) Container tests: add a fn plus an entry (name, tier fast|full) to TESTS in diskgc-e2e/src/suite.rs. Build fixtures only through ctx.scratch (Scratch ops in diskgc-e2e/src/scratch.rs abort with exit 86 when a path is outside the /tmp scratch tmpfs, checked by statx mount id). Run code under test with ctx.run_as_tester (strace, uid 1000, no caps) and judge it with oracle::Oracle::arm(Fixture::new().eligible(..)) then judge(&run)?.assert_clean(). (2) Kernel facts seen in the container on this host (kernel 7.2): statx reports STATX_MNT_ID, STATX_ATTR_MOUNT_ROOT and btime on tmpfs. A bind mount of the scratch tmpfs gets a new mount id with the same device; interlock_aborts_outside_scratch proves it. The overlay root keeps inode numbers across copy-up but changes btime. (3) rustix 1.1.5 (linux_raw; features fs, mount, process, thread) builds and runs under static musl with no C toolchain. diskgc-e2e already locks it. When diskgc-linux gains rustix, refresh diskgc-e2e/Cargo.lock too (cargo update --manifest-path diskgc-e2e/Cargo.toml); the flake builds with --locked. (4) Container root has CAP_SYS_ADMIN and CAP_SYS_PTRACE inside the user namespace; the code under test has neither.

Forward-carried from task-59: container tests get tmpfs at /tmp, /run/diskgc (host lock 0644 root, provisioned per test) and /home/tester. Scratch::mount(root, mode, owner) mounts more tmpfs (nested mounts are fine), and Scratch::bind_mount_read_only makes a ro bind for mount-traversal fixtures. The snapshot skips read-only mounts by statx mount id but walks writable mounts nested below them (snapshot::plan_walk), so ro-bind fixtures do not blind the oracle where something can be deleted. The runner and each container require Podman's native overlay root (fuse-overlayfs is refused). Every traced run must be judged by the oracle (Ctx::teardown).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
