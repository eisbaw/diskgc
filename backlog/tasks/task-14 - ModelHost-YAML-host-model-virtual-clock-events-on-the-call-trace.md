---
id: TASK-14
title: 'ModelHost: YAML host model, virtual clock, events on the call trace'
status: In Progress
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 08:42'
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
- [x] #1 The sketch in docs/testing.md loads and runs as a passing model test
- [x] #2 An openat2 without the required flags fails the test, and a lock lives exactly as long as some fd refers to its open file description (a ModelFd, or a modelled process's fd), released by nothing else. (Reworded from 'a lock released with LOCK_UN fails the test': since task-3 the port has no unlock and Locked cannot be re-locked, so LOCK_UN cannot be written; the model checks the lifetime instead.)
- [x] #3 Unknown keys and dangling pid references are load errors
- [x] #4 An unlinkat without a preceding identity check fails the test
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: the flake's source filter is an allowlist (workspaceFiles in flake.nix): root Cargo.toml/Cargo.lock plus .rs/.toml files under crates/. Embedded or test YAML (include_str! built-in profiles, host models, golden sets, insta snapshots) is NOT a build input until added, e.g. lib.fileset.fileFilter (f: f.hasExt "yaml") ./crates. Without that, the dev shell works but nix build / nix flake check fail with a missing-file error. Add it in the same change that adds the first such file.

Forward-carried from task-3: (1) The port is diskgc_core::host::Host (associated type Fd; dropping it closes). There is NO unlock operation, so 'a lock released with LOCK_UN fails the test' (AC #2) cannot be expressed. Check instead that a lock lives exactly as long as its Fd, including fds inherited by children (architecture.md updated). The openat2 RESOLVE_* check stays meaningful: OpenHow carries ResolveFlags{beneath, no_symlinks, no_xdev}. (2) Stat fields are Option. Model 'kernel capabilities (statx masks)' by returning None, which makes resolve give unsupported_fs. (3) Call order of resolve(): mountinfo; openat2(Base::Root, mount_point, mount_point_how()); statx(fd, ''); openat2(Base::Dir, rest or '.', beneath_how()); statx(fd, ''). resolve.rs tests use a small scripted fake (Scripted) that ModelHost may replace. (4) flock_wait(deadline), now() and sleep() are port ops for the virtual clock. LinuxHost polls flock every 10-100 ms.

Forward-carried from task-3 (update): lock calls now take the Fd by value. flock_try returns TryLock<F>::{Acquired(Locked<F>), Busy(F)}; flock_wait returns WaitLock<F>::{Acquired(Locked<F>), DeadlinePassed(F)}. Build Locked with Locked::granted(fd, mode) (for Host implementations only). Re-locking or unlocking a held lock cannot be written (compile_fail doctest on Locked), so the model needs no LOCK_UN check; model the lock as living until the Locked is dropped. proc_link became proc_links -> Vec<LinkTarget> (per task).

Implementation (task-14):
- diskgc-model: spec.rs (format types, serde, deny_unknown_fields everywhere, JSON Schema via schemars, committed as crates/diskgc-model/host-model.schema.json and kept current by loading.rs the_committed_json_schema_is_current; regenerate with DISKGC_UPDATE_SCHEMA=1 cargo test -p diskgc-model --test loading), load.rs (strict loading, references resolved), state.rs (inode graph per device, mounts attached to the directory they cover, open file descriptions carrying flocks, processes; space, nlink, mount points and lock holders derived), host.rs (ModelHost: Host + Delete, trace, events, call checks, virtual clock), harness.rs (load fixture, run scenario, check expect, fail on unfired events). diskgc-modelgen: proptest strategies (name, file, tree, process, host_model) and with_event_at_call.
- YAML library: serde-saphyr 2 (maintained, pure Rust, panic-free parser, errors carry line/column and a snippet). serde_yaml is deprecated. Tree nodes go through serde_json::Value to tell files (mapping of scalars) from directories (mapping of mappings).
- Port change (minimal, needed by AC #4): diskgc_core::host::Delete: Host with unlinkat(dir, name, Unlink::{NonDir, Dir}); errno consts EBUSY, EEXIST, EISDIR, EROFS, ENOTEMPTY, ENXIO. LinuxHost does not implement Delete yet (task-5, behind the task-50 gate). docs/architecture.md (Host port) updated.
- Sketch (AC #1): docs/testing.md sketch adapted minimally: the process's exe path is added to the tree (strict loading refuses a dangling path). Run by tests/sketch.rs through a stand-in for the Cargo lock check (resolve + open_beneath + flock_try), until the executor exists.
- Tests: AC #1 sketch.rs the_testing_md_sketch_loads_and_runs. AC #2 checks.rs openat2_below_a_directory_without_a_required_flag_fails_the_test, openat2_of_a_mount_point_without_a_required_flag_fails_the_test, the_cores_resolution_passes_the_call_checks; lock lifetime: a_lock_lives_exactly_as_long_as_its_fd, a_process_lock_lives_as_long_as_the_process_holds_its_fd, a_process_lock_ends_when_the_process_exits. AC #3 loading.rs unknown_keys_are_load_errors_at_every_level, dangling_pid_references_are_load_errors (plus dangling paths/mounts, contradictions). AC #4 checks.rs unlinkat_without_a_preceding_identity_check_fails_the_test, an_identity_check_of_another_name_does_not_count, an_identity_check_covers_one_unlinkat.
- Bites (mutate, run, restore from a backup copy, git diff --quiet verified): lock conflicts ignored -> sketch fails; check_openat2 made a no-op -> both openat2 tests fail; RESOLVE_NO_XDEV requirement dropped -> below-directory test fails; ModelFd drop not closing -> a_lock_lives_exactly_as_long_as_its_fd fails; open file descriptions never released -> 3 lock tests fail (a_process_lock_ends_when_the_process_exits first survived: it only checked locks_on; strengthened to take the lock, then it bit); deny_unknown_fields removed from HostModel or FileSpec -> unknown-keys test fails; event pid check removed -> dangling-pid test fails (the model loaded); lock holder check removed -> dangling-pid test fails on the message only (proc_attach still refuses the load: a second line); identity check removed -> 3 identity tests fail.
- Limits: see docs/testing.md (Model tests, Limits), the single home.

Review round 1 (mped NO-GO, one blocker) fixed:
- BLOCKING: the identity check now requires the previous port call to be a successful statx of (directory inode, name), derived from the trace (Call.entry); the identity_checked set is gone. New tests checks.rs an_identity_check_from_an_earlier_walk_does_not_count, rmdir_after_descending_needs_a_fresh_identity_check (both failed on the old code). Bites: 'any earlier statx counts' -> 3 tests fail (both new ones plus an_identity_check_covers_one_unlinkat); check removed -> 5 fail.
- remove is rm -r: every entry below is unlinked, an open object keeps only itself. Test behaviour.rs remove_of_a_held_directory_empties_it_like_rm_r (failed on the old code; bite: skip the clearing -> fails).
- An empty fd table, maps or environment, or no exe, of a readable process is ProcError::Failed, as on LinuxHost. Test behaviour.rs an_empty_proc_answer_is_failed_as_linuxhost_reports_it (failed on the old code; bite: never empty -> fails). Generated processes run and map /usr/bin/tool with PATH set; the sketch's pid 4242 maps its exe and has an environment.
- Bytes schema description fixed and schema regenerated; testing.md Limits completed; architecture.md names the statx-from-root check.
- Deferred items filed on task-61; mped's checked_unlink idea on task-5; distinct gate error on task-50.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
