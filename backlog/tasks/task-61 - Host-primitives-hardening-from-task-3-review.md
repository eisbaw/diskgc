---
id: TASK-61
title: Host primitives hardening from task-3 review
status: To Do
assignee: []
created_date: '2026-10-17 01:02'
updated_date: '2026-10-17 16:30'
labels:
  - wave-1
  - hardening
  - safety
dependencies:
  - TASK-3
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: non-blocking findings from the task-3 deep review (mped-architect, codex), fixed once the primitives are stable. (1) Shadowed mount points are refused (unsafe_path) instead of resolved: since step 2 confirms the mount against the kernel, fall back to the next shallower mountinfo candidate, and retry once when the mount table changed. (2) A mount whose parent id equals its own id (namespace root, e.g. initramfs) makes '/' ambiguous and refuses every path; handle it. (3) open_beneath with Access::Read can block on a FIFO or trigger device actions: add O_NONBLOCK|O_NOCTTY or allow Read only with Want::Directory. (4) Host::statx(Base::Root, ...) follows symlinks before the last component; restrict it to an empty path or a single name. (5) stat of /proc fd links can hang on an unresponsive FUSE/NFS mount: use AT_STATX_DONT_SYNC and tie into the deadline design. (6) A symlink as the last component of a mount point gives ENOTDIR -> missing; map it to unsafe_path. (7) The host_flock acquire check only asserts waited < 5000 ms; assert the release-during-wait case actually happened.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each of the seven items is fixed with a test, or recorded as accepted with a reason in docs/safety.md
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From the task-3 second deep gate (minor, deferred): (1) diskgc_core::host::Locked::granted is pub, so any code can wrap an fd and claim it is locked. Restrict it, e.g. to an associated Host::Lock type or a sealed constructor for Host implementations, without breaking ModelHost (task-14). (2) Add a grep or clippy check (just check) that rustix::fs::flock is called only in crates/diskgc-linux/src/host.rs, outside its #[cfg(test)] module. The kernel-fact test there calls flock raw on purpose, so the check must allow that test or name it.

From the task-3 round-3 gate (deferred, do not do in task-3):
(1) Split tasks.rs collect further: a step that reads the unread tids, plus a pure function deciding trust and verdict from (listing, next, seen).
(2) Add a note at the top of the diskgc-linux test module (host.rs tests) that any test which forks, spawns or unshares must take fd_table_guard().
(3) Document or measure that a real process with a churning thread pool may come back Failed (safe, but it costs reclaim).
(4) Remaining limit from the B3 analysis: a second non-leader exec in the same process between a task's stat and its second read is not detected.

From task-3 round 4: item (4) of the round-3 note (a second non-leader exec between a task's stat and its second read) is moot. A listed exec'ing thread now always triggers a leader re-read (its old tid shows the dead old leader or vanishes), and an unlisted one falls under the 'started after the last trusted listing' limit. Nothing new to file.

From task-3 round 5 (deferred):
(1) mped's relaxed trust rule: the verdict over the trusted listing plus the forced leader re-read, without requiring every tid of the next listing to be read. In his simulation it cut Failed sharply at high churn, with no misses. Evaluate it against the counting argument before adopting. (Adopted in round 8.)
(2) QA's churn probe (task-3 scratchpad) measured the reclaim cost of the rules then current: processes spawning threads every few tens of microseconds came back Failed for most reads. Those were debug builds under load; see the round-8 note for what release builds show.
(3) tid reuse at pid_max 32768: the 'no tid reuse within one scan' assumption gets weaker on hosts with a small pid_max and heavy churn. Quantify it, or detect reuse (e.g. stat starttime change for non-leader tids, where no de_thread copy applies).
(4) Closing the round-3 item (3) (document or measure that a churning thread pool may come back Failed): covered by (2) and by the safety.md Threads text ('a busy thread pool can cost a candidate this way').

From task-3 round 6 (deferred):
(1) hidepid=invisible/ptraceable: a visibility change mid-scan makes the pid vanish from the /proc readdir while the process lives, giving a false Gone. Detect hidepid (mount options of /proc) and treat absence as not proof when it is set.
(2) Cost of listed(): a full /proc readdir per ENOENT under churn. kill(pid, 0) is an O(1) alternative (mped tested it); measure before switching, and check that its semantics match (ESRCH only after the tgid is released, EPERM for other users).
(3) Root-owned processes with an empty environ are unverified. Under --allow-root an empty environ is Failed (unreadable), with the same cost as for same-user env -i processes.

From task-3 round 7 (deferred, documented in safety.md Threads limits):
(1) Codex 3 needs a coincidence of independent events: a privileged reader, an exec landing after the first getdents of the exec'ing thread's fd dir, an fd table larger than the getdents buffer, and child inodes bound to P while the directory inode stays bound to T. The second getdents can then confirm EOF against the dead old leader, and a prefix of the survivor's table is taken for the whole. Possible fix: re-read the fd dir when its listing filled most of the buffer, or compare the fd count with /proc/<pid>/task/<tid>/status FDSize.
(2) Related: a larger initial getdents buffer (FIRST_BUFFER, 64 KiB) makes the prefix case rarer; measure the cost first.
(3) The churn cost of the rules: QA's probe (proc_fds of a process spawning threads every 50-200us) came back Failed for most reads (debug builds under load, see round 8). mped's relaxed trust rule (round-5 note) was the candidate to cut it.

From task-3 round 8:
(1) CORRECTION of the round-5 (2) and round-7 (3) notes: those churn measurements were debug builds under load. In release builds moderate churn gives no or almost no Failed, and only extreme churn (many threads started and ended every few tens of microseconds) gives a small share (see round 9, H3).
(2) mped's relaxed trust rule (R9) is adopted (list/count/list per round, leader read every round); that item is done.
(3) kill(pid,0) vs /proc readdir cost is moot: Gone now uses poll on a pidfd.
(4) hidepid is no longer a false-Gone risk, only Failed; detecting hidepid is no longer needed for soundness.
(5) Options to cut the remaining Failed under extreme churn: raise MAX_ROUNDS (QA compared 16 and 64), or retry only list/count/list (not the reads) when a listing is incomplete.

From task-3 round 9:
(1) H1: on exec-loop targets (a thread execs every few milliseconds) a small share of reads is Denied, and occasionally an environ read is Failed (empty while the exec replaces the memory map). Hypothesis, not verified: between exec_mmap and set_dumpable in begin_new_exec (fs/exec.c) the new mm is not yet dumpable for the reader, so ptrace_may_access in mm_access refuses with EACCES. Options: treat EACCES from a task whose exec is in progress as one more round (bounded), or leave it to task-6's bounded re-read of Denied/Failed processes. Measure before choosing.
(2) H2 (collect readability) is done in round 9: shortfall() is a pure function, Exited and Dead are one status, the listing.has(pid) guard is gone (reading the leader again is always safe), and the FirstLiveTask early return is in one place.
(3) H3: under heavy churn (16 threads started and ended every 50us) a noticeable share of reads is Failed: the task list rarely holds still from one listing to the next within MAX_ROUNDS rounds. Options as in round 8 (5): more rounds, or repeat only list/count/list (not the reads) while the listing is incomplete.
(4) On Linux 5.8-6.8 pidfd_poll never reports POLLHUP, so a process released during a scan is Failed, not Gone. pidfd_send_signal(pidfd, 0) gives ESRCH exactly when the pid has no task (the lookup comes before the permission check), on every supported kernel; verify that in kernel/signal.c and consider it as the hang-up test there.
(5) The pidfd_open probe in LinuxHost::new is tested only through check_pidfd_open with scripted errnos: removing the call from new() passes every test. Pinning it needs a child under a seccomp filter that blocks pidfd_open.
(6) The real-kernel exec-loop test (a_process_execing_from_a_thread_in_a_loop_is_never_gone) is a regression guard, not a proof: the pidfd_poll race is too rare to be hit in its few seconds (the scripted test one_transient_readable_poll_is_never_gone bites the old rule).

From task-3 round 10:
(1) When a round finds nothing live and the pidfd is not yet readable, poll the pidfd with a short bounded timeout before the next round, so that an exiting process becomes Gone instead of Failed after MAX_ROUNDS back-to-back rounds (the cargo workload's Failed reads were all exiting processes).
(2) startup() in crates/diskgc/src/main.rs builds LinuxHost, and so refuses to start, for every command except --version/--help, including commands that never read /proc (profiles, check). Decide in the CLI task which commands need the /proc checks.
(3) The equal-pid namespace test (the_host_is_refused_under_an_ancestors_proc_with_an_equal_pid) relies on ns_last_pid in a user namespace and a fresh /proc mount; if a future CI sandbox forbids mounting proc, it fails rather than skips. Decide then whether to move it to the container suite.

From task-3's final commit gate:
- ProcRoot::names uses rustix Dir::read_from, which does fcntl_getfl + openat(fd, ".") per listing; use Dir::new(fd) (takes ownership, no reopen). Two extra syscalls inside the window where a task listing must hold still; QA measured slightly higher Failed under heavy churn than round 10.
- Error messages from Kernel::list_once, fd_numbers and read_environ print the procfs-relative path (e.g. 1234/task/1234/fd); route them through ProcRoot::shown like the other sites.
- LinuxHost::new: require fstatfs(proc).f_type == PROC_SUPER_MAGIC so a non-procfs /proc is refused directly rather than via a missing self/status.
- Add a real-kernel regression test for /proc replaced after startup (mount over /proc, and lazy umount exposing an ancestor procfs), next to the equal-pid namespace test; mped's probe r12_proc_swap and QA's remount.rs show it catches the round-11 code.
- procfs_is_reached_only_through_the_held_fd: derive its file list from src/ instead of a hard-coded list, and limit the exemption to LinuxHost::new rather than any fn named new. Being grep-based, it cannot catch a deliberately obfuscated path (QA showed format!("/{}/...", "proc") passes).
- docs/architecture.md: the /proc readers return the result of the per-task round algorithm in diskgc-linux, not syscall-shaped calls; say so.
- a_process_execing_from_a_thread_in_a_loop_is_never_gone: its 20-exec viability threshold flakes above load ~70 (5/720 in QA's stability run, also seen in round 11); never the safety assertion. Lower the threshold or scale it with the time budget.

From task-14: diskgc-model's ModelHost builds Locked with Locked::granted in flock_try and flock_wait (another crate), so restricting granted must leave a way for Host implementations outside diskgc-core.

ModelHost follow-ups from task-14's commit review (mped), deferred, do not block wave 1:
(1) Derive /proc link text with State::path_of instead of storing the open-time path in Ofd.path / Ref.path (Linux follows renames; the stored text is stale after one).
(2) An optional per-node ctime (today mtime = ctime = btime at load, so 'ctime newer than mtime' cannot be expressed), and an inode-reuse variant of the replace event (inode numbers are never reused, so the reused-inode replacement window cannot be modelled).
(3) A spawn/fork event and open file descriptions shared between processes. Task-6 needs these for the fork handoff race; tasks 10 and 54 need children inheriting diskgc's fds (the host lock).
(4) A test that the yaml block of docs/testing.md (Model tests) equals crates/diskgc-model/tests/models/sketch/locked-cargo-target.yaml (docs/ is not a flake build input today; the test would need it or run outside the sandbox).
(5) NodeSpec doc caveat: file attributes must never take mapping values, because a misspelled attribute with a mapping value silently becomes a directory entry (a mapping of mappings is a directory).
(6) Event paths are only syntax-checked at load (whether they exist depends on the run); a wrong path surfaces as a 'host model error' panic when the event fires.

From task-14's final review (mped):
(7) Kernel threads: LinuxHost answers PF_KTHREAD processes with an empty Ok for fds, maps, environ and exe; the model has no kernel threads. Add a kernel_thread: true flag and note the exception in testing.md Limits.
(8) Generated processes often lack stdio fds (subsequence(files, 0..=2)), so about a third read as Failed; with the Denied/Failed share about half of generated processes are uninspectable. Give them fds 0-2 on a /dev/null in the tree.
(9) The replace event's description (spec.rs, schema) says the old object is 'detached, as if renamed away'; it is unlinked (nlink 0) and a held old directory keeps statable entries, a state Linux cannot have. Fix the wording, or make a directory replace follow remove's rm -r.
(10) Pin serde-saphyr exactly (2.0.0 and granit-parser 2.0.0 were published 2026-10-13); revisit before task-25.

From task-50: procfs_is_reached_only_through_the_held_fd (diskgc-linux host.rs) lists its source files by hand, so a new module escapes it. deleting_syscalls_only_behind_the_gate lists them from the src directory instead; switch the /proc scan to that, and consider sharing code_only (it blanks out comments and string literals) so a /proc path in a comment cannot cause a false positive and one in a string still counts.

Correction to the note above: code_only blanks string literals, so the /proc string-literal check cannot use it as is. Only the PROC identifier check can.

From task-50's review (deferred hardening):
- H3: ModelHost should be able to inject a Cause::SandboxGate refusal on unlinkat (a model event or a host-model flag), so task-5 can test that a refusal is an error for the candidate and the run, not a skip or 'changed'.
- H4: expose the cached gate status without a Permit (sandbox::status()), so ensure/watch without --dry-run fail fast at startup and 'diskgc check' can report 'sandbox gate: closed (reason)'. Noted on task-10.
- permit() returns &'static str and drops the structured NotInSandbox. It could cache and return the structured reason (e.g. &'static NotInSandbox, with io::Error kept as its kind and message) so callers can match on which marker failed.
- The flag test sandbox_gate_flag_is_enabled_nowhere scans .cargo/config(.toml) only at the repository root. A crates/*/.cargo/config.toml (or diskgc-e2e/.cargo/config.toml) could set features or rustflags unseen; scan those too.

From task-50 round 3 (mped): consider replacing the hand lexer in crates/diskgc-linux/src/source_scan.rs with a syn visitor (a dev-dependency; syn is already in Cargo.lock) if it needs another fix. It has had two lexer bugs fixed so far (raw C strings, the '\'' char escape). Symlinks under src/ are already refused (fixed in task-50 rather than filed).

From task-50's final review (mped, QA). The scanner is frozen for task-50; switch to syn on the next lexer bug.
(11) #[cfg_attr(..., path = "...")] mod x; passes the scan: in hiding_forms flag a path identifier followed by = anywhere inside an attribute's brackets.
(12) pinned_lines reads only the first line of a workspace dependency, so a reviewed multi-line rustix entry is no longer checked: return a 'spans lines, unsupported' string when brackets are unbalanced so the test fails.
(13) Document that gated names must appear inside the allowlisted function (fully qualified or a use in its body); a module-level use std::process::Command cannot be allowlisted. Fix the failure message of deleting_syscalls_only_behind_the_gate, which says 'route them through the sandbox gate' even for Ungated entries.
(14) use<'a> precise capturing is falsely reported as a glob import; skip a use followed by <.
(15) Within the documented limits but cheap: OFlags::all(), !OFlags::..., OFlags::from_name, type F = std::fs::File; F::create, rustix destructive calls not on the list (e.g. fremovexattr), .cargo/config.toml [patch]/[source], os_access reading only the first name after std::process.
(16) The gate refusal's subject prints LinuxFd(OwnedFd { fd: 4 })/victim (shown() in host.rs); show the path instead.
(17) Housekeeping: the earlier item about procfs_is_reached_only_through_the_held_fd listing files by hand is done by task-50 (shared source_scan); its 'code_only' correction note is obsolete. Still open: the /proc exemption matches any fn named new, not only LinuxHost::new.
(18) Kernel threads in ModelHost, and stdio fds for generated processes, are items (7) and (8) above.

From task-51: the ProcRoot::names item (Dir::read_from reopen) is done: names and the new LinuxHost::read_dir share dir_names(OwnedFd), which uses rustix Dir::new on the fd it was given. Still open from that list: the procfs-relative paths in error messages, the PROC_SUPER_MAGIC check, the /proc-replaced regression test.

From task-51's review (mped, QA):
(19) LinuxHost::read_dir's fresh open should use O_NOATIME (retry without it on EPERM): listing updates atime under relatime, and a daily tally would keep /tmp subdirectories fresh and defeat systemd-tmpfiles age cleanup (which uses O_NOATIME for this reason). diskgc ignores atime.
(20) Put the call's subject (name/path) in the driver's pause and kill control lines; once the driver's delete calls the real engine (filesystem order), unlinkat:N no longer identifies the entry.
(21) Controlled is !Sync (Cell/RefCell); use AtomicU64 and a Mutex if a watch-style driver shares the host across threads.
(22) diskgc-e2e under_test.rs: SYS_READ = 0 is x86_64 only; add cfg(not(target_arch = "x86_64")) compile_error!.
(23) Spawned is dropped without kill/reap when wait_paused or resume errors; low impact (container teardown).
(24) Two over-long lines in testing.md's Controlled pause and kill points bullets.

Filed from task-5's deep gate (round 1; mped/Codex), for a later hardening pass: (1) the deletion engine's reopen cost is quadratic in the depth of a single deep chain (frames past the 16-fd window are reopened from the nearest open ancestor): consider checkpoint fds (e.g. keep every 16th ancestor) or a depth cap with a skip reason; (2) Delete::fchmod takes any permission bits, while its one caller only adds owner write: consider narrowing it to add_owner_write(dir) so the gated surface cannot remove permissions or set setuid bits; (3) untested scenarios: an entry renamed OUT of a candidate between walk and delete (expected: its checked_unlink finds ENOENT -> changed), and a bind mount over a FILE inside a candidate appearing after the walk (unlinkat EBUSY -> changed; a container test needs root to bind-mount while the driver is paused); (4) the engine module docs (diskgc-core/src/engine/mod.rs) restate safety.md's walk-shape paragraph: point to safety.md instead so the rule has one home.

From task-5's final review (Codex, mped, QA):
(25) Deadline check immediately before make_writable's fchmod: the deadline can expire during a directory open or its identity statx, after which delete calls make_writable without another check, contradicting safety.md's 'never once the deadline has passed'. Cover with a model clock advance after the open.
(26) LinuxHost statx without AT_NO_AUTOMOUNT: statx(2) triggers automount on the last name unless the flag is passed, so the walk would mount an autofs point inside a candidate (a dead NFS one could hang). One-line fix; openat2 with RESOLVE_NO_XDEV already returns EXDEV before automounting. Untested reasoning from fs/stat.c.
(27) Model support for inode reuse (a replace that keeps the inode number) so type and birth-time compares are pinned at both comparison sites; a test that chowns a directory to pin the owner check at reopen.
(28) A listing test with a 255-byte name, pinning that the RawDir buffer never yields EINVAL.
(29) Dead match arm in the walk's open_failure (inner ResolveError::Host(_) => Changed); restructure so removing the earlier arm cannot turn host errors into changed.
(30) Stale text: the task-51 note here says listings use dir_names over rustix Dir::new; they now use for_each_name over RawDir (task-5). safety.md 'Scope' says directories are matched by (device, inode); the identity is (device, inode, owner, type, birth time).
(31) block_ipc false has no enforcement that the caller consults ipc_entries(); documented in Rules and the task-6 note.

From task-6: (1) round-9 H1 (transient Denied/Failed while a process execs) is absorbed by task-6's re-reads (2, 8, 30 ms) of unreadable processes whose status gives no lasting reason; a process mid-exec looks non-dumpable for a moment, so non-dumpable ones are re-read before they are excused. (2) Scan cost: on a desktop the fd tables of multi-threaded processes dominate (proc_fds reads every task's table; a browser with 44 threads x 470 fds was 93 ms of a 0.9 s single-threaded scan). task-6 added LinuxHost::proc_views (8 threads; about 0.2 s for 500 processes under load). A further cut, if needed: read a task's fd table only when it is not shared with an already-read task, e.g. kcmp(KCMP_FILES) made AFTER the representative's read (sharing is never re-joined, so equal-after-read means equal throughout); rustix has no kcmp wrapper and the source scan forbids raw syscall, so it would need a reviewed wrapper.

From task-6's deep gate round 1 (deferred, filed by the coordinator): (1) held_by_deleted_files on btrfs (device of deleted files vs the requested filesystem's statx device) and bytes double-counted when several processes hold the same deleted file; (2) never reuse an unsettled scan in inuse::Checker; (3) measure how the 16-listing bound (inuse::scan::MAX_LISTINGS) converges under configure/make -j fork rates; (4) the environ heuristic could also split values at whitespace and '=' (only adds matches); (5) docs/safety.md limit 4 should also cover pid reuse after a process was read; (6) add bcachefs to inuse MAPS_BY_INODE; (7) make the in_use message for a newgrp/sg shell (dumpable, same uid, other group) read well: it currently says 'cannot be inspected', which hides why.

From task-6's deep gate round 2 (deferred, filed by the coordinator): (1) proc_status does more work than needed for other users' processes (uid_map, task listing and live-task owner are only needed for the user's own); (2) LinuxHost::proc_views swallows reader-thread spawn failures without logging (the work still gets done by the remaining threads); (3) inuse::Scan::processes() counts gone and unreadable entries too (every entry with a read map), name or split it; (4) Codex's non-blocking note on per-process retry spacing is covered by task-6's per-pid due times (each_process_is_read_again_on_its_own_schedule).

Correction to 'From task-6 (1)' (the first task-6 note here): round-9 H1 is no longer absorbed by 'non-dumpable ones are re-read before excused' alone; since task-6 round 3 every refused process of the user is re-read and an excuse needs every one of its status reads (four, counted apart from environment-only failures) to agree, with one exception (a last read finding another user's process). Item (2) there (kcmp dedup) stands.
From task-6's deep gate round 3 (deferred, filed by the coordinator): (1) pid reuse within a scan is detected only by 'found gone, listed again' (once per pid); identify a process by pid plus start time or by a held pidfd; (2) a live read-only test asserting that systemd --user, when refused, is excused as more capable (environment-dependent today: the live scan only prints notes); (3) rename engine::delete to delete_unchecked or narrow its visibility, and make inuse::Checker::with_options non-public or guarded; (4) a non-dumpable process caught exiting on its 4th status read gives a false in_use (safe, costs reclaim); (5) pull gather's scheduler (due times, listing budget) into its own struct; (6) QA: on this machine 'nix flake check' can report all checks passed while building nothing; docs/testing.md (Commands) now says gates build checks explicitly; make 'just ci' build them explicitly with --rebuild; (7) Codex's mixed-outcome note is fixed in task-6 round 4 (refusals counted on their own, a_late_refusal_gets_every_classification).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
