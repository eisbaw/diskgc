---
id: TASK-61
title: Host primitives hardening from task-3 review
status: To Do
assignee: []
created_date: '2026-10-17 01:02'
updated_date: '2026-10-17 06:59'
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
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
