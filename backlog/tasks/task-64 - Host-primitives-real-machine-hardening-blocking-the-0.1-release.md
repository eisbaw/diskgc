---
id: TASK-64
title: 'Host primitives: real-machine hardening blocking the 0.1 release'
status: To Do
assignee: []
created_date: '2026-10-17 23:54'
updated_date: '2026-10-20 12:15'
labels:
  - wave-1
  - hardening
  - safety
dependencies:
  - TASK-3
references:
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: 0.1 lifts the sandbox gate in the release package (PRD Scope, Releases; docs/safety.md, Pre-release sandbox gate), so diskgc will delete on real machines with dead network mounts, FIFOs, automounts, old kernels, btrfs and busy CI hosts. These task-61 items are correctness or safety, not cosmetics, or matter only once the gate is lifted on a real machine; the rest of task-61 stays there, deferred to wave 5.

Item references are task-61's: D1-D7 are the numbered items of its description; #N are the running numbers (1)-(43) in its notes; other items are named by the note heading they sit under.

A. Blocking or hanging opens and stats:
- D3: open_beneath with Access::Read can block on a FIFO or trigger device actions (O_NONBLOCK|O_NOCTTY, or Read only with Want::Directory).
- D5: stat of /proc fd links can hang on an unresponsive FUSE/NFS mount (AT_STATX_DONT_SYNC, tie into the deadline design).
- #26: LinuxHost statx without AT_NO_AUTOMOUNT mounts an autofs point inside a candidate (a dead NFS one could hang).
B. Path resolution correctness:
- D4: Host::statx(Base::Root, ...) follows symlinks before the last component.
- D6: a symlink as the last component of a mount point gives ENOTDIR -> missing; map it to unsafe_path.
C. /proc integrity:
- task-3 final commit gate: LinuxHost::new requires fstatfs(proc).f_type == PROC_SUPER_MAGIC.
- task-3 final commit gate: a real-kernel regression test for /proc replaced after startup (mount over /proc, lazy umount exposing an ancestor procfs).
D. Deletion engine permissions and deadlines:
- #25: deadline check immediately before make_writable's fchmod.
- task-5 deep gate round 1 (2): narrow Delete::fchmod to add owner write only, so the gated surface cannot remove permissions or set setuid bits.
- task-5 deep gate round 1 (3): untested races: an entry renamed out of a candidate between walk and delete (ENOENT -> changed); a bind mount over a file inside a candidate after the walk (EBUSY -> changed).
E. Delete only what was checked:
- #32: bind a Clear to its exact WalkRecord, so a re-walk or a hard-linked candidate cannot be mixed up.
- task-6 deep gate round 3 (3): rename engine::delete to delete_unchecked or narrow its visibility; make inuse::Checker::with_options non-public or guarded.
- #27: model inode reuse (a replace keeping the inode number) so type and birth-time compares are pinned at both sites; a chown test pinning the owner check at reopen.
F. Process identity:
- task-6 deep gate round 3 (1): pid reuse within a scan is detected only by 'found gone, listed again'; identify a process by pid plus start time or by a held pidfd.
- task-6 deep gate round 1 (5): docs/safety.md limit 4 must also cover pid reuse after a process was read.
- task-3 round 5 (3): tid reuse on hosts with a small pid_max (32768) and heavy churn; quantify or detect.
G. In-use scan soundness and liveness on real hosts and kernels:
- task-6 deep gate round 1 (2): never reuse an unsettled scan in inuse::Checker.
- task-3 round 9 (4): on Linux 5.8-6.8 (e.g. Debian 12, Ubuntu 22.04, RHEL 9) pidfd_poll never reports a hang-up, so a user process released during a scan is unreadable and makes every candidate in_use; on a busy CI host ensure may reclaim nothing. Evaluate pidfd_send_signal(pidfd, 0) == ESRCH as the hang-up test.
- task-3 round 10 (1): poll the pidfd with a short bounded timeout before the next round, so an exiting process becomes gone, not failed.
- task-6 deep gate round 1 (3) and #38: the 16-listing bound under make -j fork rates and heavy load ('process list still growing'); measure, and decide what ensure and the test do with an unsettled scan.
H. Host lock tests:
- #37: a_lock_conflicts_until_its_fd_is_closed failed 1/600 at -j4, unexplained; investigate before claiming fd_table_guard covers flock.
- task-7 review H10: one guarded-spawn helper for host.rs tests (kill and reap on drop, bounded wait), a candidate fix for #37.
- D7: the host_flock acquire check must assert that the release-during-wait case actually happened.
I. Roots and config validation (the schema freezes at 0.1):
- #39: static root normalisation (reject or normalise '..', '//', '.', duplicates in find.roots, tier roots and --roots) and dedupe of overlapping roots by identity at run time.
- #39 scope note: profile find.roots (task-8) and --roots (task-10) are wave-1 surfaces; tier roots arrive with task-15, which then applies the same normalisation (see the note on task-16).
- #41: idle_for: 0s makes every known-age candidate idle; reject it at load or document it.
J. Side effects and reporting on real filesystems:
- #19: LinuxHost::read_dir should open with O_NOATIME (retry without on EPERM), so listing /tmp does not defeat systemd-tmpfiles age cleanup.
- task-6 deep gate round 1 (1): held_by_deleted_files on btrfs (device of deleted files vs the requested filesystem's statx device) and bytes double-counted when several processes hold the same deleted file.
K. Gate flag test coverage (the 0.1 release task relies on it):
- task-50 review: sandbox_gate_flag_is_enabled_nowhere scans .cargo/config(.toml) only at the repository root; a crates/*/.cargo/config.toml could set the feature or rustflags unseen.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A (blocking or hanging opens and stats: D3, D5, #26): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [x] #2 B (path resolution: D4, D6): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [x] #3 C (/proc integrity: PROC_SUPER_MAGIC check, /proc-replaced real-kernel test): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [ ] #4 D (deletion engine permissions and deadlines: #25, task-5 round 1 (2) and (3)): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [ ] #5 E (delete only what was checked: #32, task-6 round 3 (3), #27): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [ ] #6 F (process identity: pid reuse within and after a scan, tid reuse): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [ ] #7 G (in-use scan soundness and liveness: unsettled scan reuse, Linux 5.8-6.8 hang-up, bounded pidfd poll, 16-listing bound under load, #38): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [ ] #8 H (host lock tests: #37, H10, D7): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [ ] #9 I (roots and config validation: #39, #41): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [ ] #10 J (side effects and reporting: #19 O_NOATIME, held_by_deleted_files on btrfs): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
- [x] #11 K (gate flag test scans nested .cargo/config files): each item fixed with a biting test or explicitly documented as a limit in docs/safety.md
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-9 round 2 (review filings H3, H7):
- H3: a dormant shell that exported CARGO_TARGET_DIR after exec is invisible to the initial-environment heuristic (environ shows the environment at exec); for an attached target its later cargo is caught by the project-dir rule, for a detached one by rule 6 only while cargo runs. Accepted limit; worth a real-machine check.
- H7: a mode-000 directory (another tool's, or a test's) inside target/tmp/<x> makes cargo_target::inspect fail closed with unreadable for the whole target, indefinitely, although target/tmp is never deleted. Consider not listing directories two levels down that cannot be profile directories, or reporting it so the user can act.

From task-53's deep gate (mped H4-H6, cargo-target reclaim protocol):
- H4: measure the lock hold time (Reclaimed::locks_held, driver cargo-reclaim held_us) on a per-unit (nightly) target with many units: phase A deletes each build/<pkg>/<hash>/fingerprint as its own part, each with a lock re-listing (read_dir + statx of every profile entry), an open of its unit dir and an in-use check; measured so far only on a classic target with 100k files in deps/ (0.49 s untraced).
- H5 (optional refactor): keep the inspected ProfileDir inside ProfileWalk, so reclaim needs no parallel 'walked' vector; it needs a clone of inspected data or a lifetime on the public ProfileWalk, so not done in task-53.
- H6: a lock file hard-linked into two profile directories (same inode) makes the protocol's second flock on it fail against diskgc's own first lock, so the target is 'locked' with a message blaming 'another process'. Safe (nothing deleted) but misleading: detect duplicate lock identities across the target (e.g. in judge_layout -> unsupported_layout) or word the message.

Group A (2026-10-20, partial): LinuxHost readable openat2 now uses O_NONBLOCK|O_NOCTTY; a fast disposable-container FIFO probe with no writer and a 2s alarm is added to catch regressions. All LinuxHost statx calls now use AT_NO_AUTOMOUNT, including proc magic links; proc magic-link target statx retains AT_STATX_DONT_SYNC. docs/safety.md records the remaining dead-NFS kernel lookup/deadline limit and autofs path-traversal limit. This is not TASK64 completion: B-K are separate, and D5 cannot be bounded by a cooperative deadline while a synchronous kernel syscall waits. Focused E2E and review still pending at note time.

Group A verification update (2026-10-20): disposable Nix container `just e2e-one host_read_open_of_fifo_does_not_wait_for_writer` passed 1/1 after correction. Initial test incorrectly expected F_GETFL to retain O_NOCTTY; it does not, so the container now tests FIFO nonblocking behavior and the pure host unit test checks the O_NOCTTY flag mapping. No diskgc reclaim was run on a developer host.

2026-10-20 group K candidate: sandbox_gate_flag_is_enabled_nowhere now scans .cargo/config and config.toml throughout checkout directories, including symlinked .cargo directories without following arbitrary symlink trees; nested rustflags/alias and symlinked config regressions bite. Focused Nix sandbox tests 13/13 and fmt passed in isolated worktree. Integrated full E2E and independent QA/MPED review are pending; TASK64 B-J remain open.

2026-10-20 group B candidate: LinuxHost root-relative statx now opens with RESOLVE_NO_SYMLINKS and stats the held fd, preserving final-symlink identity while refusing intermediate symlinks. A mount-point final symlink returning ENOTDIR is classified unsafe_path through a no-symlink follow-up stat. Red-before/fixed-after host tests and focused disposable Nix-container host_resolve_refuses_symlinks passed 1/1; interlock canary refused host exit 86. Integrated full E2E and QA/MPED review pending; TASK64 C-J remain open.

2026-10-20 integrated gate: group B and K changes passed nix develop -c just e2e (Nix image 139/139, native Docker 6/6) on the combined staged tree; parallel QA and MPED reviewers found no blocking issue. AC2 and AC11 checked. TASK64 remains open for groups C-J and remaining group A caveat. No diskgc execution on developer host or real project paths.

Group C candidate (2026-10-20): LinuxHost::new now fstatfs-checks the held /proc directory against PROC_SUPER_MAGIC before reading NSpid or opening a pidfd, with a contextual refusal for other filesystems. In a disposable Nix container, host_procfs_stays_pinned_after_overmount passed 1/1: the probe unshared a private mount namespace, mounted its own procfs, opened a host on it, mounted tmpfs over /proc, verified a new host specifically refused the non-procfs mount, then lazily detached the overlay and the held procfs; readers through the held fd continued listing the probe. The probe cannot model an ancestor PID namespace procfs underneath, so docs/safety.md names that limit. Interlock canary refused host execution (exit 86). Full integrated E2E and independent QA/MPED review are pending; AC3 remains unchecked until integration.

2026-10-20 integrated group C gate: nix develop -c just e2e passed Nix image 140/140 and native Docker 6/6; parallel QA and MPED reviews GO. AC3 checked with the exact ancestor-namespace construction limit documented in docs/safety.md. TASK64 remains open for D-J and remaining group A caveat; no developer-host diskgc run.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
