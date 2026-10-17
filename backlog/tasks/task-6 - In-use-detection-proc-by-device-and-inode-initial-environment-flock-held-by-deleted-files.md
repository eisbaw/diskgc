---
id: TASK-6
title: >-
  In-use detection: /proc by device and inode, initial environment, flock;
  held-by-deleted-files
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 06:59'
labels:
  - wave-1
  - irreversible
  - safety
dependencies:
  - TASK-3
  - TASK-14
references:
  - docs/safety.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: deleting files in use is the worst failure. Implement the in-use checks: open fds, cwd, root, memory maps matched by (dev, inode); initial-environment path heuristic; flock on the candidate directory; the one-second freshness bound per candidate; held_by_deleted_files collection. See docs/safety.md#in-use.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Container tests: a file held open, a cwd, an mmap, a TMPDIR in the initial environment, a flocked dir each make the candidate in_use / locked
- [ ] #2 The known limit (export after exec is invisible) is pinned by a test and documented
- [ ] #3 A process in another mount namespace (an unshare helper bind-mounting the scratch dir at a different path) is matched by device and inode
- [ ] #4 A same-user non-dumpable process whose fd directory gives EACCES makes candidates in_use; processes exiting mid-scan (ESRCH/ENOENT) do not abort the scan
- [ ] #5 A running binary mapping its shared library from an overlayfs scratch dir is detected (overlay maps-inode mismatch)
- [ ] #6 The one-second freshness bound holds across a simulated suspend (model test with a monotonic clock including suspend time)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-2: (1) The code under test runs as uid 1000 with no capabilities, so it sees only same-uid processes' fds and maps. Helper processes that hold files open must run as tester to be visible, or as another uid to test the invisible case. (2) under_test::run_as_tester blocks until exit; long-lived helpers need a spawn variant (see the task-4 note). (3) Container root has CAP_SYS_PTRACE, so harness-side checks can read any /proc entry in the container. The container has its own pid namespace with a clean /proc.

Forward-carried from task-59: (1) /home/tester is a fresh tmpfs per test (0755 tester) and /run/diskgc a tmpfs with the lock; deleted-but-open files on these and on /tmp keep stable (dev, ino, btime) keys. (2) Helper processes that hold files open are fixtures: start them untraced via Command (as tester through 'diskgc-e2e as-tester -- ...' if they need tester's uid); only diskgc runs through ctx.run_as_tester, and every such run must be judged by the oracle (Ctx::teardown). (3) strace -y annotates fds of deleted files and directories with ' (deleted)'; the oracle classifies such removal paths as Unmatched::DeletedDirectory and logs them.

Forward-carried from task-3 (the reader contract after round 9; replaces every earlier task-3 note here, some of which had become wrong):
(1) Readers: Host::pids is a fresh /proc readdir (thread-group ids). proc_fds unions /proc/<pid>/task/<tid>/fd over every task, each fd statted through its magic link (an fd closed mid-scan is dropped). proc_links(pid, Cwd|Root|Exe) returns one LinkTarget per distinct target across tasks: match ANY of them. proc_maps (MapEntry{dev, ino, path}) and proc_environ (Environ{vars split at the first '=', without_equals}) are read through one live task.
(2) Errors: ProcError::Gone means the kernel said, through the process's pidfd, that it has no live task: pidfd_open gave ESRCH, poll gave POLLHUP, or poll gave POLLIN and a /proc/<pid>/status read made after it showed State Z with Threads 1. Never from ENOENT or a task listing; ProcError::from_host never gives Gone. Denied is EACCES/EPERM. Failed is anything else: a live task that could not be inspected, a task list that did not hold still for MAX_ROUNDS rounds, every task exiting, and on Linux 5.8-6.8 a process released during the scan. Treat Failed like Denied: in_use for everything.
(3) A live task's empty memory map, environment or fd table is never the answer (it can be a read bound to the dead old leader of a non-leader exec). A process whose environment really is empty (env -i) is Failed, and the message says its environment was empty. Decide here whether, for the initial-environment heuristic only, a Failed environ may count as 'no environ evidence'; safety.md (In use) currently says unreadable means in_use for everything.
(4) Churn: a process that creates and ends threads very fast, or execs in a loop, is now and then Failed for one read (a round needs its task list to hold still between its two listings), and now and then Denied while an exec is in progress (task-61). Real build processes (cargo, rustc) settle; the failures seen in real cargo builds came from processes that were exiting. Re-read a Failed or Denied process a bounded number of times within the 1 s freshness window before treating it as in use for everything; an exiting process turns Gone once it is a zombie.
(5) Exclude diskgc's own pid: diskgc holds candidates open itself (the exclusive flock on a candidate directory is an open fd of its own), so its own fd table would mark every candidate it is working on as in use. (The earlier reason, 'proc_fds on itself never settles', was wrong: reads of itself settle.)
(6) LinuxHost::new() is fallible: it refuses a /proc of another pid namespace (the NSpid line of /proc/self/status must hold exactly one pid, equal to getpid(); a missing file or line is refused) and an unavailable pidfd_open. diskgc's startup turns that into exit code 2. Every /proc read goes by pid number while the pidfd pins the process: a process whose number is taken over by another one mid-scan may be missed until the next scan (docs/safety.md, Threads limits).
(7) Gotchas: /proc/<pid> is owned by root for non-dumpable processes, so 'same user but unreadable' needs the Uid line of /proc/<pid>/status (no reader for that yet). /proc/<pid>/environ is the environment block's memory, which a process can overwrite in place (setproctitle), not only change via export. Host::now() is CLOCK_BOOTTIME (includes suspend), for the 1 s freshness bound. Assumed: no tid reuse within one round. Limits: docs/safety.md (In use, Threads).
(8) Fixtures: in the container, 'probe hold FILE CWD' (prints ready, exits on stdin EOF) and 'probe hold-thread FILE CWD main-exits|unshare' (diskgc-e2e/src/host_probe.rs), started untraced as tester. diskgc-linux unit tests that copy the fd table (spawn, unshare CLONE_FILES) take fd_table_guard() in the host.rs tests.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
