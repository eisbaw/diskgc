---
id: TASK-3
title: >-
  Host port and LinuxHost primitives: mounts, openat2 resolution, statx,
  statvfs, flock, /proc readers
status: Done
assignee:
  - '@mped-architect'
created_date: '2026-10-16 22:39'
updated_date: '2026-10-17 07:48'
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
- [x] #1 A symlink anywhere in a configured path yields unsafe_path; a bind mount of the same filesystem below a root is refused (container tests)
- [x] #2 statx attributes are used only when their mask bit is set; an unanswerable check yields unsupported_fs
- [x] #3 Kernel older than 5.8 (simulated) exits 2 at startup
- [x] #4 openat2 and statx are called as raw syscalls via rustix (no libc wrapper dependency, works with static musl)
- [x] #5 flock (release only by close) and the /proc readers have container tests
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. diskgc-core: Host trait (syscall-shaped: mountinfo, openat2, statx, statvfs, flock try/wait-with-deadline (release = drop/close only, no unlock op), /proc readers, kernel release; time: now (boottime) + sleep). Core types with Option per statx field (mask-checked at the LinuxHost boundary). Pure path resolution (deepest mount prefix, stacked=topmost, confirm via statx mount root + mnt id, openat2 BENEATH|NO_SYMLINKS|NO_XDEV, '.' for the mount point) and kernel-minimum check in core, unit-tested with a fake host. 2. diskgc-linux: LinuxHost over rustix linux_raw (openat2, statx, fstatvfs, flock, uname, clock_gettime BOOTTIME), mountinfo parser with octal unescaping, /proc readers (fd/cwd/root/exe stat'ed through the magic link, maps dev:inode, environ). 3. diskgc binary: kernel check at startup (exit 2), --version/--help exempt. 4. e2e: probes that drive LinuxHost as tester; container tests for symlinks, bind/stacked mounts, escaped mount points, statx masks on a real kernel, statvfs, flock (close semantics with an inherited fd, wait deadline), /proc readers, kernel<5.8 via UNAME26 personality. 5. Bite proofs (one mutated build), docs/safety.md limits, forward-carries, gates.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
NOTE: the round sections below are history. The current /proc reader rules are in the LAST 'Round N' section; earlier rounds are superseded where they differ.

Forward-carried from task-1: (1) The flake's checks build and run tests for x86_64-unknown-linux-musl (static). The dev shell's just check runs them on gnu. Syscall code is therefore exercised on both libcs. Use rustix with its default linux_raw backend for openat2/statx/statvfs/flock (no libc wrappers). That works the same under static musl. (2) No musl C cross toolchain is configured in the crane derivations. Avoid deps with C code (cc/bindgen). If one is unavoidable, the flake needs a musl stdenv (pkgsCross.musl64 or pkgsStatic). (3) Source filtering: only .rs/.toml under crates/ plus the root Cargo.toml/Cargo.lock are build inputs. Test fixtures in other formats (e.g. .yaml, mountinfo samples) must be added to workspaceFiles in flake.nix, or they are missing in nix flake check while working in the dev shell.

Forward-carried from task-2: (1) Container tests: add a fn plus an entry (name, tier fast|full) to TESTS in diskgc-e2e/src/suite.rs. Build fixtures only through ctx.scratch (Scratch ops in diskgc-e2e/src/scratch.rs abort with exit 86 when a path is outside the /tmp scratch tmpfs, checked by statx mount id). Run code under test with ctx.run_as_tester (strace, uid 1000, no caps) and judge it with oracle::Oracle::arm(Fixture::new().eligible(..)) then judge(&run)?.assert_clean(). (2) Kernel facts seen in the container on this host (kernel 7.2): statx reports STATX_MNT_ID, STATX_ATTR_MOUNT_ROOT and btime on tmpfs. A bind mount of the scratch tmpfs gets a new mount id with the same device; interlock_aborts_outside_scratch proves it. The overlay root keeps inode numbers across copy-up but changes btime. (3) rustix 1.1.5 (linux_raw; features fs, mount, process, thread) builds and runs under static musl with no C toolchain. diskgc-e2e already locks it. When diskgc-linux gains rustix, refresh diskgc-e2e/Cargo.lock too (cargo update --manifest-path diskgc-e2e/Cargo.toml); the flake builds with --locked. (4) Container root has CAP_SYS_ADMIN and CAP_SYS_PTRACE inside the user namespace; the code under test has neither.

Forward-carried from task-59: container tests get tmpfs at /tmp, /run/diskgc (host lock 0644 root, provisioned per test) and /home/tester. Scratch::mount(root, mode, owner) mounts more tmpfs (nested mounts are fine), and Scratch::bind_mount_read_only makes a ro bind for mount-traversal fixtures. The snapshot skips read-only mounts by statx mount id but walks writable mounts nested below them (snapshot::plan_walk), so ro-bind fixtures do not blind the oracle where something can be deleted. The runner and each container require Podman's native overlay root (fuse-overlayfs is refused). Every traced run must be judged by the oracle (Ctx::teardown).

task-3 implementation (implementer, 2026-10-17). Not committed; changes staged.

Design: the Host trait is in diskgc-core/src/host.rs. It is syscall-shaped (mountinfo, openat2, statx, statvfs, flock_try, flock_wait(deadline), now, sleep, kernel_release, and the pids/proc_fds/proc_link/proc_maps/proc_environ readers). Path traversal (mount_for, resolve, open_beneath) and the kernel minimum (kernel.rs) are pure core logic over the port, so ModelHost can check flags and call order. LinuxHost is in diskgc-linux (host.rs, stat.rs, mountinfo.rs, procfs.rs) over rustix linux_raw. The port has NO unlock operation: a lock lives exactly as long as its Fd, so LOCK_UN cannot be expressed. Stat fields are Option, filled only when the stx_mask or stx_attributes_mask bit is set; that conversion is the LinuxHost boundary. Deleting ops are not in the port yet (task-5 adds them as separate methods; task-50 gates them).

Gotchas:
- Path::components silently drops interior '.' segments. '.' and '..' are therefore checked on the raw bytes.
- With O_PATH|O_NOFOLLOW, openat2 + RESOLVE_NO_SYMLINKS returns an fd TO a trailing symlink (openat2(2)). The beneath step omits O_NOFOLLOW so the kernel refuses with ELOOP.
- Without RESOLVE_NO_XDEV, RESOLVE_BENEATH crosses into a bind mount below the dir. Absolute symlinks under BENEATH give EXDEV, not ELOOP.
- A mount shadowed by a later mount over its parent stays in mountinfo. The step-2 statx confirmation catches it: the container shows 'mount point is mount id 968 (mount root: false), mountinfo says 967'.
- personality(UNAME26) simulates an old kernel with no product hook: uname reports 2.6.(60+minor), '2.6.62' on 7.2. Podman's default seccomp allows exactly 0x20000. rustix has no personality(), so diskgc-e2e uses the libc crate (bindings only, no C toolchain) for that one call: diskgc-e2e uname-2.6 -- PROGRAM.
- procfs answers no birth time (STATX_BTIME absent from stx_mask), which gives a real-kernel absent-field case. MOUNT_ROOT is answered on every fs on 5.8+, so an unanswerable mount-root can be tested only with simulated masks.
- /proc/locks prints dev as hex major:minor plus a decimal inode.
- rustix Statx is #[non_exhaustive]: tests take a real statx and edit its masks.
- as-tester then exec leaves /proc/<pid> readable by tester (exec without a credential change resets dumpable). The harness is pid 1 in the container, and tester gets EACCES on its /proc entries.

Evidence per AC (container tests are in diskgc-e2e/src/host_suite.rs, all tier fast, all oracle-judged clean):
AC1: host_resolve_refuses_symlinks covers symlinks as the first component, in the middle, last, dangling, and the /home->/var/home layout onto another mount. Each gives unsafe_path/ELOOP; controls resolve. host_resolve_refuses_mount_crossings: a bind mount of the same tmpfs below /tmp/root (same dev 0:98, new mount id) is refused, EXDEV -> unsafe_path. Also covered: a path ON the bind mount resolves on it; stacked mounts give the topmost; a shadowed mount point gives unsafe_path mount_mismatch; an escaped mount point (\040 \011 \134) is found. Core unit tests: resolve::tests (flags, '.' for the mount point, errno mapping, deepest prefix, stacked/ambiguous).
AC2: Core unit test mount_id_or_mount_root_not_reported_is_unsupported_fs (simulated masks give unsupported_fs). diskgc-linux stat::tests (an attribute or field without its mask bit becomes None). Container test host_statx_and_statvfs_on_a_real_kernel: procfs btime=none, verified independently by a raw harness statx; tmpfs mount_root true/false; statvfs equals the harness's f_bavail*f_frsize.
AC3: kernel::tests (2.6.62, 5.7.19, 5.4 and others refused; garbage refused). host_kernel_older_than_5_8_exits_2: under UNAME26, diskgc ensure exits 2 with 'needs Linux 5.8 or newer'; --version is exempt; without the personality the same command line gives the usage error.
AC4: diskgc-linux depends only on diskgc-core plus rustix (linux-raw-sys, bitflags). There is no libc in the tree of the diskgc crate. Tests run on gnu (just check) and on static musl (nix flake check and every container test).
AC5: host_flock_release_only_by_close checks: held while open; released on close; a child that inherited the fd keeps the lock after the parent closes; released after the last close; try=busy while the harness holds it; wait(300ms) gives up at the deadline; a lock released mid-wait is acquired. host_proc_readers checks that fd/cwd/root/exe/maps dev:ino match the harness's statx and that the initial environ is parsed (split at the first '='); another user's process gives denied, an exited one gives gone. diskgc-linux host::tests do the same on the own process, plus a flock deadline test.

Bite proofs (one-off mutated build, then the tree was restored from the index):
- M1 RESOLVE_NO_SYMLINKS off: host_resolve_refuses_symlinks FAIL ('/tmp/root/mid/b' resolved ok). The core flags test and diskgc-linux resolves_the_root_and_own_proc_dir also FAIL (/proc/self was followed).
- M2 RESOLVE_NO_XDEV off: host_resolve_refuses_mount_crossings FAIL. 'sub/f' was refused only by the post-open mount-id check (left_mount) instead of EXDEV, so that second check is a real backstop. The core flags test also FAILs.
- M3 step-2 confirmation off: core a_mount_point_that_is_not_that_mount_is_refused FAIL. The container test FAILs (the shadowed path is refused only by the final mount-id check).
- M4 kernel check removed from main: host_kernel_older_than_5_8_exits_2 FAIL.
- M5 flock(LOCK_UN) in a Drop of LinuxFd: host_flock_release_only_by_close FAIL with held_after_parent_close=no. The single-fd unit test does not catch this.
- M6 stx_attributes_mask ignored: stat::an_attribute_is_ignored_unless_its_mask_bit_is_set FAIL. The container statx test does NOT catch it (this kernel always answers MOUNT_ROOT), so the mask handling is proven by unit tests only.

Docs: safety.md Path traversal now covers the topmost rule, step 2 with NO_SYMLINKS and refusal on mismatch, no O_NOFOLLOW in step 3, the post-open mount-id check, dot segments, paths ON a bind mount, per-field and per-attribute mask rules, and the kernel-check scope and limits. safety.md Host lock now covers the polling wait and its limits. architecture.md Host port: LOCK_UN is unrepresentable, and statx fields are optional. testing.md mutation scope: resolution lives in diskgc-core.

Deep-gate fixes (NO-GO from mped-architect plus codex; re-review pending; not committed, all staged):
1. /proc readers now cover every task. New module crates/diskgc-linux/src/tasks.rs holds the pure logic: collect(), task_exited() and parse_task_stat() (the state letter and PF_KTHREAD, read after the last ')'). proc_fds and proc_links(pid, which) read /proc/<pid>/task/<tid>/{fd,cwd,root,exe} for every task and de-duplicate the results. proc_maps and proc_environ read through the first task that is still alive after the read. Gone is returned only when no listed task is alive. A live task whose read gives ENOENT is Failed (conservative), except kernel threads, which hold nothing. The port method proc_link became proc_links -> Vec<LinkTarget>. Tests: container tests host_proc_readers_see_threads_after_main_exits (pthread_exit; the harness confirms the leader's fd listing is empty and its cwd gives ENOENT) and host_proc_readers_see_threads_with_own_fd_table (unshare CLONE_FILES|CLONE_FS), plus unit tests tasks::tests and host::tests::a_thread_with_its_own_fd_table_and_cwd_is_read (in-process, also runs under musl in the nix sandbox). safety.md 'In use' has a Threads paragraph.
2. proc_alive() is removed. Liveness is task_exited(&Result<TaskStat, HostError>): Z/X/x or ENOENT/ESRCH counts as exited; any other error propagates (Denied or Failed), never Gone. An fd that vanishes mid-listing is just skipped; no liveness guess is involved. Unit tests: only_a_confirmed_end_counts_as_exited, a_stat_error_propagates_instead_of_meaning_exited.
3. Locks: flock_try and flock_wait take the Fd by value. They return TryLock<F>::{Acquired(Locked<F>), Busy(F)} and WaitLock<F>::{Acquired(Locked<F>), DeadlinePassed(F)}. Locked lends the fd out by reference only and closes it on drop, so re-locking, converting or unlocking a held lock does not compile. Proofs: a compile_fail,E0308 doctest on Locked with a positive-control doctest; the unit test a_held_shared_lock_stays_held; and the kernel-fact test a_failed_flock_conversion_drops_the_shared_lock_kernel_fact, which pins why. architecture.md (Host port) and safety.md (Host lock) are updated.
Bite proofs (mutate, run, restore):
- B1, reading only the leader's task (the old /proc/<pid> view): both new container tests FAIL ('the worker's open file 0:98:3 was not found'), and the unshare unit test FAILs.
- B2, treating any stat error as exited (the old proc_alive semantics): two tasks:: unit tests FAIL.
- B3, Locked::fd handing the fd out by value: the compile_fail doctest FAILs ('Test compiled successfully, but it's marked compile_fail').
Gates after the fixes: nix flake check exit 0 (19 s; musl tests: 14 core, 33 linux, 2 doctests, 4 binary; e2e harness unit tests 34). just check exit 0. just e2e exit 0, 19/19 (2.6 s). just e2e-full exit 0, 20/20 (8.1 s).
Limit: threads started during the scan are not seen (stated in safety.md).

Second deep-gate round (re-review pending; not committed, staged):
1. Gone from an incomplete task listing. tasks.rs::collect now takes a list() closure and re-lists after the reads. Tasks not yet read are read and the listing repeated, up to MAX_LISTINGS=8, then Failed. Tasks that only disappeared need no further listing (a stricter 'two equal listings' rule gave Failed under normal thread churn: a stress run showed 17/180 failures). Gone requires that /proc/<pid> has vanished, or that two consecutive equal listings show only exited tasks. EveryTask keeps results of tasks that exit mid-scan. Tests: a_task_missing_from_the_first_listing_is_read, results_of_tasks_that_exit_during_the_scan_are_kept, gone_only_when_proc_entry_vanished_or_every_listed_task_exited, a_task_list_that_never_settles_is_failed.
2. PF_EXITING (0x4) is parsed into TaskStat.exiting, giving Status::{Live, KernelThread, Exiting, Exited}. An exiting task is never the source for FirstLiveTask (maps, environ); in EveryTask its ENOENT is not a failure and its Ok results are kept; if no task is live and some are exiting, the result is Failed (not Gone). Unit tests: an_exiting_task_is_never_the_source_of_the_memory_map, an_exiting_task_that_cannot_be_read_is_not_a_failure, every_task_exiting_is_failed_not_gone. Container regression host_proc_maps_while_the_leader_exits_slowly: the worker unshares CLONE_FILES, the leader opens 400,000 /dev/null files in its own table, then pthread_exit. A tester probe calls proc_maps in a loop for 4 s. One run: calls=102575, with the mapping=102575, without=0, gone=0, leader seen exiting in 2632 calls (the window is exercised).
3. Test isolation. A process-wide FD_TABLE mutex in the host tests serialises the fd-table-copying tests (spawn in a_process_that_is_gone_is_gone; unshare in a_thread_with_its_own_fd_table_and_cwd_is_read) with the three flock tests. Stability, diskgc-linux unit-test binary run 60 times in parallel: before the fixes 25/60 failed (19 own_process_is_readable [PF_EXITING test threads], 10 unshare test, 2+1 flock). After the fixes, 300/300 passed (5 rounds of 60).
Bite proofs (mutate, run, restore):
- B4, no second listing: a_task_missing_from_the_first_listing_is_read and a_task_list_that_never_settles FAIL.
- B5, results of exiting tasks dropped: results_of_tasks_that_exit_during_the_scan_are_kept FAILs.
- B6, PF_EXITING ignored in the parser: the parse test FAILs, and the container regression FAILs with without=5464 of 104923 calls.
- B6b, exiting treated as live in task_status: four tasks:: tests FAIL.
- B7, no serialisation: 2/120 stress runs FAIL (a_lock_conflicts_until_its_fd_is_closed). That bite is statistical: the flake is a race.
Gates: nix flake check exit 0, 23 s; derivations really ran, the -L log shows workspace tests with 38 linux, 14 core, 2 doctests and 4 binary tests on musl, and e2e harness tests 34. just check exit 0. just e2e twice exit 0, 20/20 both times (6.3 s, 5.9 s). just e2e-full exit 0, 21/21 (5.9 s).
Limit (safety.md In use, Threads): a thread missing from every listing is not seen.

Third deep-gate round (re-review pending; not committed, staged):
- B1. A listing is trusted only when every tid in it is still in the next listing (no task of it was released, so readdir did not cut it short) and every tid of the next listing has been read. New tids just trigger another round. The 'no unread tid' shortcut and the separate equal-listings check for Gone are both gone.
- B2. Before any verdict, tids recorded as Exited are stat'ed again; a tid no longer exited (taken over by a non-leader exec) counts as unread.
- B3 (Codex hypothesis, real). starttime does NOT tell the two tasks apart: de_thread copies start_time/start_boottime to the exec'ing thread. Verified on the host with a Python non-leader exec: the worker's own starttime was 46151579; after its exec, tid==pid showed the old leader's 46151577. The approach taken instead: a read of the exiting old leader finds nothing, because its mm, files and fs pointers are cleared (empty result or ENOENT), and de_thread swaps tids only after the leader is a zombie. So a read that found nothing from a task whose following stat shows Live is read once more after that stat. And when a tid vanishes between listings, the leader tid (pid) counts as unread again (the exec'ing thread's own fd table is then visible only through it).
- collect is restructured: collect(pid, list, Tasks{read, is_empty, stat}, want); a read_task helper does the read, stat and one retry; verdict() is pure. MAX_LISTINGS=16 counts every listing, the first included, and a unit test pins the exact count.
- QA flake. own_process_is_readable is replaced by a_process_is_read_through_its_tasks, and the unshare test now reads a Holder: this test binary re-run as an ignored helper test, holder_process, with a fixed thread set. The old test read the churning test process itself.
Bite proofs:
- B1, a_listing_cut_short_twice_is_not_trusted: FAILed on the round-2 code with Ok(["other","other"]).
- B2, a_tid_taken_over_by_exec_is_not_gone: FAILed with Err(Gone).
- B3: an_empty_read_from_a_live_task_is_read_again, a_missing_entry_of_a_live_task_is_read_again and the_leader_is_read_again_when_a_tid_vanishes all FAILed on the pre-B3 code.
- QA flake: the old own-process test failed 1 of 1440 runs ('task list did not settle within 16 listings'). That bite is statistical.
Stability: diskgc-linux unit binary, 660 parallel runs (11 x 60), 660 pass, 0 fail, each with 43 tests.
Gates: nix flake check exit 0 (26 s; tests ran: 43 linux and 14 core on musl, e2e harness tests 34). just check exit 0. just e2e twice exit 0, 20/20 both times. just e2e-full exit 0, 21/21.
Gotcha (my process): 'git checkout -- <file>' to undo a mutation restores the INDEX version. Stage before any mutation-and-restore, or unstaged work is lost (it happened once this round; I re-applied it).

Fourth deep-gate round (re-review pending; not committed, staged):
- A (exec window). After exchange_tids, de_thread unlocks tasklist_lock before release_task(old leader), so the old leader stays listed under the exec'ing thread's old tid, dead (X). collect now also re-reads the leader when a non-leader task is first found Exited in a round (in addition to the vanished-tid trigger). The removal happens before trust is computed. Test: the_leader_is_read_again_when_a_listed_thread_is_found_dead (mped's probe b3_old_leader_still_listed_after_swap). On the round-3 code it FAILed with Ok(["old leader's table", ""]).
- B (double-cut listings). Readdir can stop at a task released before it was emitted, so 'still in the next listing' proves nothing alone. Listing now carries the Threads: count (signal->nr_threads) read before and after the readdir. A listing is trusted only if both counts are equal and equal to its distinct tid count, it is kept in the next listing, and the next listing is fully read. The counting argument: L is a subset of the tasks alive at the second count, has exactly that many tasks, so it equals them; a thread alive throughout is in it (assuming no tid reuse). Tests: a_listing_shorter_than_the_thread_count_is_not_trusted (mped's b1_unemitted_release_twice) and short_listings_of_a_zombie_leader_are_not_gone (Codex trace). On the round-3 code they FAILed with Ok(["shared","shared"]) and Err(Gone).
- Kernel sources checked: linux 7.2.10 from nixpkgs; the host runs 7.2.9.
  (1) kernel/fork.c copy_process: under write_lock_irq(tasklist_lock) and siglock it does current->signal->nr_threads++, list_add_tail_rcu(thread_node) and attach_pid(PIDTYPE_PID). kernel/exit.c release_task takes write_lock_irq(tasklist_lock); __exit_signal, under siglock, does sig->nr_threads-- then __unhash_process: detach_pid(PIDTYPE_PID) and list_del_rcu(thread_node), both unconditional.
  (2) fs/proc/array.c task_sig prints 'Threads:' from get_nr_threads() when lock_task_sighand succeeds. A zombie leader keeps sighand until release_task/__exit_signal, so it reports.
  (3) fs/exec.c de_thread: exchange_tids and leader->exit_state = EXIT_DEAD under tasklist_lock, write_unlock_irq, then release_task(leader). The old leader stays on the thread list and counted until then. Also fs/proc/base.c: proc_task_readdir skips tasks whose pid is detached (task_pid_nr_ns == 0), next_tid stops on !pid_alive, and first_tid resumes by offset. That confirms both Codex's premise and the need for the count.
- Bite proofs on the new code (mutate, run, restore): MA, dropping the exited-thread trigger, FAILs the A test. MB, ignoring the counts, FAILs both B tests.
- Minors: the trust premise and limit are restated in the tasks.rs module doc and safety.md (Threads). The 'second exec' residual is folded into the limit: a thread started after the last trusted listing is not read, also when it then execs. The tid-reuse assumption is stated. Holder stderr is now inherited. The FD_TABLE comment is updated. Listing membership uses binary_search. The final summary is rewritten without counts.
- Gates: nix flake check exit 0, plus nix build --rebuild on workspace-tests and e2e-tests, which re-ran them (the new tests are visible by name in the log). just check exit 0. just e2e twice exit 0, 20/20 both times. just e2e-full exit 0, 21/21. Stability: 660 parallel runs of the diskgc-linux unit binary, 660 pass, 0 fail.

Round 5 (deep gate round 5; re-review pending; not committed, staged). CURRENT RULES of the per-task /proc readers (tasks.rs collect):
- Listing = sorted, distinct tids plus Threads: (signal->nr_threads) read right AFTER the readdir; the before-count was dropped. counted = threads > 0 and tids.len() == threads.
- Trusted listing: counted, every tid still in the next listing (kept), and every tid of the next listing read. Claim: a trusted listing holds every task alive at its count (no tid reuse assumed).
- Unread are: unread tids; Unsettled tasks (a live userspace task whose read found nothing, an empty result or ENOENT, twice); a leader whose stat says ENOENT while the pid is still in /proc; the leader again when a non-leader task is found ended or a tid vanished between listings.
- ENOENT from the listing, status or the leader's stat is never Gone by itself. Gone = the pid is absent from a fresh /proc readdir (LinuxHost proc_pids, Process.listed) or every task of a trusted listing ended (Exited or Vanished).
- Before the verdict, ended tids are re-stat'ed; one no longer ended (or the leader vanished with the pid still listed) is unread.
- FirstLiveTask (maps, environ) returns the first read from a Live or KernelThread task that found something. Exiting tasks are never a source. An empty maps, environ or fd table of a live userspace task is never the answer.
- Bounded at MAX_LISTINGS listings, then Failed.
Round-5 blockers:
- (1) Threads: 0 (task_sig without a sighand). Test: a_zero_thread_count_is_never_trusted; on round-4 code Err(Gone).
- (2) Cold-lookup ENOENT during a non-leader exec. Tests: enoent_from_a_listing_with_the_pid_still_in_proc_is_not_gone (round 4: Err(Gone)) and enoent_from_the_leaders_stat_is_gone_only_if_absent_from_proc (regression).
- (3) maps bound to the dead old leader. Test: an_empty_memory_map_of_a_live_task_is_never_the_answer; round 4 gave Ok([""]).
Kernel facts for 2(b), from linux 7.2.10 source:
- fs/proc/base.c next_tgid iterates by pid number via find_ge_pid and takes pid_task(pid, PIDTYPE_TGID).
- proc_pid_readdir emits the name through proc_fill_cache, which reaches dir_emit even when instantiation fails (end_instantiate).
- fs/exec.c de_thread does exchange_tids then transfer_pid(leader, tsk, PIDTYPE_TGID) under write_lock_irq(tasklist_lock); kernel/pid.c transfer_pid is hlist_replace_rcu, so an RCU reader always finds a task on P.
Environ decision: an empty environ from a live task is not authoritative; test an_empty_environment_is_never_the_answer. The proposed alternative, 'accept empty environ if maps read after it is non-empty', is unsound: environ and maps are separate child lookups, so environ can be bound to the dead old leader while maps binds to the live exec'ing thread. The cost: a same-user process with a really empty environment (env -i) or no fds at all is Failed, so the caller treats it as unreadable and its candidates cost reclaim while it runs (forward-carried to task-6 and task-9).
Bites on the new code (mutate, run, restore):
- M1, dropping threads>0: the zero-count test FAILs.
- M2, taking a listing's ENOENT as Gone: the ENOENT test FAILs.
- M3, accepting the second empty read: the maps, environ, gone and uninspectable tests FAIL.
Minors: the complete()/listings() helpers are simplified and their docs fixed. The tasks.rs module doc and safety.md Threads are restated ('alive at the count'; ENOENT; empty reads; cost).
Gates: nix flake check exit 0, plus nix build --rebuild on workspace-tests, e2e-tests, workspace-clippy and e2e-clippy, all of which re-ran (the tests show by name; clippy shows Checking/Finished). just check exit 0. just e2e twice exit 0 (20/20 each). just e2e-full exit 0 (21/21). Stability: 660 parallel runs, 660 pass, 0 fail, each with all unit tests.

Round 6 (deep gate round 6; re-review pending; not committed, staged). CURRENT RULES (supersede round 5):
- Statuses: Live, KernelThread, Exiting (PF_EXITING), Exited (Z only), Dead (X), Vanished (stat ENOENT/ESRCH). Ended = Exited, or a Vanished non-leader. Usable source = Live or KernelThread.
- judge(tid, status) is the one place that decides 'not settled': Dead is never settled (unread); a leader that Vanished is unread while the pid is still in a fresh /proc readdir, else Gone. gone_unless_listed() is the only caller of listed().
- Listing = sorted distinct tids plus Threads: read right after. counted = threads > 0 and len == threads. Trusted = counted, kept (every tid in the next listing), and every tid of the next listing read and settled. A vanished tid (not kept) makes the leader unread. Before the verdict, ended tids are judged again from a fresh stat.
- Gone = every task of a trusted listing ended, or a lookup ENOENT with the pid absent from /proc. Otherwise bounded (MAX_LISTINGS), then Failed.
- Per task: found nothing (empty or ENOENT) from a Live task, read again once, then Unsettled (unread). Exiting tasks are never a FirstLiveTask source.
- Complete reads (host.rs, using procfs.rs helpers):
  - environ: one read() call (read_in_one_call; a full buffer means reopen with double size, bounded).
  - maps: read to EOF, then lseek(0) and read again on the same fd (read_confirmed); nothing there means the content is discarded as empty.
  - fd dir: one getdents call confirmed by a second returning 0 (list_in_one_call over RawDir).
  - A listed fd whose link says ENOENT is checked against a fresh listing; still listed and still ENOENT makes the task read ENOENT, so found-nothing.
Rules deleted: the round-4 'non-leader task found ended re-reads the leader' trigger (found_ended_thread). Dead now being never settled, plus the vanished-tid trigger, covers it: the A test still passes, and with Dead settled (mutation MD) the A test FAILs. The separate settle() is folded into collect's list closure plus gone_unless_listed. collect is 87 lines before, 80 after (-21/+14).
Kernel citations, linux 7.2.10:
- X is transient. kernel/exit.c: exit_notify autoreap sets EXIT_DEAD and release_task()s the dead list; reparent_leader and exit_ptrace (kernel/ptrace.c __ptrace_detach) add to the same dead list; wait_task_zombie does cmpxchg ZOMBIE->DEAD and then release_task; release_task's zap_leader does 'goto repeat'. fs/exec.c de_thread sets EXIT_DEAD, then release_task(leader).
- Exception: make_task_dead's recursive-fault path leaves a task in EXIT_DEAD until reboot; such a process yields Failed, which is safe.
- Dentry revalidation: fs/proc/base.c pid_revalidate keeps a dentry iff pid_task(proc_pid(inode), PIDTYPE_PID) is non-NULL, so until release detaches the pid. fs/proc/fd.c tid_fd_revalidate drops an fd dentry as soon as tid_fd_mode() fails (the dead task's files are NULL).
- environ_read (base.c): one mmget_not_zero per call and env_start/env_end read once; it loops over pages within the call; a failure mid-loop returns <=0, not a count. So 0 < n < buf means a complete snapshot.
- seq_read_iter (fs/seq_file.c): one start..stop per call, but a call fills at most m->size (PAGE_SIZE unless one record overflows), so maps CANNOT be read in one call. maps m_start (fs/proc/task_mmu.c) uses the mm captured at open and returns NULL once mmget_not_zero fails; mm users never come back from zero, hence the re-read-from-start confirmation.
- proc_readfd_common (fs/proc/fd.c): get_proc_task once per getdents call. A per-fd stat through a dentry bound to the dead old leader gives ENOENT (proc_fd_link with NULL files), never another task's file.
Bites:
- On round-5 code, a_dead_task_is_never_final gave Err(Gone). (a_dead_leader_entry_is_read_again_once_released is a regression guard and passes on both.)
- B2 helpers (new; round 5 used std::fs::read, which has no seam). Mutations: accepting a full buffer (MP1), skipping the confirmation (MP2) and accepting a two-call listing (MP3) each FAIL their procfs:: test. Codex reproduced the underlying kernel prefix+EOF behaviour directly.
Doc fixes: the task_listing doc (after only); the task_ids doc (ENOENT is decided by collect); safety.md 'Unreadable or vanishing processes' (ENOENT alone is not proof); safety.md Threads (X never final, complete reads, trigger list).
Gates: nix flake check exit 0, and nix build --rebuild on workspace-tests, e2e-tests, workspace-clippy and e2e-clippy all re-ran. just check exit 0. just e2e twice exit 0 (20/20 each). just e2e-full exit 0 (21/21). The slow-exit container test via read_confirmed found the mapping in every call (calls=43045 with=43045). Stability: 660 parallel runs, 660 pass, 0 fail.

Round 7 (deep gate round 7; re-review pending; not committed, staged). CURRENT RULES (supersede round 6):
- Statuses: Live, KernelThread, Exiting, Exited (Z), Dead (X), Vanished. Ended = Exited, or a Vanished non-leader. Usable source = Live or KernelThread. Dead is never final: neither ended nor usable.
- read_task: read, then stat. Found nothing (empty or ENOENT) from a Live task means Unsettled (unread, re-read in a later round). There is no in-task second read any more.
- judge: a leader that Vanished is unread while the pid is listed in /proc, else Gone; all other statuses are recorded. gone_unless_listed() is the only caller of listed().
- Leader re-read (the leader is removed from seen before trust is computed) when a non-leader is first seen ended or Dead (Z, X, vanished) in a round, OR a tid vanished between listings (not kept).
- Trusted = counted (threads > 0 and len == threads), kept, and every tid of the next listing seen. Before the verdict, ended tids are judged again from a fresh stat. Gone = every task of a trusted listing ended, or ENOENT with the pid absent from /proc.
- Reads (host.rs and procfs.rs): environ in one read() call (read_in_one_call); maps in TWO passes on separate opens, each read to EOF and confirmed by a re-read from offset 0, union of lines (read_two_passes over read_confirmed); fd dir in one getdents confirmed by an EOF second call (list_in_one_call). An fd link ENOENT is simply skipped.
FIX 1 (Codex 1): kernel check, fs/proc/task_mmu.c 7.2.9. proc_get_vma sets *ppos = vma->vm_end of the last emitted VMA, and m_start resumes vma_iter_init(mm, last_addr), so a VMA moved by mremap from above that address to below it between two read() calls is skipped. Two passes take the union; a mapping moved again in step with the second pass needs a second event (documented limit). Test: procfs::a_record_moved_during_one_pass_is_found_by_the_other. Bite: round 6 had no seam (single pass), so I used mutation MU (first pass only), which FAILs it.
FIX 2 (Codex 2 + mped 1): fs/exec.c de_thread runs exchange_tids before leader->exit_state = EXIT_DEAD, both inside one write_lock_irq(&tasklist_lock) section, which /proc readers on other CPUs do not take; that is the Z window. The leader re-read on a first-seen ended or Dead non-leader is restored. Tests:
- a_thread_seen_as_a_zombie_makes_the_leader_be_read_again (Codex's Live-then-Z). Round 6 gave Ok(["old leader's data", ""]).
- ended_tasks_are_checked_again_before_gone (pins the pre-verdict recheck). Round 6 gave Err(Gone).
- a_tid_taken_over_by_exec_is_not_gone, rewritten with pid 10 (mped's zombie_window_under_swapped_tid).
- a_vanished_unread_thread_makes_the_leader_be_read_again (pins the vanished-tid trigger).
- The run() helper now uses the real leader pid. The exposed assertion (leader ESRCH/ENOENT with the pid listed) is Failed.
Deleted:
(1) The readlink-ENOENT recheck in task_fds (mped ML).
(2) The in-task second read in read_task (mped MS). Measured with QA's churn probe on proc_fds of a churning process: without the retry ok=67/failed=933 (k=4, 200us) and ok=3/failed=997 (k=8, 50us); with the retry ok=11/failed=989 and ok=4/failed=996. That is no measurable benefit. Round-6 code on the same probe: failed=1000 in both settings.
(3) 'Dead is unread' (judge's Dead -> None). It is redundant once the trigger is restored (the first sighting of X re-reads the leader; a recorded Dead is neither ended nor usable), and unpinnable: mped's MD survived every test.
Mutants on the final code: MR FAILs ended_tasks_are_checked_again_before_gone. MK (no vanished trigger) FAILs a_vanished_unread_thread_makes_the_leader_be_read_again. MT (no ended-thread trigger) FAILs 3 tests. MU FAILs the two-pass test. MD, MS and ML are moot (deleted).
Docs: the module doc names the Z window as the reason for the re-read and the recheck; it documents X never final, two-pass maps, and fd_numbers as 'one task' (exit_files prefix caught by PF_EXITING, set before exit_files). safety.md Threads is reordered (the gone rule before the read-in-full sentence) and names the Z window. It gains limits for the mapping moved in step with both passes and for Codex 3 (privileged reader, coincidence of events).
Gates: nix flake check exit 0, and nix build --rebuild on workspace-tests, e2e-tests, workspace-clippy and e2e-clippy all re-ran. just check exit 0. just e2e twice exit 0 (20/20 each). just e2e-full exit 0 (21/21). Slow-exit container maps watch: calls=17522 with=17522 without=0. Stability: 660 parallel runs, 660 pass, 0 fail.

Round 8 (deep gate round 8: restructure; re-review pending; not committed, staged). CURRENT RULES (supersede everything above):
- Rounds are self-contained: one Listing, a read of every listed task including the leader's tid (nothing carried over between rounds), and a decision. Bounded at MAX_ROUNDS=16, then Failed with the last round's reason ('listing not complete (N tasks, C counted)', 'task T gave no trustworthy read', 'no task of a complete listing was live', 'its task listing vanished').
- Listing (mped R9) = readdir, Threads:, readdir again. Listing::new(tids, threads, &again) takes the second listing as a required argument. complete = confirmed (every tid also in the second listing) and threads > 0 and len == threads. Claim: the listing is exactly the tasks alive at the count.
- Per task (observe): read, then stat. Untrusted (no status): a Live task whose read found nothing (empty or ENOENT), and the leader's own entry vanished. FirstLiveTask returns the first trusted usable read.
- In-round leader re-read: if a non-leader is seen ended (Z, X or vanished), the leader's tid is read again after every other read of the round.
- Answer: a complete listing, every task trusted, and one usable (Live or KernelThread). If no task is usable, check exited(): Gone if so, else another round.
- GONE comes ONLY from the process itself through a pidfd: pidfd_open ESRCH (in host.rs open_pidfd), or poll(pidfd, POLLIN) readable (Process.exited), asked at the start, after a round with nothing live, and when the listing vanished. Listings and ENOENT never decide Gone.
Gone mechanism and citations (linux 7.2.9). I used pidfd poll instead of G1 (pidfd_send_signal 0) plus G2 (status Z with Threads 1): one direct fact covering both, and rustix's Signal cannot be 0.
- fs/pidfs.c pidfd_poll: task = pid_task(pid, PIDTYPE_PID); !task means EPOLLIN|EPOLLHUP (released); task->exit_state && !delay_group_leader(task) means EPOLLIN (the leader exited with no thread left: a zombie group).
- include/linux/sched/signal.h: delay_group_leader(p) = thread_group_leader(p) && !thread_group_empty(p); thread_group_empty = the leader is the last entry on signal->thread_head. Thread-list removal happens only at release (kernel/exit.c __unhash_process), so ptraced unreleased zombie non-leaders keep the group non-empty: not exited, so Failed.
- Non-leader exec: before exchange_tids, pid P's task is the zombie old leader with the survivor still on the list (delay_group_leader true, so not readable); after it, the survivor (exit_state 0). The kernel comment in pidfd_poll states exactly this case.
- kernel/pid.c pidfd_open -> kernel/fork.c pidfd_prepare: ESRCH when !pid_has_task(pid, PIDTYPE_PID) (reaped); ENOENT when the pid is not a thread-group leader. There is no permission check, so no EPERM path. A pidfd is not procfs, so hidepid is irrelevant, and it pins struct pid, so pid reuse is irrelevant.
Deleted: judge(), gone_unless_listed() and its /proc readdir; Process.listed; verdict(); the cross-round 'seen' map; the kept/next-listing logic; the vanished-tid-between-listings trigger (the leader is read every round); the pre-verdict recheck of ended tids (Gone no longer comes from statuses; an ended task is neither a source nor proof). collect is 86 lines (82 before); non-test tasks.rs is 434 lines (492 before), with a longer module doc.
Bites:
- On round-7 code (exported index; old-API equivalents): leader_read_carried_across_an_unseen_exec gave Ok(["old leader's table"]); Codex 1 (zombie window held open), Codex 2 (later count of 2) and Codex 3 (invisible in /proc) each gave Err(Gone).
- Mutations on the new code: MI (no in-round leader re-read) FAILs 2 tests. MC1, MC2 and MC3 (each listing-completeness condition dropped) FAIL 2, 1 and 5 tests. MG (no exited check after a dead round) FAILs gone_only_when_the_process_says_it_exited. Wiring: W1 (environ fixed read) FAILs 3; W2 (maps single pass) FAILs 1; W3 (maps unconfirmed) FAILs 1; W4 (fd listing accepted incomplete) FAILs 2, including the real-kernel big test.
Wiring pins (QA r8 #5): host.rs ProcFiles seam (Kernel in production, Scripted in tests) for read_environ, read_maps and fd_numbers, plus host::tests::a_big_process_is_read_completely: a Holder with a 100 KiB environment variable, 4000 fds and 3000 mappings; complete contents checked; it also runs under musl in the nix sandbox. fd_numbers uses Vec::with_capacity + spare_capacity_mut (QA r8 #6).
Release-build numbers (scratch probe, external churn process, 1000 reads each of fds, maps, environ and cwd):
- k=1/1000us, k=4/200us, k=8/50us: all ok, 0 failed, 0 gone.
- k=16/20us: fds 33 failed, cwd 38 failed (incomplete listing); maps and environ 0.
- k=32/5us: fds 7, cwd 6 failed.
- gone_but_alive = 0 everywhere.
Real cargo workload, release build of just from source, 292 distinct pids, 43 s: gone_but_alive=0 in every class. Failed: rustc fds 25/15676, rustc maps 23, cargo fds 1, 'other' 4, all 'no task of a complete listing was live' (exiting processes). No incomplete-listing failure for cargo or rustc.
Gates: nix flake check exit 0, and nix build --rebuild on workspace-tests, e2e-tests, workspace-clippy and e2e-clippy all re-ran. just check exit 0. just e2e twice exit 0 (20/20 each). just e2e-full exit 0 (21/21). Slow-exit maps watch: calls=51730 with=51730. Stability: 660 parallel runs, 660 pass, 0 fail.

Round 9 (fixes for the round-9 deep gate, reviewed by the round-10 gate; not committed, staged). CURRENT RULES for Gone (supersede the Gone part of round 8; the round rules are unchanged except as listed):
- Gone decision rule, pure, in tasks.rs has_exited(poll, status_after), used by collect through Process{pid, list, poll, status}: pidfd_open ESRCH is Gone (host.rs open_pidfd); poll POLLHUP is Gone; POLLIN is Gone only when one /proc/<pid>/status read made AFTER that poll shows State Z and Threads 1; anything else means another round. Asked at the start, after a round with nothing live, and when the listing vanished. The status read is only made after a readable poll.
- Why POLLIN alone was unsound (B1; the round-8 note's 'not readable before exchange_tids' was wrong): pidfd_poll loads the task under RCU, then tests exit_state && !delay_group_leader(task). A poll that loaded the old leader (zombie) of a non-leader exec just before exchange_tids, and read exit_signal after de_thread set leader->exit_signal = -1, sees thread_group_leader() false, so readable while the process lives.
- Why Z + Threads 1 in one status read is sound: proc_pid_status shows one task; task_state prints its state, then task_sig prints signal->nr_threads under lock_task_sighand (0 when the task's sighand is gone, i.e. it was released). nr_threads++ (copy_process) and nr_threads-- with sighand = NULL (__exit_signal) both happen under that siglock. So Threads 1 means the task shown is the only unreleased task of its group: none is live, and a lone zombie group never runs again. The old leader during an exec shows Z only with the exec'ing thread still counted (>= 2), and 0 once released.
- Memory ordering (arm64): I did not find mped's arm64 note in the material available to me (r9poll, r10tree, r10live/r10race, logs), so this argument is mine. The state load comes before lock_task_sighand in program order; it may be satisfied late on arm64 but not after the siglock release that ends task_sig (release orders all earlier accesses), and no task can join or leave the group while that lock is held. So the Z seen held at the latest while the count was 1, and Z only moves on to X. Reading the count before the state, outside one lock section, would be unsound (a lone live task could start a thread and exit in between).
- Kernel history (sources from the torvalds tags on GitHub): EPOLLHUP for a pid with no task first appears in v6.9 (fs/pidfs.c). v5.8 through v6.8 have pidfd_poll in kernel/fork.c (v5.8 inline, later thread_group_exited) with EPOLLIN only. There a process released mid-scan gives POLLIN, its status says ENOENT, so it is Failed, never a false Gone. v5.8 fs/proc/array.c has the same order (task_state, then task_sig under lock_task_sighand) and kernel/exit.c the same nr_threads--/sighand = NULL.
- Citations (linux 7.2.9): fs/pidfs.c pidfd_poll; include/linux/sched/signal.h thread_group_leader (exit_signal >= 0), delay_group_leader, get_nr_threads; fs/exec.c de_thread (exchange_tids, transfer_pid, exit_signal = -1, EXIT_DEAD, release_task after write_unlock_irq); include/linux/rculist.h hlists_swap_heads_rcu (never leaves a head empty, so POLLHUP means released); fs/proc/array.c proc_pid_status, task_state, task_sig; include/linux/sched.h task_state_index (READ_ONCE(__state), exit_state); kernel/signal.c lock_task_sighand; kernel/exit.c __exit_signal; kernel/fork.c copy_process; fs/proc/base.c ONE("status") single_open plus fs/seq_file.c seq_read_iter (one show per open, so one std::fs::read of status is one snapshot).
- Data (scratch release probe, new rule polled from 16 threads on 4 of mped's execloop targets for 70 s): about 2.7e9 polls, 2300 readable while alive (each a false Gone under the old rule), 0 Gone under the new rule, about 106k execs, 4/4 alive. A 30 s rerun logging the status of each readable poll: R/1, R/2, S/2, D/1 (the survivor), Z/2 (old leader still counted with the exec'ing thread), Z/0 (released old leader); never Z/1.
B2 (pid namespace) and QA 5 (pidfd availability): LinuxHost::new() returns Result. It requires readlink(/proc/self) == getpid() (ENOENT counts as a mismatch) and pidfd_open on its own pid to work. Errors are clear messages; main.rs startup exits 2; the e2e host probes fail with 'host setup: ...'. Tested with scripted values (check_proc_namespace, check_pidfd_open) and on the real kernel: the_host_is_refused_under_another_namespaces_proc runs pid 1 of a new pid namespace under the old /proc, with no privilege needed. The outer process makes a user namespace and maps uid 0 in pre_exec; the helper (root there) unshares only CLONE_NEWPID from its test thread. Two kernel facts forced this shape: after unshare(CLONE_NEWPID) copy_process refuses CLONE_THREAD, so libtest cannot start its test thread; and exec without a uid map drops the namespace capabilities. It passes in the nix build sandbox too.
S1: ProcError::from_host never gives Gone (ENOENT/ESRCH become Failed). A vanished listing step is an internal None (Process.list returns Option<Listing>; task_ids and group_status give None on ENOENT/ESRCH). The Gone doc now means 'the kernel said through the pidfd that there is no live task'. grep for Gone across the crates: the only producers are collect (via has_exited) and open_pidfd (ESRCH); everything else is tests and the e2e probes matching on it.
S3: completeness wording in the safety.md Threads text, the tasks.rs module doc and Listing::complete: every task of the first listing is in the second one, and their number equals the count. S4: results reset per round (only the answering round's reads are returned; test only_the_answering_rounds_reads_are_returned). S5: thread 20 in a_thread_seen_ended_makes_the_leader_be_read_again_in_the_round has vanished (de_thread kills it before the swap).
H2 done: pure shortfall(listing, seen) -> Option<Shortfall>; Status::Dead merged into Exited; the listing.has(pid) guard removed (Listing::has deleted); one early return for FirstLiveTask (the round's read closure reports 'done').
QA addendum: (1) task_listing takes list and count as a seam; a_task_listing_is_confirmed_by_listing_again_after_the_count scripts [10,11], 2, [10], and dropping the second listing fails it. (4) Messages: 'its task listing changed between reads' for an unconfirmed listing; 'task T is live but its environment was empty ...' (Tasks.what names the read: fd table, cwd/root/exe, memory map, environment). (6) Kernel history above. (3) task-6 and task-61 notes rewritten with no stale figures; the task-6 own-pid reason corrected.
Deleted: ProcError::from_host's Gone arm; Process.exited; Status::Dead; Listing::has and the has(pid) guard; thread_count and parse_status_threads (replaced by group_status and parse_status, which reads State and Threads from one read); the duplicate FirstLiveTask return; the LinuxHost Default derive.
Bites:
- One transient readable poll: the ported r10_one_transient_exited_is_gone (old API: exited true, then false), run on an export of the round-8 index, gave Err(Gone) for a live process. The new test one_transient_readable_poll_is_never_gone passes.
- Mutants on the new code (applied to the worktree, restored from the index):
  - poll answered as Running: FAILs an_unreaped_zombie_is_gone;
  - Readable treated as exited: FAILs exited_only_on_hangup_or_a_readable_poll_confirmed_by_status and one_transient_readable_poll_is_never_gone;
  - task_listing without its second listing: FAILs a_task_listing_is_confirmed_by_listing_again_after_the_count;
  - namespace check accepting any pid: FAILs the_host_refuses_what_its_readers_cannot_stand_on and the_host_is_refused_under_another_namespaces_proc.
- Not a bite: the real-kernel regression a_process_execing_from_a_thread_in_a_loop_is_never_gone gave no Gone on the round-8 code in 3 runs. The race is too rare (mped's logs: about one per 1.7M polls) for the test's tens of thousands of reads in 6 s, so it is a regression guard only.
Gotcha found by the stability run: the first version of that test's generation helper read /proc/<pid>/environ with std::fs::read (several read calls) and failed about 1 run in 10 under 60-way load. The cause: an exec between the calls mixes two images' memory, and the retries had no pause. Fixed by reading through host.proc_environ (one call, from a live task) with paced retries. The product reader already avoids this; only the test helper had it.
Gates (final code): nix flake check exit 0, and nix build --rebuild of workspace-tests, e2e-tests, workspace-clippy and e2e-clippy exit 0 (the namespace and exec-loop tests ran in the sandbox under musl). just check exit 0. just e2e twice exit 0 (20/20 each). just e2e-full exit 0 (21/21). Stability: 660 parallel runs, 660 pass.
Release figures (scratch probe; product code identical to the final code; only the test helper changed afterwards):
- Churn matrix, 1000 reads each of fds, maps, environ and cwd: gone_but_alive = 0 everywhere. No Failed at k=1/1000us, 4/200us or 8/50us. Under heavier churn some fds and cwd reads were Failed, mostly 'listing changed between reads': at 16/50us more often than at 16/20us or 32/5us (H3, task-61).
- Cargo workload (release build of just from source, 275 processes, 44 s): gone_but_alive = 0 in every class. Every Failed was a process that was exiting ('no task of a complete listing was live'), plus one 'leader's entry vanished'.
- mped's exec-loop poller against the new rule for 70 s: 0 false Gone (see Data above).
Known gaps, filed in task-61: on 5.8-6.8 a process released mid-scan is Failed (pidfd_send_signal(0) ESRCH could restore Gone; to verify); the pidfd_open call in LinuxHost::new is pinned only through check_pidfd_open with scripted errnos (removing the call passes every test).

Round 10 (fixes for the round-10 deep gate; re-review pending; not committed, staged). Codex and mped confirmed the Z + Threads 1 confirmation (mped: 1.6e9 polls of has_exited, no false Gone); one blocker, N1.
N1, the namespace check passed on equal numbers: with /proc of an ancestor pid namespace and diskgc at the same number in both, readlink(/proc/self) == getpid() held, pids() listed ancestor numbers, pidfd_open resolved them in diskgc's namespace and gave ESRCH, so a live process was Gone (mped reproduced it: proc_fds(7777) = Gone while 7777 held its file).
- Fix: LinuxHost::new reads /proc/self/status and requires its NSpid line to have exactly one field, equal to getpid(). fs/proc/array.c task_state prints NSpid as 'for (g = ns->level; g <= pid->level; g++)', with ns the /proc mount's pid namespace (proc_pid_ns(sb), via proc_single_show in fs/proc/base.c) and pid the task's struct pid. One field means the mount's namespace is at the task's own level, and since the task is visible in it, it is the task's own. A missing /proc/self/status (ENOENT) or NSpid line (no CONFIG_PID_NS; the line exists since 4.1) is refused. The readlink check is gone (subsumed).
- Tests: the_host_refuses_what_its_readers_cannot_stand_on, a pure test: '4242 4242' refused, '855089 1' refused, '17' with getpid 4242 refused, a missing line refused, ENOENT refused, EACCES passed on, '4242' accepted. Also the_host_is_refused_under_an_ancestors_proc_with_an_equal_pid on the real kernel, without root and without clone3. The helper is root of a new user namespace and pid 1 of namespace A. It mounts A's /proc in a private mount namespace and unshares B. A's init and B's init then each set their namespace's next pid through kernel.ns_last_pid (kernel/pid_namespace.c pid_ns_ctl_handler: the writer's own namespace, CAP_CHECKPOINT_RESTORE in its user namespace), with a ready/go handshake, so the probe is 4242 in both A and B. Seen: '/proc/self: 4242', NSpid '4242 4242'. It passes in the nix build sandbox (musl) too.
- Bites:
  - The equal-pid test on the round-9 code (export of the index before this round; old non-test code with the new test module): 'host setup: ok as pid 4242 (/proc/self: 4242)', and the test FAILs. (The other namespace test also fails there, but only on its changed message.)
  - Mutants on the new check: accepting when the last NSpid field matches FAILs 3 tests (the pure test and both real-kernel tests); accepting a missing NSpid line FAILs the pure test.
- mped's nsprobe scenario (unshare -Urpf --mount-proc, clone3 set_tid [4242, 4242]) runs the diskgc binary:
  - new binary: 'diskgc: read /proc/self/status: its NSpid line is "4242 4242" ... /proc belongs to another pid namespace', refused before argument parsing (startup exits 2; nsprobe does not pass the exit status on);
  - round-9 binary: accepted, and went on to argument parsing.
Small items:
- R/0 and D/2 added to the not-exited cases of the rule test.
- The check_pidfd_open message now names seccomp or a sandbox, not a kernel older than 5.3.
- Docs (tasks.rs module doc, safety.md):
  - soundness does not depend on the poll coming first; the poll only decides whether the status read is made;
  - exit_state, once set, is never cleared (Z to EXIT_TRACE and back while a tracer reaps, kernel/exit.c wait_task_zombie), replacing the too strong '0 to Z to X';
  - safety.md says what the namespace check reads (NSpid), and that kernels without CONFIG_PID_NS are refused;
  - limit 4 in safety.md: pid number replacement after the pidfd was taken (the newcomer may be missed until the next scan).
- collect's read closure is now Round::read.
- The open_pidfd doc names both ESRCH sources: find_get_pid (kernel/pid.c) and pidfd_prepare's !pid_has_task(PIDTYPE_PID) (kernel/fork.c).
- The namespace tests' failures name the precondition (unprivileged user namespaces, and inside them pid and mount unshare, mounting proc, kernel.ns_last_pid). Checked by running the equal-pid test in a user namespace with max_user_namespaces set to 0: it fails with 'start a process in a new user namespace: No space left on device (os error 28); the namespace tests need unprivileged user namespaces ...'. task-52 has the CI note.
- X with Threads 1 stays rejected (conservative, QA optional item).
- Filed in task-61: a bounded pidfd wait when a round finds nothing live; startup()'s host checks for commands that never read /proc; the equal-pid test's dependence on mounting proc in CI.
Gates (code before the QA addendum edits; those edits change only a doc comment, test messages, safety.md and backlog, and are re-gated below): nix flake check exit 0; nix build --rebuild of workspace-tests, e2e-tests, workspace-clippy and e2e-clippy exit 0 (70 passed, both namespace tests ran in the sandbox). just check exit 0. just e2e twice exit 0 (20/20 each). just e2e-full exit 0 (21/21). Stability: 660 parallel runs, 660 pass.

Round 10 final gates, on the tree with the QA addendum edits (all staged): nix flake check exit 0; nix build --rebuild of workspace-tests, e2e-tests, workspace-clippy and e2e-clippy exit 0 (70 passed; both namespace tests ran in the sandbox). just check exit 0. just e2e twice exit 0 (20/20 each). just e2e-full exit 0 (21/21). Stability: 660 parallel runs, 660 pass.

Final (round 11; convergence policy from the user: the check-vs-delete TOCTOU and the narrow multi-event races are accepted, documented limits; only this single-event fix went in). Not committed, staged.
- Fix (Codex round 11, single event, false Gone): LinuxHost::new validated /proc once, but every later reader resolved absolute /proc paths, so a procfs swapped in on /proc after startup (an ancestor namespace's, exposed by unmounting ours) would hand outer pids to pidfd_open (ESRCH, so Gone). Now new() opens /proc once (O_PATH|O_DIRECTORY|O_CLOEXEC) and LinuxHost keeps the fd. The NSpid check reads self/status through it, and every procfs access is relative to it (ProcRoot: openat, statx, readlinkat): pids (readdir of '.'), task listings, status, stat, fd directories (getdents on openat), fd/N, cwd, root and exe links, maps, environ, and mountinfo. Plain *at calls, without RESOLVE_* flags: the magic links must be followed by statx and read by readlinkat, which RESOLVE_NO_MAGICLINKS/BENEATH would refuse. While the fd is held the checked mount cannot be unmounted except lazily, and a lazily detached mount stays reachable through the fd.
- Tests: every_reader_goes_through_the_held_procfs_fd builds the host on a fake procfs tree in a temp directory (LinuxHost::with_proc). Each reader must read the fake tree: pids [4242], tasks [4242, 4243], status S/2, stat, an fd/3 symlink, the cwd link, maps, environ, mountinfo. A fake self/status with NSpid '1 <pid>' is refused through the fd. procfs_is_reached_only_through_the_held_fd is the grep check: the non-test source of every diskgc-linux file may hold the '"/proc' literal only in const PROC, and may use PROC only in new() and ProcRoot::shown (messages).
- Bites: four mutants each revert one reader to the old path-based access (pids, task_ids, group_status, mountinfo), and each FAILs both new tests. No full real-kernel remount test: the seam test pins the fd use, as the coordinator allowed.
- Docs: the host.rs module doc and safety.md (checked once at startup on a /proc that diskgc keeps open; every later read goes through it). The safety.md limits are now 'accepted limits', and limit 5 is the check-vs-delete window (including a thread acquiring after a round's count). Limits 1-4 already covered the late thread, the mapping moved below both passes, the large fd-table prefix with misbound lookups, and pid-number replacement.
- Gates (final staged tree): nix flake check exit 0; nix build --rebuild of workspace-tests, e2e-tests, workspace-clippy and e2e-clippy exit 0 (72 passed). just check exit 0. just e2e twice exit 0 (20/20 each). just e2e-full exit 0 (21/21). Stability: 660 parallel runs, 660 pass.

Committed as 22f29d9 (tree 34d52bb) after the deep gate's final round: qa-test-runner, mped-architect and Codex all GO. Verification note on refs/notes/verification.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
The Host port is in diskgc-core:
- host.rs: syscall-shaped port. Lock calls consume the fd and return a Locked handle, released only by drop/close. statx fields are Option, filled only when the kernel answered. ProcError::Gone means the kernel said, through the process's pidfd, that it has no live task.
- resolve.rs: the deepest/topmost mount from mountinfo; an O_PATH open of the mount point with NO_SYMLINKS, confirmed by statx (MOUNT_ROOT plus mnt_id); then openat2 BENEATH|NO_SYMLINKS|NO_XDEV with a post-open mount-id check.
- kernel.rs: the 5.8 minimum.
LinuxHost is in diskgc-linux over rustix linux_raw:
- LinuxHost::new() refuses a /proc of another pid namespace and an unavailable pidfd_open;
- a mountinfo parser with octal unescaping, mask-checked statx, statvfs (f_bavail*f_frsize), flock try and a polling deadline wait;
- /proc readers that cover every task (tasks.rs), in self-contained rounds. A round lists the tasks, reads the thread count, and lists them again; its listing is complete when every task of the first listing is in the second and their number equals the count. A round is the answer when the listing is complete, every read is trusted and one task is live. The leader's tid is read every round, and again when another task was seen ended. Reads are one-call or confirmed (environ, maps in two passes, fd table).
- Gone comes only from the pidfd: pidfd_open ESRCH, POLLHUP, or POLLIN confirmed by a later status read showing Z with Threads 1.
The diskgc binary exits 2 at startup on kernels below 5.8 or when the host setup is refused (--version/--help exempt). Container tests are in diskgc-e2e/src/host_suite.rs, unit tests run on gnu and musl, and the bite proofs are recorded in the notes. Docs: safety.md (Path traversal, In use/Threads, Host lock), architecture.md (Host port, Deployment), testing.md (mutation scope). Not committed; the deep review gate is pending, so DoD stays unchecked.
<!-- SECTION:FINAL_SUMMARY:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
