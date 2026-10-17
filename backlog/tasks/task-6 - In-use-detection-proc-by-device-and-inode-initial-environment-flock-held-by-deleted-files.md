---
id: TASK-6
title: >-
  In-use detection: /proc by device and inode, initial environment, flock;
  held-by-deleted-files
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 13:17'
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

From task-3's commit review (mped): process-level handoff race. diskgc lists P; P forks C (C inherits P's fd on X); P closes X; diskgc reads P and misses X; C was not in the pid listing, so X looks unused while C holds it. One timed event from diskgc's side, so it is NOT covered by the accepted check-vs-delete limit. The scan needs the process-level analogue of the per-task rule: after reading, list pids again, read any new pid, repeat until a listing adds nothing (bounded; give up as Failed for the affected candidates). Also exclude diskgc's own pid (its flock fd holds candidates).

Forward-carried from task-14: ModelHost answers the /proc readers from the model's process table: proc: readable|denied|failed per process (Denied is EACCES, Failed a HostError::invalid), Gone once the pid left the model (exit event). Differences from LinuxHost: an empty fd table, map or environment is answered as Ok(empty) (LinuxHost: Failed; say proc: failed to model that), one task per process (proc_links returns one target), no uid, and diskgc itself is not among the pids (the port has no getpid op; if this task adds one, add the self process to ModelHost so the own-pid exclusion is model-testable). Suspend for AC #6: event advance: <dur> jumps the clock (CLOCK_BOOTTIME includes suspend); events at {elapsed: <dur>} fire during sleep/flock_wait.

From task-14 (update): the empty-answer divergence is gone. ModelHost now reports an empty fd table, map or environment, or a missing exe, of a readable process as ProcError::Failed, like LinuxHost. Not modelled yet, filed on task-61 (3): spawn/fork events and fds shared between processes, which this task's fork handoff race needs.

Dogfooding run 2026-10-17 (read-only probe of LinuxHost plus ncdu of $HOME, /tmp, /var/tmp; full report in the session scratchpad dogfood/). DECISION by the user: narrow the unreadable-process rule. Read literally, safety.md's 'an unreadable same-user process makes every candidate it could use in_use' blocks all reclaim on any desktop: systemd --user, (sd-pam) and ssh-agent are non-dumpable (Denied), and sudo -i / fusermount3 are setuid root with real uid = user. New rule to write into docs/safety.md (In use) with tests: non-dumpable same-uid processes and setuid processes whose effective uid is not the user's are logged but do not block; any other unreadable same-user process still blocks. Define 'same user' explicitly (real vs effective uid). Measured on this machine: 486 processes scanned in 0.64 s, 176 of this user's readable, 6 Denied, 0 Failed, 1 Gone (exiting, pidfd-confirmed); no false in-use seen.

From task-5 (deletion engine): the engine asks for socket/FIFO liveness through diskgc_core::engine::IpcLiveness { fn is_live(&self, entry: &IpcEntry) -> Result<bool, String> } with IpcEntry { kind: IpcKind::{Socket, Fifo}, id: DevIno, path: PathBuf (absolute, for /proc/net/unix which lists sockets by path; the socket inode there is sockfs's, not the file's) }. Ok(true) = live, Ok(false) = no visible process uses it, Err = cannot tell (the engine treats it as live). Used only under IpcPolicy::BlockLive (the user's dogfooding decision; default BlockAny). A FIFO held open is also caught by the general in-use check by dev:ino, since the walk records every entry: feed the in-use check from WalkRecord::objects() (candidate first). The in-use check runs between engine::walk and engine::delete (docs/safety.md, Deletion step 2); the freshness bound is measured to the start of engine::delete.

From task-5 round 2 (deep gate, mped's guidance): the IPC policy moved out of the walk. engine::Rules { owners, block_ipc, max_entries }: block_ipc = true (the default policy) refuses the candidate at the first socket or FIFO (contains_ipc). With block_ipc = false the walk refuses nothing for them and records them; WalkRecord::ipc_entries() returns IpcEntry { kind: Socket|Fifo, id: DevIno, rel: path below the candidate }, and this task decides liveness with the same /proc scan it uses for in-use (no callback from the walk; the IpcLiveness trait is gone). Guidance: (1) a FIFO held open is already caught by the in-use check on WalkRecord::objects() (fds matched by dev:ino), so no extra FIFO check is needed; (2) /proc/net/unix is per network namespace and lists the path given at bind time, so absence there is no proof a socket is dead (a socket bound in another netns, or renamed/moved after bind, is not found by path); (3) use NETLINK_SOCK_DIAG with UNIX_DIAG_VFS (request flag UDIAG_SHOW_VFS), which reports the bound socket file's (dev, ino) for an exact match against IpcEntry::id; a socket not found there counts as live unless the scan is complete for diskgc's netns, and sockets of other netns are invisible (state it as a limit; the PRD now says 'a socket bound in diskgc's network namespace'). Any failure of that scan: treat every socket as live.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
