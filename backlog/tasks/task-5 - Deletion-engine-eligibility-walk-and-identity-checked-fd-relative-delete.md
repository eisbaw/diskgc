---
id: TASK-5
title: 'Deletion engine: eligibility walk and identity-checked fd-relative delete'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 08:41'
labels:
  - wave-1
  - irreversible
  - safety
dependencies:
  - TASK-3
  - TASK-14
  - TASK-51
references:
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: this is the code that removes files. Implement the eligibility walk (ownership of every entry, .diskgc-keep anywhere, the Cargo structural recogniser, sockets/FIFOs, mounts) and the delete: reopen each directory relative to its parent and match (dev, inode), fstatat identity+owner check before every unlinkat, make_writable after the checks, ENOTEMPTY reported. See docs/safety.md#deletion.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A mixed-owner tree is skipped as not_owner; a nested .diskgc-keep or Cargo target makes the candidate kept / contains_special (container tests)
- [ ] #2 A 100k-entry, 30-level-deep tree is deleted without exceeding a low RLIMIT_NOFILE (e.g. 64)
- [ ] #3 The deletion oracle shows nothing outside the candidate disappeared in every engine test
- [ ] #4 An entry replaced between walk and unlink is never deleted and the candidate is changed (ModelHost event test, plus one container test using the pause wrapper)
- [ ] #5 A mode-000 directory inside a candidate makes it ineligible (fail closed), never treated as empty
- [ ] #6 A single directory with 200,000 entries and a tree deeper than PATH_MAX are deleted completely by a non-recursive walk
- [ ] #7 A mount appearing inside a candidate mid-delete stops the delete there (EXDEV) and marks the candidate changed
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-2: (1) AC3 (oracle in every engine test): wrap each container engine test with Oracle::arm(Fixture::new().eligible(<candidate>)), then judge(&run)?.assert_clean(). Use judge_runs(&[..]) for several runs. Also assert the strace view (verdict.traced_paths()) where agreement matters. (2) Keys: on the scratch tmpfs the key is (path, dev, ino, btime). On the overlay root, btime is dropped (copy-up changes it), so a delete+recreate that reuses the inode number is invisible to the snapshot there and only strace sees it. Keep candidates on scratch. (3) Scale measured in oracle_large_tree: 100k files built through Scratch ops in about 0.5 s; one whole-container snapshot of about 102k objects in about 0.33 s; strace adds per-unlink overhead (50k unlinkat took 2 to 12 s depending on host load). (4) A mode-000 directory of another mapped uid: tester gets EACCES and container root can read it (CAP_DAC_OVERRIDE over mapped uids); tester_cannot_read_other_users_mode000_dir proves both.

Forward-carried from task-59: (1) Every traced engine run must be judged by an Oracle armed before it; Verdict is #[must_use]. (2) The engine's fd-relative unlinkat shows up in strace -y as '<dir fd path>/name'. If a directory is renamed mid-delete (AC #4, #7), those paths no longer match the before snapshot; the oracle logs them as Unmatched (symlink prefix, '..', or a ' (deleted)' dir fd) instead of dropping them, and the snapshot diff stays authoritative. Assert on verdict.unmatched where the test expects such paths. (3) For the 100k/200k trees, a whole-container snapshot of about 100k objects takes about 1.2 s each (e2e-full oracle_large_tree), plus classification of every created-during-run removal; budget accordingly. (4) Wall time on this host varies several-fold; for performance assertions prefer counts (syscalls, ptrace stops via voluntary_ctxt_switches) over seconds.

Forward-carried from task-3: (1) Descend with diskgc_core::resolve::open_beneath(host, dir_fd, mount_id, rel, access, want), passing one name at a time. It sends openat2 with BENEATH|NO_SYMLINKS|NO_XDEV, then a statx confirming stx_mnt_id == mount_id. A mount inside a candidate surfaces as UnsafePath{why: CrossesMount} (EXDEV) or LeftMount (the post-check); the engine maps that to contains_mount/changed. (2) Stat fields (uid, ino, file_type, btime, mnt_id...) are Option, Some only when the kernel answered. Treat None on an identity or owner check as fail-closed (unsupported_fs), never as a default. (3) Host::statx(Base::Dir(fd), name) does not follow the last component but does follow earlier ones, so pass single names or the empty path. (4) O_PATH fds (Access::Path) cannot be listed or flocked. Open directories with Access::Read through open_beneath; an empty rel opens the dir itself as '.'. The port has no directory listing yet, so add read_dir(fd) as a port op. (5) unlinkat/rmdir must be new, distinct Host methods (the task-50 gate guards exactly those). (6) Container pattern for primitives: diskgc-e2e/src/host_probe.rs drives LinuxHost as tester, and diskgc-e2e/src/host_suite.rs judges every run with the oracle.

Forward-carried from task-14: (1) The deleting port op exists: diskgc_core::host::Delete (a trait of its own over Host) with unlinkat(dir_fd, name, Unlink::NonDir|Dir). Take H: Delete only in the deletion engine. LinuxHost does not implement it yet: add it here, behind the task-50 gate. (2) ModelHost fails the test (panics, 'ModelHost call check failed') on an unlinkat unless the previous port call was a successful statx(Base::Dir(dir), name) of that name in that directory (any fd of it): the identity check must come right before each unlinkat, so a statx from the walk, or one followed by descending into the directory, does not count, on unlinkat/statx of more than one name, and on openat2 without the step-2/3 flags. (3) Model events for this task's ACs: replace: <path> (new inode at the name; the old one detached), remove, rename, mkdir, at {op: unlinkat, path, nth, when: before}. diskgc-model tests/behaviour.rs an_entry_replaced_between_its_identity_check_and_its_unlink_is_the_one_unlinked shows the accepted window on the trace (Call.object of the unlinkat is the replacement). (4) The port still has no read_dir; when you add it, ModelHost needs it too (entries of Kind::Dir in diskgc-model state.rs; mounts on entries are crossed by State::child). (5) Not modelled: permissions (mode-000 dirs: container test), mounts appearing mid-run (no mount event yet; add one to diskgc-model load.rs Action if AC #7 needs a model test).

From mped's review of task-14: make one core helper, checked_unlink(host, dir, name, expected) -> Removed | Changed, the only caller of Delete::unlinkat (statx of the name right before, compare (dev, ino, owner) with the walk's record, then unlinkat), and enforce that with module visibility or a grep test. Also from task-14: the model's remove event is rm -r (every entry below is unlinked; a directory still held open stays, empty), which fits the 'concurrent rm -rf of a candidate' scenario.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
