---
id: TASK-14
title: 'ModelHost: YAML host model, virtual clock, events on the call trace'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 01:15'
labels:
  - wave-1
  - testing
dependencies:
  - TASK-3
references:
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: most behaviour is tested in-process and deterministically through the Host port. Implement diskgc-model (inode graph, derived free space, strict loading with deny_unknown_fields and resolved references, JSON Schema, virtual clock, events addressed by Host call, call-flag validation such as missing RESOLVE_* flags or LOCK_UN) and diskgc-modelgen value generators.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The sketch in docs/testing.md loads and runs as a passing model test
- [ ] #2 An openat2 without the required flags, or a lock released with LOCK_UN, fails the test (proves the model checks how it is called)
- [ ] #3 Unknown keys and dangling pid references are load errors
- [ ] #4 An unlinkat without a preceding identity check fails the test
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: the flake's source filter is an allowlist (workspaceFiles in flake.nix): root Cargo.toml/Cargo.lock plus .rs/.toml files under crates/. Embedded or test YAML (include_str! built-in profiles, host models, golden sets, insta snapshots) is NOT a build input until added, e.g. lib.fileset.fileFilter (f: f.hasExt "yaml") ./crates. Without that, the dev shell works but nix build / nix flake check fail with a missing-file error. Add it in the same change that adds the first such file.

Forward-carried from task-3: (1) The port is diskgc_core::host::Host (associated type Fd; dropping it closes). There is NO unlock operation, so 'a lock released with LOCK_UN fails the test' (AC #2) cannot be expressed. Check instead that a lock lives exactly as long as its Fd, including fds inherited by children (architecture.md updated). The openat2 RESOLVE_* check stays meaningful: OpenHow carries ResolveFlags{beneath, no_symlinks, no_xdev}. (2) Stat fields are Option. Model 'kernel capabilities (statx masks)' by returning None, which makes resolve give unsupported_fs. (3) Call order of resolve(): mountinfo; openat2(Base::Root, mount_point, mount_point_how()); statx(fd, ''); openat2(Base::Dir, rest or '.', beneath_how()); statx(fd, ''). resolve.rs tests use a small scripted fake (Scripted) that ModelHost may replace. (4) flock_wait(deadline), now() and sleep() are port ops for the virtual clock. LinuxHost polls flock every 10-100 ms.

Forward-carried from task-3 (update): lock calls now take the Fd by value. flock_try returns TryLock<F>::{Acquired(Locked<F>), Busy(F)}; flock_wait returns WaitLock<F>::{Acquired(Locked<F>), DeadlinePassed(F)}. Build Locked with Locked::granted(fd, mode) (for Host implementations only). Re-locking or unlocking a held lock cannot be written (compile_fail doctest on Locked), so the model needs no LOCK_UN check; model the lock as living until the Locked is dropped. proc_link became proc_links -> Vec<LinkTarget> (per task).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
