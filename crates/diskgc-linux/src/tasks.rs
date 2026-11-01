//! Reading a process through every one of its tasks (threads).
//!
//! `/proc/<pid>/{fd,cwd,root,exe,maps,environ}` show the thread-group
//! leader only. That is not enough:
//!
//! - after the main thread exits (`pthread_exit`) while workers run, the
//!   leader is a zombie: its `fd` listing is empty and `cwd`, `root`, `exe`
//!   give `ENOENT`, although the process still uses its files;
//! - a thread that called `unshare(CLONE_FILES)` or `unshare(CLONE_FS)` has
//!   its own fd table or working directory, which the leader's entries do
//!   not show.
//!
//! So the readers walk `/proc/<pid>/task/<tid>/...` for every task, in
//! rounds. Each round stands on its own: one listing, a read of every task
//! in it, and a decision ([`collect`]). The kernel facts behind it:
//!
//! - **The task listing is not a snapshot.** `readdir` walks the thread list
//!   under RCU (fs/proc/base.c `first_tid`/`next_tid`): it stops when the
//!   task it holds is released, even one it never emitted, and a `getdents`
//!   resumed at an offset can skip or repeat entries. The kernel's thread
//!   count (`signal->nr_threads`, `Threads:` in `/proc/<pid>/status`) changes
//!   in the same critical section as thread-list membership and the pid
//!   (`copy_process` in kernel/fork.c; `__exit_signal`/`__unhash_process`
//!   in kernel/exit.c, under `tasklist_lock` and `siglock`). So a round lists
//!   the tasks, reads the count, and lists them again. The listing is
//!   complete when every task of the first listing is in the second one,
//!   their number equals the count, and the host confirms each numeric tid
//!   still reaches its held procfs task inode after the count. Without that
//!   last check, a tid recycled between the two listings would counterfeit
//!   continuity. Each confirmed task was attached before and after the
//!   count, hence counted, so they are exactly the tasks alive at the count
//!   ([`Listing::complete`]). `Threads: 0` proves nothing
//!   (`task_sig` in fs/proc/array.c prints 0 when the task the `status` read
//!   reached has no signal handlers any more).
//! - **A task that is exiting** (`PF_EXITING`) still shows state `R` or `D`
//!   while it releases its memory map, files, `cwd` and `exe`: reading it
//!   gives `ENOENT` or empty results that say nothing about the process.
//! - **A tid can change hands.** When a non-leader thread execs, `de_thread`
//!   (fs/exec.c) waits until the old leader is a zombie, then, inside one
//!   `write_lock_irq(&tasklist_lock)` section, swaps tids with it
//!   (`exchange_tids`), makes it a non-leader (`exit_signal = -1`) and marks
//!   it dead (`EXIT_DEAD`), and releases it after dropping the lock.
//!   `start_time` is copied over, so `stat` cannot tell the two tasks apart.
//!   Meanwhile the exec'ing thread's old tid shows the old leader (`Z` for
//!   the moment between the swap and `EXIT_DEAD`, then `X`), and a cold
//!   lookup of any entry under `/proc/<pid>` can fail (`ENOENT`) or stay
//!   bound to the old leader until its release (`pid_revalidate` keeps an
//!   entry while its pid has a task). Reads of the old leader find nothing
//!   (no `mm`, `files`, `fs`).
//!
//! Hence, in every round:
//!
//! - the leader's tid is read like every other task, never carried over from
//!   an earlier round: an unseen exec may have handed it to another task;
//! - when a non-leader task is seen ended (`Z`, `X` or vanished), the
//!   leader's tid is read once more, after every other read of the round:
//!   that task may be the old leader under the exec'ing thread's old tid,
//!   and the leader's tid may name the survivor, whose private fd table or
//!   `cwd` would otherwise be missed. (Every exiting thread passes through
//!   `Z` or `X` too, `exit_notify` in kernel/exit.c, so this also happens
//!   without an exec; it is one more read.)
//! - a read that found nothing (empty, or `ENOENT`) from a task its `stat`
//!   then shows live is not trusted: a live userspace task always has a
//!   memory map, so an empty one is never the answer. The same holds for an
//!   empty environment and an empty fd table, which cannot be told apart
//!   from such a misbound read: a process whose environment or fd table
//!   really is empty (`env -i`, all fds closed) is reported as unreadable
//!   (`Failed`), which costs candidates while it runs. The leader's own
//!   entry vanishing is not trusted either. A kernel thread (`PF_KTHREAD`
//!   in the `stat`s around the read) has no memory map or environment of
//!   its own: for the memory map and environment, its read that found
//!   nothing is the answer, nothing (TASK-306: Linux 6.18 answers its
//!   `environ` with `ESRCH`, which root's scan retried for every kernel
//!   thread).
//!
//! A round is the answer when its listing is complete, every task in it
//! gave a trusted read, and one of them is live ([`shortfall`] says what a
//! round lacks). Otherwise another round follows, up to [`MAX_ROUNDS`],
//! then `Failed`. Only the answering round's reads are returned.
//!
//! **Gone** is never inferred from listings or `ENOENT`: a lookup can race
//! an exec, `hidepid` hides live processes, and a stalled exec can keep
//! showing `Z`. It comes only from the kernel's answer about the process
//! itself, asked through its pidfd at the start and whenever a round finds
//! nothing live or its listing vanished ([`has_exited`]; LinuxHost also says
//! `Gone` when `pidfd_open` gives `ESRCH`):
//!
//! - **`POLLHUP`: the pid has no task.** `pidfd_poll` (fs/pidfs.c) reports
//!   `EPOLLHUP` only when `pid_task(pid, PIDTYPE_PID)` is `NULL`, i.e. the
//!   process was released. A live process never shows no task: the exec
//!   swap moves the task lists with `hlists_swap_heads_rcu`
//!   (include/linux/rculist.h), which never leaves a head empty.
//! - **`POLLIN` alone is not enough.** `pidfd_poll` reports `EPOLLIN` when
//!   the task it found has `exit_state` set and is not a leader waiting for
//!   its threads (`!delay_group_leader`). A poll that found the old leader
//!   (a zombie) of a non-leader exec just before `exchange_tids` and reads
//!   it after `de_thread` set `leader->exit_signal = -1` sees a zombie that
//!   is no leader (`thread_group_leader` is `exit_signal >= 0`,
//!   include/linux/sched/signal.h), and reports readable while the process
//!   lives on. So `POLLIN` counts only when one read of
//!   `/proc/<pid>/status` taken **after** the poll shows `State: Z` and
//!   `Threads: 1`.
//! - **Why `Z` and `Threads: 1` in one read is sound.** `proc_pid_status`
//!   (fs/proc/array.c) shows one task: `task_state` prints its state, then
//!   `task_sig` prints `signal->nr_threads` under `lock_task_sighand`, or 0
//!   once the task was released. The count goes up in `copy_process` and
//!   down in `__exit_signal` (which also clears the task's `sighand`), both
//!   under that `siglock`. So `Threads: 1` means the task shown is the only
//!   unreleased task of its group; every live task is unreleased, so none is
//!   live, and a group whose only task is a zombie never runs again. The old
//!   leader of a non-leader exec shows `Z` only while the exec'ing thread is
//!   still counted (`Threads:` 2 or more), and 0 once released. This does
//!   not depend on the poll coming first: one such read alone shows that no
//!   task of the process is live, given a `/proc` of diskgc's own pid
//!   namespace (checked when the host is set up) and that the number still
//!   names the process (see the limits below). The poll only decides
//!   whether the extra read is made.
//! - **Memory ordering.** The state is read before `lock_task_sighand`, the
//!   count inside it. On a weakly ordered CPU (arm64) the state load may be
//!   satisfied late, but never after the `siglock` release that ends
//!   `task_sig` (a release orders every earlier access), and no task can
//!   join or leave the group while that lock is held. So the zombie state
//!   seen held at the latest while the count was 1, and the task never runs
//!   again: once set, `exit_state` is never cleared (it can go from zombie
//!   to `EXIT_TRACE` and back while a tracer reaps the task, then to dead;
//!   kernel/exit.c `wait_task_zombie`). The other order (count read before the
//!   state, outside one lock section) would not be sound: a lone live task
//!   could start a thread and exit in between.
//! - **Kernels.** `EPOLLHUP` for a pid without a task came with fs/pidfs.c
//!   in Linux 6.9. On 5.8 to 6.8 (kernel/fork.c `pidfd_poll`,
//!   `thread_group_exited`) a released process gives `POLLIN` alone.
//!   LinuxHost checks `pidfd_send_signal(pidfd, 0)` then: `ESRCH` confirms
//!   release; success or `EPERM` leaves it inconclusive. A blocked syscall
//!   fails closed. Subsequent polls wait at most 2 ms before the next
//!   round so a task in the act of exiting has a chance to be released.
//!
//! A read of one task's entry must also come from one task and one memory
//! map in full; [`crate::host`] makes sure of that with the helpers in
//! [`crate::procfs`]: `environ` in one `read` call; `maps` in two passes on
//! separate opens, each read to the end and confirmed by reading again on
//! the same file (the memory map captured at open still exists), keeping
//! the lines of both (a mapping moved below a pass's read position by
//! `mremap` is found by the other pass); the fd table from one `getdents`
//! call.
//!
//! Each task entry is bracketed by two `stat` reads through the same held
//! procfs `stat` file of the task. Its inode holds the kernel `struct pid`;
//! a recycled numeric tid cannot redirect the second `stat` to a new task,
//! even if both tasks started in one clock tick. A changed `starttime`
//! (field 22) or a vanished held task discards the round. `de_thread` swaps
//! tasks under the *same* pid structure and copies the start time; the
//! leader re-read below covers that separate case. The window in which an exec'ing
//! thread's old tid shows the old leader as `Z` is
//! normally sub-microsecond (it lies inside `de_thread`'s locked section),
//! and while it lasts the process is not settled, never gone. A thread
//! started after a round's count is not read (also when it then execs), the
//! same race as a process opening a file after the check. The `status` read
//! goes by pid number: if the process was released and its number reused
//! before that read, the read shows the new process, and `Gone` is still
//! true. Every read under `/proc/<pid>` goes by number too, while the pidfd
//! pins the process itself, not its number: when the process is released
//! and another one gets its number before a later read, the readers may
//! read the newcomer in its place or miss it. In particular, a scan that
//! already marked the number settled need not read its new occupant, even
//! if that occupant appears in a later listing. The scan's one-second
//! freshness limit bounds the time this answer can be used, but not the
//! rate of pid reuse within it. A zombie whose `status` is hidden
//! (`hidepid`) is `Failed`, not `Gone`.
//!
//! For [`Want::EveryTask`], a task whose entry the kernel says is the very
//! object an earlier task of the round was read for (an fd table shared by
//! `CLONE_FILES`) counts as read by that read ([`same_as_reference`]): a
//! desktop's thousands of threads share some hundreds of fd tables.
//!
//! The decisions are made here without I/O ([`collect`] takes the reads as
//! closures; [`has_exited`], [`shortfall`], [`task_status`] and
//! [`parse_task_stat`] are pure) so they are unit-tested; [`crate::host`]
//! supplies the actual reads.

use std::collections::BTreeMap;

use diskgc_core::host::{Errno, HostError, ProcError};

use crate::procfs::StatusFields;

/// What a task's `stat` says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskStat {
    /// The state letter (`R`, `S`, `D`, `Z`, `X`, ...).
    pub state: char,
    /// `PF_EXITING`: the task is exiting and releasing what it holds.
    pub exiting: bool,
    /// `PF_KTHREAD`: a kernel thread, which has no memory map or `exe`.
    pub kernel_thread: bool,
    /// Field 22 of `/proc/<pid>/task/<tid>/stat`, in clock ticks since boot.
    /// This is an additional check beside the held task-directory identity.
    pub start_ticks: u64,
}

/// `PF_EXITING` and `PF_KTHREAD` in the flags field of `stat`.
const PF_EXITING: u64 = 0x0000_0004;
const PF_KTHREAD: u64 = 0x0020_0000;

/// The most rounds [`collect`] makes (each one confirmed listing and the
/// reads of its tasks) before it gives up on a process that does not settle.
pub(crate) const MAX_ROUNDS: usize = 16;

/// Parses `/proc/<pid>/task/<tid>/stat`. The command name (field 2) is in
/// parentheses and may itself contain spaces and `)`, so the fields after
/// it are found from the **last** `)`.
pub(crate) fn parse_task_stat(text: &[u8]) -> Result<TaskStat, String> {
    let shown = || String::from_utf8_lossy(text).into_owned();
    let close = text
        .iter()
        .rposition(|b| *b == b')')
        .ok_or_else(|| format!("task stat without ')': {}", shown()))?;
    let rest = std::str::from_utf8(&text[close + 1..])
        .map_err(|_| format!("non-UTF-8 task stat: {}", shown()))?;
    // After the name: state(3) ppid(4) pgrp(5) session(6) tty_nr(7)
    // tpgid(8) flags(9) ... starttime(22). Read in place: this runs twice
    // per task entry read, so it allocates nothing (TASK-115).
    let mut fields = rest.split_whitespace();
    let state = fields
        .next()
        .and_then(|s| {
            let mut chars = s.chars();
            chars.next().filter(|_| chars.next().is_none())
        })
        .ok_or_else(|| format!("no state in task stat: {}", shown()))?;
    // nth counts from the field after the state.
    let flags: u64 = fields
        .nth(5)
        .and_then(|f| f.parse().ok())
        .ok_or_else(|| format!("no flags in task stat: {}", shown()))?;
    let start_ticks = fields
        .nth(12)
        .and_then(|f| f.parse().ok())
        .ok_or_else(|| format!("no starttime in task stat: {}", shown()))?;
    Ok(TaskStat {
        state,
        exiting: flags & PF_EXITING != 0,
        kernel_thread: flags & PF_KTHREAD != 0,
        start_ticks,
    })
}

/// A task as [`collect`] judges it, after reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Live,
    /// Alive, but has no memory map, `exe` or files of its own to show.
    KernelThread,
    /// Releasing what it holds (`PF_EXITING`, not yet a zombie): what it
    /// shows is incomplete.
    Exiting,
    /// A zombie (`Z`) or dead but not yet released (`X`).
    Exited,
    /// Its entry is gone (`ENOENT`, `ESRCH`).
    Vanished,
}

impl Status {
    /// A read from this task can be the answer.
    fn usable(self) -> bool {
        matches!(self, Self::Live | Self::KernelThread)
    }

    /// The task has exited, or is the old leader of a non-leader exec under
    /// the exec'ing thread's old tid (see the module docs).
    fn ended(self) -> bool {
        matches!(self, Self::Exited | Self::Vanished)
    }
}

/// Classifies a task from the result of reading its `stat`. Any error other
/// than the entry's disappearance is passed on, so the caller stays
/// conservative.
pub(crate) fn task_status(stat: &Result<TaskStat, HostError>) -> Result<Status, HostError> {
    match stat {
        Ok(s) if matches!(s.state, 'Z' | 'X' | 'x') => Ok(Status::Exited),
        Ok(s) if s.exiting => Ok(Status::Exiting),
        Ok(s) if s.kernel_thread => Ok(Status::KernelThread),
        Ok(_) => Ok(Status::Live),
        Err(e) if is_vanished(e) => Ok(Status::Vanished),
        Err(e) => Err(e.clone()),
    }
}

/// How many results [`collect`] wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    /// One result per task (fd tables, cwd and root can differ per task).
    EveryTask,
    /// One result from a task that is live after the read (memory map and
    /// environment, shared by all tasks of a process).
    FirstLiveTask,
}

pub(crate) fn is_vanished(e: &HostError) -> bool {
    e.is(Errno::ENOENT) || e.is(Errno::ESRCH)
}

/// One listing of a process's tasks: `/proc/<pid>/task` read, then the
/// kernel's thread count (`Threads:` in `/proc/<pid>/status`, i.e.
/// `signal->nr_threads`), then `/proc/<pid>/task` read again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listing {
    /// Sorted, without duplicates (a `readdir` resumed at an offset can
    /// repeat an entry).
    tids: Vec<u32>,
    threads: usize,
    /// Every numeric tid of the first listing is in the second one, and
    /// the host confirmed its held task inode still owns that tid.
    confirmed: bool,
}

impl Listing {
    /// `tids`, the count read right after them, and `again`, the listing
    /// made right after the count.
    pub(crate) fn new(mut tids: Vec<u32>, threads: usize, again: &[u32]) -> Self {
        tids.sort_unstable();
        tids.dedup();
        let confirmed = tids.iter().all(|tid| again.contains(tid));
        Self {
            tids,
            threads,
            confirmed,
        }
    }

    /// The tasks of one plain listing, for [`Want::FirstLiveTask`]: neither
    /// counted nor confirmed, which that want does not need (see
    /// [`shortfall`]).
    pub(crate) fn candidates(tids: Vec<u32>) -> Self {
        Self::new(tids, 0, &[])
    }

    /// A host saw a different procfs inode under a numeric tid before and
    /// after the count. The numeric listing alone cannot prove which tasks
    /// were counted, even if it contains the same numbers both times.
    pub(crate) fn invalidate_identity(&mut self) {
        self.confirmed = false;
    }

    /// The listing is exactly the set of tasks alive at the count: every
    /// task of the first listing still owns its numeric tid after the count,
    /// hence was counted, and their number equals the count. The count
    /// changes together with the thread list (see the module docs).
    /// `Threads: 0` proves nothing (`task_sig` prints 0 when
    /// the task the `status` read reached has no signal handlers any more).
    fn complete(&self) -> bool {
        self.confirmed && self.threads > 0 && self.tids.len() == self.threads
    }
}

/// What `poll` on a process's pidfd reported (fs/pidfs.c `pidfd_poll`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PidfdPoll {
    /// Not readable: the task found is running, or is a leader waiting for
    /// its threads.
    Running,
    /// `POLLIN` without `POLLHUP`: the task the poll found had exited and
    /// did not look like a leader with threads. Not proof by itself (see
    /// the module docs).
    Readable,
    /// `POLLHUP`: the pid has no task left; the process was released.
    Hangup,
}

/// The decision rule for `Gone` (see the module docs): `Hangup` is final;
/// `Readable` counts only when `status_after`, one read of
/// `/proc/<pid>/status` made after the poll (`None` when it could not be
/// found), shows a zombie that is the only task of its group.
pub(crate) fn has_exited(
    poll: PidfdPoll,
    status_after: impl FnOnce() -> Result<Option<StatusFields>, ProcError>,
) -> Result<bool, ProcError> {
    Ok(match poll {
        PidfdPoll::Running => false,
        PidfdPoll::Hangup => true,
        PidfdPoll::Readable => {
            status_after()?.is_some_and(|status| status.state == 'Z' && status.threads == 1)
        }
    })
}

/// What [`Process::list`], [`Process::poll`] and [`Process::status`] give.
pub(crate) type Listed = Result<Option<Listing>, ProcError>;
pub(crate) type Polled = Result<PidfdPoll, ProcError>;
pub(crate) type StatusRead = Result<Option<StatusFields>, ProcError>;

/// How [`collect`] reaches one process.
pub(crate) struct Process<L, P, G> {
    /// The process id, also the leader's tid.
    pub pid: u32,
    /// Makes one [`Listing`]; `None` when a lookup under `/proc/<pid>` said
    /// `ENOENT`, which alone proves nothing.
    pub list: L,
    /// Polls the process's pidfd.
    pub poll: P,
    /// Reads `/proc/<pid>/status` once; `None` when it said `ENOENT`.
    pub status: G,
}

/// How [`collect`] reaches the tasks of one process.
pub(crate) struct Tasks<R, E, S, K> {
    /// What a read gives, for messages ("fd table", "environment", ...).
    pub what: &'static str,
    /// Reads one task's entry.
    pub read: R,
    /// Whether a successful read found nothing (an empty fd table, memory
    /// map or environment).
    pub is_empty: E,
    /// Reads one task's `stat`.
    pub stat: S,
    /// `shares(reference, tid)`: whether the kernel says, now, that task
    /// `tid` holds the very object task `reference` holds for this entry
    /// (`kcmp(KCMP_FILES)` for the fd table). `false` whenever that cannot
    /// be told; then `tid` is read in full ([`same_as_reference`]).
    pub shares: K,
}

/// The `shares` of an entry that is never taken from another task's read.
pub(crate) fn never_shared(_: u32, _: u32) -> bool {
    false
}

/// What one read of one task says, for the round's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// A read the round can stand on, and the task's status right after it.
    Trusted(Status),
    /// The task is live, yet its read was empty: maybe bound to another
    /// task (see the module docs).
    Empty,
    /// The task is live, yet its read said `ENOENT` or `ESRCH` (which).
    Missing(Errno),
    /// The leader's own entry vanished: a lookup can race a non-leader exec.
    LeaderVanished,
    /// The number was handed to another task during this read. The read
    /// cannot prove anything about the task now occupying the number.
    Reused,
}

impl Seen {
    fn usable(self) -> bool {
        matches!(self, Self::Trusted(status) if status.usable())
    }
}

/// Brackets task `tid`'s entry read with `stat` from the same pinned task
/// directory. A changed start tick or a missing first `stat` makes the
/// whole round untrustworthy;
/// the task's status after the read catches one that exited during it.
///
/// The value worth keeping: for [`Want::EveryTask`] every successful read,
/// also from a task that exits meanwhile or an untrusted one (reporting too
/// much in use is the safe side); for [`Want::FirstLiveTask`] only a
/// trusted read from a live task or a kernel thread.
fn observe<T>(
    pid: u32,
    tid: u32,
    tasks: &mut Tasks<
        impl FnMut(u32) -> Result<T, HostError>,
        impl Fn(&T) -> bool,
        impl FnMut(u32) -> Result<TaskStat, HostError>,
        impl FnMut(u32, u32) -> bool,
    >,
    want: Want,
) -> Result<(Option<T>, Seen, Option<TaskStat>), ProcError> {
    let before = (tasks.stat)(tid);
    let result = (tasks.read)(tid);
    let after = (tasks.stat)(tid);
    let status = task_status(&after).map_err(ProcError::from_host)?;
    let reused = match (&before, &after) {
        (Ok(before), Ok(after)) => before.start_ticks != after.start_ticks,
        // The directory may be pinned to a previous incarnation from an
        // earlier round. A numeric tid in this round's listing can already
        // name a replacement even when both reads through the old handle
        // say ENOENT. It must never count as a trusted vanished task.
        (Err(error), _) if is_vanished(error) => true,
        (Err(error), _) => return Err(ProcError::from_host(error.clone())),
        _ => false,
    };
    let seen = match &result {
        _ if reused => Seen::Reused,
        _ if tid == pid && status == Status::Vanished => Seen::LeaderVanished,
        Ok(value) if status == Status::Live && (tasks.is_empty)(value) => Seen::Empty,
        Err(e) if status == Status::Live && is_vanished(e) => {
            Seen::Missing(e.kernel_errno().unwrap_or(Errno::ENOENT))
        }
        // A kernel thread by both `stat`s around the read, or not at all
        // (TASK-306 review): a task that turned into one is no answer.
        _ if status == Status::KernelThread && !matches!(&before, Ok(b) if b.kernel_thread) => {
            Seen::Reused
        }
        _ => Seen::Trusted(status),
    };
    let value = match result {
        Ok(value) if want == Want::EveryTask || seen.usable() => Some(value),
        Ok(_) => None,
        Err(e) if is_vanished(&e) => None,
        Err(e) => return Err(ProcError::from_host(e)),
    };
    Ok((value, seen, after.ok()))
}

/// Whether task `tid` can be counted as read by the round's reference read
/// (`reference`: a task whose own read in this round was kept, trusted and
/// made while it was live, and its `stat` after that read), without
/// reading `tid`'s entry itself. In this order: `tid`'s `stat` (which pins
/// it before the comparison; it being live already only spares a
/// comparison that could not count); `shares(reference, tid)` true; the reference's `stat` again, live and
/// with its start time; `tid`'s `stat` again, live and with its start time.
/// Every `stat` goes through the task's held procfs file, which pins its
/// `struct pid`. Anything else, an error included, is `false`, and `tid`
/// is read in full (and its errors reported) by [`observe`].
///
/// Why one read then stands for both (`KCMP_FILES`, the fd table): a task
/// only ever leaves a table it shares, never joins one that exists:
/// `unshare(CLONE_FILES)` and `execve` give the caller a copy, and only a
/// task created afterwards by `clone(CLONE_FILES)` can share that copy.
/// The reference was live with its start time after its read and again
/// after the comparison, through its held file, so the comparison saw the
/// reference itself, not a task that took its number. `tid`'s held file may
/// have been opened only here (by number, after the listing), so `tid` is
/// not shown to be the listed task; it does not need to be. The table both
/// hold at the comparison is either the one the reference read, which
/// `tid` held at the read too or joined only by being created since (a
/// thread started after the round's count, docs/safety.md limit 1), or a
/// copy the reference made after its read, which only a task created since
/// can share (limit 1 again); what was opened in it after the read is the
/// check-to-use race (limit 5). An exec in the process kills every other
/// task before it swaps tids (`de_thread`), so a task compared across it is
/// not live at its second `stat`. `kcmp` itself compares two pointers
/// without a lock (docs/safety.md limit 7).
fn same_as_reference(
    reference: (u32, TaskStat),
    tid: u32,
    stat: &mut impl FnMut(u32) -> Result<TaskStat, HostError>,
    shares: &mut impl FnMut(u32, u32) -> bool,
) -> bool {
    let live = |read: &Result<TaskStat, HostError>| matches!(task_status(read), Ok(Status::Live));
    let (reference_tid, reference_stat) = reference;
    let before = stat(tid);
    if !live(&before) || !shares(reference_tid, tid) {
        return false;
    }
    let reference_again = stat(reference_tid);
    if !live(&reference_again)
        || reference_again.is_ok_and(|now| now.start_ticks != reference_stat.start_ticks)
    {
        return false;
    }
    let after = stat(tid);
    live(&after) && matches!((before, after), (Ok(b), Ok(a)) if b.start_ticks == a.start_ticks)
}

/// What one round of [`collect`] has read so far.
struct Round<T> {
    results: Vec<T>,
    seen: BTreeMap<u32, Seen>,
    /// The first task of the round whose own read was kept, trusted and
    /// made while it was live, with its `stat` after that read: the read a
    /// task sharing its entry is counted by ([`same_as_reference`]). Only
    /// for [`Want::EveryTask`]; never carried into another round.
    reference: Option<(u32, TaskStat)>,
}

impl<T> Round<T> {
    fn new() -> Self {
        Self {
            results: Vec::new(),
            seen: BTreeMap::new(),
            reference: None,
        }
    }

    /// Reads task `tid` into the round (see [`observe`]), or counts it as
    /// read by the round's reference read when it shares the reference's
    /// entry ([`same_as_reference`]); true when the round is done:
    /// [`Want::FirstLiveTask`] got its read. `may_share` false: always a
    /// read of its own (the leader's re-read after a task was seen ended,
    /// which looks for an exec's survivor).
    fn read(
        &mut self,
        pid: u32,
        tid: u32,
        tasks: &mut Tasks<
            impl FnMut(u32) -> Result<T, HostError>,
            impl Fn(&T) -> bool,
            impl FnMut(u32) -> Result<TaskStat, HostError>,
            impl FnMut(u32, u32) -> bool,
        >,
        want: Want,
        may_share: bool,
    ) -> Result<bool, ProcError> {
        if want == Want::EveryTask
            && may_share
            && let Some(reference) = self.reference
            && reference.0 != tid
            && same_as_reference(reference, tid, &mut tasks.stat, &mut tasks.shares)
        {
            self.seen.insert(tid, Seen::Trusted(Status::Live));
            return Ok(false);
        }
        let (value, seen, after) = observe(pid, tid, tasks, want)?;
        self.seen.insert(tid, seen);
        let kept = value.map(|value| self.results.push(value)).is_some();
        if want == Want::EveryTask
            && kept
            && seen == Seen::Trusted(Status::Live)
            && self.reference.is_none()
        {
            self.reference = after.map(|after| (tid, after));
        }
        Ok(kept && want == Want::FirstLiveTask)
    }
}

/// Why a round is not the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shortfall {
    /// A task of the first listing was missing from the second.
    Changed,
    /// The listing and the count disagree (or the count is 0).
    Miscounted { listed: usize, counted: usize },
    /// A task's read cannot be trusted.
    Untrusted { tid: u32, seen: Seen },
    /// No task of a complete listing (or, for [`Want::FirstLiveTask`], of
    /// the plain listing) gave a usable read.
    NothingLive,
    /// The same for [`Want::FirstLiveTask`], whose listing is not checked
    /// for completeness.
    NoLiveRead,
}

impl Shortfall {
    /// Says what the round lacked; `what` names what a read gives.
    fn describe(self, what: &str) -> String {
        match self {
            Self::Changed => "its task listing changed between reads".to_owned(),
            Self::Miscounted { listed, counted } => {
                format!("its task listing was not complete ({listed} tasks, {counted} counted)")
            }
            Self::Untrusted { tid, seen } => match seen {
                Seen::Empty => format!(
                    "task {tid} is live but gave an empty or incomplete {what} (which cannot be \
                     told from a read bound to another task)"
                ),
                Seen::Missing(errno) => {
                    format!(
                        "task {tid} is live but its {what} said {}",
                        errno.name().unwrap_or("an errno")
                    )
                }
                Seen::LeaderVanished => format!("the leader's entry (task {tid}) vanished"),
                Seen::Reused => format!("task {tid} changed identity during its {what} read"),
                Seen::Trusted(_) => format!("task {tid} gave no trustworthy read"),
            },
            Self::NothingLive => "no task of a complete listing was live".to_owned(),
            Self::NoLiveRead => format!("no listed task gave its {what} while live"),
        }
    }
}

/// What a round lacks to be the answer, or `None` when it is the answer:
/// its listing is [complete](Listing::complete), every task it read gave a
/// trusted read, and one of them is live.
///
/// For [`Want::FirstLiveTask`] the listing is not judged: such a round is
/// the answer as soon as one task gives a usable read ([`collect`] returns
/// it then, before asking this), whatever else the listing holds or lacks,
/// because every task of a process shares the memory map and environment
/// it reads, and that read is bracketed by the task's own `stat`s. Its
/// listing only names the tasks to try ([`Listing::candidates`]); here it
/// only says why no task gave one, unless every read was trusted and one
/// task was a kernel thread: it has no memory map or environment of its
/// own (Linux 6.18 answers its `environ` with `ESRCH`), so a read that gave
/// nothing is the answer, nothing (TASK-306; a user task's `ESRCH` is
/// [`Seen::Missing`], untrusted, and keeps its rounds). `PF_KTHREAD` is
/// read from the task's `stat` before and after the read, with one start
/// time; no user task can set it.
fn shortfall(listing: &Listing, seen: &BTreeMap<u32, Seen>, want: Want) -> Option<Shortfall> {
    if want == Want::FirstLiveTask {
        if let Some((&tid, &seen)) = seen
            .iter()
            .find(|(_, seen)| !matches!(seen, Seen::Trusted(_)))
        {
            return Some(Shortfall::Untrusted { tid, seen });
        }
        if seen
            .values()
            .any(|seen| *seen == Seen::Trusted(Status::KernelThread))
        {
            return None;
        }
        return Some(Shortfall::NoLiveRead);
    }
    if !listing.confirmed {
        Some(Shortfall::Changed)
    } else if !listing.complete() {
        Some(Shortfall::Miscounted {
            listed: listing.tids.len(),
            counted: listing.threads,
        })
    } else if let Some((&tid, &seen)) = seen
        .iter()
        .find(|(_, seen)| !matches!(seen, Seen::Trusted(_)))
    {
        Some(Shortfall::Untrusted { tid, seen })
    } else if !seen.values().any(|seen| seen.usable()) {
        Some(Shortfall::NothingLive)
    } else {
        None
    }
}

/// Reads the tasks of one process, in rounds. `Gone` only by
/// [`has_exited`], asked at the start, when a round finds nothing live and
/// when the listing vanished; no listing and no `ENOENT` decide it.
///
/// Each round makes one [`Listing`] and reads every task in it, the
/// leader's tid included (nothing is carried over from an earlier round). If
/// a non-leader task is seen ended (`Z`, `X` or vanished), the leader's tid
/// is read once more, after every other read of the round. The round is the
/// answer unless [`shortfall`] says what it lacks; then the next round
/// follows. After [`MAX_ROUNDS`] rounds: `Failed`, saying what the last
/// round lacked.
///
/// Per task: `EACCES`/`EPERM` is `Denied`; any error other than `ENOENT` /
/// `ESRCH` is `Failed`; an error reading `stat` other than the entry's
/// disappearance is passed on. [`Want::FirstLiveTask`] returns the first
/// usable read at once.
pub(crate) fn collect<T>(
    mut process: Process<
        impl FnMut() -> Listed,
        impl FnMut() -> Polled,
        impl FnMut() -> StatusRead,
    >,
    mut tasks: Tasks<
        impl FnMut(u32) -> Result<T, HostError>,
        impl Fn(&T) -> bool,
        impl FnMut(u32) -> Result<TaskStat, HostError>,
        impl FnMut(u32, u32) -> bool,
    >,
    want: Want,
) -> Result<Vec<T>, ProcError> {
    let pid = process.pid;
    let mut exited = || has_exited((process.poll)()?, &mut process.status);
    if exited()? {
        return Err(ProcError::Gone);
    }
    let mut lacked = String::new();
    for _ in 0..MAX_ROUNDS {
        let Some(listing) = (process.list)()? else {
            if exited()? {
                return Err(ProcError::Gone);
            }
            lacked = "its task listing vanished".to_owned();
            continue;
        };
        let mut round = Round::new();
        for &tid in &listing.tids {
            if round.read(pid, tid, &mut tasks, want, true)? {
                return Ok(round.results);
            }
        }
        // A non-leader seen ended may be the old leader under the exec'ing
        // thread's old tid: the leader's tid may name the survivor now.
        let thread_ended = round
            .seen
            .iter()
            .any(|(&tid, how)| tid != pid && matches!(how, Seen::Trusted(s) if s.ended()));
        if thread_ended && round.read(pid, pid, &mut tasks, want, false)? {
            return Ok(round.results);
        }
        match shortfall(&listing, &round.seen, want) {
            None => return Ok(round.results),
            Some(lack) => {
                lacked = lack.describe(tasks.what);
                // The Linux poll closure waits at most 2 ms after the
                // initial check. This gives an exiting task time to be
                // released before another round repeats the same reads.
                if exited()? {
                    return Err(ProcError::Gone);
                }
            }
        }
    }
    Err(ProcError::Failed(HostError::invalid(
        "read",
        format!("process {pid}"),
        format!("no trustworthy read in {MAX_ROUNDS} rounds: {lacked}"),
    )))
}

/// The owner of a live task's `fd` directory (see `ProcStatus::fd_dir_owner`
/// in diskgc-core): the first of `tids` whose `stat` shows it live (not
/// ended, not exiting, not a kernel thread) both before and after `owner`
/// reads its directory's owner, with the same `starttime`. `PF_EXITING` is
/// set before a task releases its memory map and is never cleared, so a
/// task live after the read had its memory map during it. Its owner reflects
/// the process's dumpability. A task that ends or vanishes on the way is
/// passed over;
/// `None` when no task qualifies (the caller then cannot tell dumpability).
/// An exited leader (`pthread_exit`) has no memory map and shows root on
/// some kernels while live threads are dumpable: it is never the answer.
pub(crate) fn owner_through_live_task(
    tids: &[u32],
    mut stat: impl FnMut(u32) -> Result<TaskStat, HostError>,
    mut owner: impl FnMut(u32) -> Result<Option<u32>, HostError>,
) -> Result<Option<u32>, HostError> {
    for &tid in tids {
        let before = stat(tid);
        if task_status(&before)? != Status::Live {
            continue;
        }
        let before = before.expect("only a successful stat can classify a task as live");
        let found = match owner(tid) {
            Ok(found) => found,
            Err(e) if is_vanished(&e) => continue,
            Err(e) => return Err(e),
        };
        let after = stat(tid);
        if task_status(&after)? == Status::Live
            && after.is_ok_and(|last| before.start_ticks == last.start_ticks)
        {
            return Ok(found);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn errno(e: Errno) -> HostError {
        HostError::errno("read", "/proc/1/task/2/fd", e)
    }

    fn task(state: char, exiting: bool, kernel_thread: bool) -> Result<TaskStat, HostError> {
        Ok(TaskStat {
            state,
            exiting,
            kernel_thread,
            start_ticks: 1,
        })
    }

    fn live() -> Result<TaskStat, HostError> {
        task('S', false, false)
    }

    fn zombie() -> Result<TaskStat, HostError> {
        task('Z', false, false)
    }

    fn dead() -> Result<TaskStat, HostError> {
        task('X', false, false)
    }

    /// State `R` with `PF_EXITING`: releasing its map, files, cwd.
    fn exiting() -> Result<TaskStat, HostError> {
        task('R', true, false)
    }

    /// A listing the kernel would give for these tasks: counted and
    /// confirmed.
    fn complete(tids: &[u32]) -> Listing {
        Listing::new(tids.to_vec(), tids.len(), tids)
    }

    /// A confirmed listing with the kernel's count `threads` (which exceeds
    /// the listing when tasks were missed).
    fn counted(tids: &[u32], threads: usize) -> Listing {
        Listing::new(tids.to_vec(), threads, tids)
    }

    /// Scripted listings, one per round (the last repeats).
    fn rounds(script: Vec<Listing>) -> impl FnMut() -> Result<Option<Listing>, ProcError> {
        let mut next = 0;
        move || {
            let listing = script[next.min(script.len() - 1)].clone();
            next += 1;
            Ok(Some(listing))
        }
    }

    /// Scripted answers of `poll` on the pidfd, in turn (the last repeats).
    fn polls(script: Vec<PidfdPoll>) -> impl FnMut() -> Result<PidfdPoll, ProcError> {
        let mut next = 0;
        move || {
            let poll = script[next.min(script.len() - 1)];
            next += 1;
            Ok(poll)
        }
    }

    /// One `status` read showing `state` and `threads`.
    fn status(state: char, threads: usize) -> Result<Option<StatusFields>, ProcError> {
        Ok(Some(StatusFields { state, threads }))
    }

    /// The process's `status` must not be read: the poll did not say readable.
    fn no_status() -> Result<Option<StatusFields>, ProcError> {
        panic!("status read without a readable poll")
    }

    /// [`collect`] over string results (`""` is an empty read) for a process
    /// whose pidfd never says readable.
    fn scan(
        pid: u32,
        list: impl FnMut() -> Result<Option<Listing>, ProcError>,
        read: impl FnMut(u32) -> Result<&'static str, HostError>,
        stat: impl FnMut(u32) -> Result<TaskStat, HostError>,
        want: Want,
    ) -> Result<Vec<&'static str>, ProcError> {
        let pidfd = Pidfd {
            poll: || Ok(PidfdPoll::Running),
            status: no_status,
        };
        scan_exited(pid, list, pidfd, read, stat, want)
    }

    /// The process's own answers: its pidfd's `poll`, and its `status`.
    struct Pidfd<P, G> {
        poll: P,
        status: G,
    }

    /// [`scan`] with scripted answers of the process's pidfd.
    fn scan_exited(
        pid: u32,
        list: impl FnMut() -> Result<Option<Listing>, ProcError>,
        pidfd: Pidfd<
            impl FnMut() -> Result<PidfdPoll, ProcError>,
            impl FnMut() -> Result<Option<StatusFields>, ProcError>,
        >,
        read: impl FnMut(u32) -> Result<&'static str, HostError>,
        stat: impl FnMut(u32) -> Result<TaskStat, HostError>,
        want: Want,
    ) -> Result<Vec<&'static str>, ProcError> {
        let is_empty = |v: &&'static str| v.is_empty();
        let process = Process {
            pid,
            list,
            poll: pidfd.poll,
            status: pidfd.status,
        };
        collect(
            process,
            Tasks {
                what: "table",
                read,
                is_empty,
                stat,
                shares: never_shared,
            },
            want,
        )
    }

    /// [`collect`] with a `shares` answer, for a process whose pidfd never
    /// says readable.
    fn scan_sharing(
        pid: u32,
        list: impl FnMut() -> Result<Option<Listing>, ProcError>,
        read: impl FnMut(u32) -> Result<&'static str, HostError>,
        stat: impl FnMut(u32) -> Result<TaskStat, HostError>,
        shares: impl FnMut(u32, u32) -> bool,
        want: Want,
    ) -> Result<Vec<&'static str>, ProcError> {
        collect(
            Process {
                pid,
                list,
                poll: || Ok(PidfdPoll::Running),
                status: no_status,
            },
            Tasks {
                what: "table",
                read,
                is_empty: |v: &&'static str| v.is_empty(),
                stat,
                shares,
            },
            want,
        )
    }

    /// The reads of one tid, in turn (the last repeats).
    type ReadScript = Vec<Result<&'static str, HostError>>;

    fn reads(script: Vec<(u32, ReadScript)>) -> impl FnMut(u32) -> Result<&'static str, HostError> {
        let mut script: BTreeMap<u32, (usize, ReadScript)> =
            script.into_iter().map(|(t, r)| (t, (0, r))).collect();
        move |tid| {
            let (n, results) = script.get_mut(&tid).expect("scripted tid");
            let result = results[(*n).min(results.len() - 1)].clone();
            *n += 1;
            result
        }
    }

    fn failed(got: &Result<Vec<&'static str>, ProcError>) -> bool {
        matches!(got, Err(ProcError::Failed(_)))
    }

    fn failed_saying(got: &Result<Vec<&'static str>, ProcError>, text: &str) -> bool {
        matches!(got, Err(ProcError::Failed(e)) if e.detail.contains(text))
    }

    #[test]
    fn task_stat_is_parsed_from_the_last_parenthesis() {
        let s = parse_task_stat(b"42 (a b) c)) S 1 42 42 0 -1 4194560 0 0 0 0 0 0 0 0 0 0 0 0 17")
            .unwrap();
        assert_eq!((s.state, s.exiting, s.kernel_thread), ('S', false, false));
        assert_eq!(s.start_ticks, 17);
        let k = parse_task_stat(b"2 (kthreadd) S 0 0 0 0 -1 2129984 0 0 0 0 0 0 0 0 0 0 0 0 8")
            .unwrap();
        assert!(k.kernel_thread, "2129984 has PF_KTHREAD");
        let x = parse_task_stat(b"7 (w) R 1 7 7 0 -1 4194372 0 0 0 0 0 0 0 0 0 0 0 0 9").unwrap();
        assert!(x.exiting && x.state == 'R', "4194372 has PF_EXITING");
        for bad in [
            &b"42 (x S 1"[..],
            b"42 (x) ",
            b"42 (x) SS 1 2 3 4 5 6",
            b"42 (x) S 1 2",
        ] {
            assert!(
                parse_task_stat(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let own = std::fs::read(format!("/proc/self/task/{}/stat", std::process::id())).unwrap();
        assert!(parse_task_stat(&own).is_ok());
    }

    #[test]
    fn statuses_are_classified_from_stat() {
        assert_eq!(task_status(&zombie()), Ok(Status::Exited));
        assert_eq!(task_status(&dead()), Ok(Status::Exited));
        assert_eq!(task_status(&exiting()), Ok(Status::Exiting));
        assert_eq!(task_status(&live()), Ok(Status::Live));
        assert_eq!(
            task_status(&task('S', false, true)),
            Ok(Status::KernelThread)
        );
        assert_eq!(
            task_status(&Err(errno(Errno::ENOENT))),
            Ok(Status::Vanished)
        );
        assert_eq!(task_status(&Err(errno(Errno::ESRCH))), Ok(Status::Vanished));
        for e in [Errno::EACCES, Errno::EPERM, Errno(12), Errno::EINVAL] {
            assert_eq!(task_status(&Err(errno(e))), Err(errno(e)), "{e}");
        }
        assert!(Status::Exited.ended() && Status::Vanished.ended());
        assert!(!Status::Exiting.ended() && !Status::Live.ended());
        assert!(Status::Live.usable() && Status::KernelThread.usable());
        assert!(!Status::Exiting.usable() && !Status::Exited.usable());
    }

    #[test]
    fn listings_are_complete_only_when_counted_and_confirmed() {
        assert!(complete(&[10, 11]).complete());
        assert!(!counted(&[10], 2).complete(), "a task missed");
        assert!(
            !Listing::new(vec![10, 11], 2, &[10]).complete(),
            "11 released after the count"
        );
        assert!(!complete(&[]).complete(), "a count of 0 proves nothing");
        assert!(
            Listing::new(vec![11, 10, 10], 2, &[10, 11, 12]).complete(),
            "duplicates and late tasks"
        );
    }

    #[test]
    fn a_tid_reused_during_a_read_forces_a_new_round() {
        let stat_reads = Cell::new(0);
        let data_reads = Cell::new(0);
        let got = scan(
            10,
            rounds(vec![complete(&[10])]),
            |_| {
                let n = data_reads.get();
                data_reads.set(n + 1);
                Ok(if n == 0 { "old task" } else { "new task" })
            },
            |_| {
                let n = stat_reads.get();
                stat_reads.set(n + 1);
                let mut stat = live().expect("live stat");
                stat.start_ticks = if n == 0 { 1 } else { 2 };
                Ok(stat)
            },
            Want::EveryTask,
        );
        assert_eq!(got, Ok(vec!["new task"]));
        assert_eq!(data_reads.get(), 2, "the first read was discarded");
    }

    /// A pin from an earlier round can still name the old task after the
    /// numeric tid has been recycled. Both pinned stats then say ENOENT;
    /// treating that pair as a harmless vanished task would accept a
    /// complete numeric listing without reading the new task.
    #[test]
    fn a_reused_tid_missing_from_its_pinned_handle_is_not_trusted() {
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            |_| Ok("read by number"),
            |tid| {
                if tid == 11 {
                    Err(errno(Errno::ENOENT))
                } else {
                    live()
                }
            },
            Want::EveryTask,
        );
        assert!(failed_saying(&got, "task 11 changed identity"), "{got:?}");
    }

    #[test]
    fn first_live_read_does_not_accept_a_reappeared_tid() {
        let stat_reads = Cell::new(0);
        let data_reads = Cell::new(0);
        let got = scan(
            10,
            rounds(vec![complete(&[10])]),
            |_| {
                let n = data_reads.get();
                data_reads.set(n + 1);
                Ok(if n == 0 { "old map" } else { "new map" })
            },
            |_| {
                let n = stat_reads.get();
                stat_reads.set(n + 1);
                if n == 0 {
                    Err(errno(Errno::ENOENT))
                } else {
                    live()
                }
            },
            Want::FirstLiveTask,
        );
        assert_eq!(got, Ok(vec!["new map"]));
        assert_eq!(data_reads.get(), 2);
    }

    #[test]
    fn a_worker_is_read_when_the_main_thread_exited() {
        let read = |t| Ok(if t == 11 { "held" } else { "" });
        let stat = |t| if t == 10 { zombie() } else { live() };
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            read,
            stat,
            Want::EveryTask,
        );
        assert!(got.as_ref().is_ok_and(|v| v.contains(&"held")), "{got:?}");
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            read,
            stat,
            Want::FirstLiveTask,
        );
        assert_eq!(got, Ok(vec!["held"]));
    }

    #[test]
    fn results_of_tasks_that_exit_during_the_scan_are_kept() {
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            |t| Ok(if t == 11 { "held" } else { "x" }),
            |t| if t == 11 { zombie() } else { live() },
            Want::EveryTask,
        );
        assert!(got.as_ref().is_ok_and(|v| v.contains(&"held")), "{got:?}");
    }

    /// An exiting leader (state R, `PF_EXITING`) shows an empty map; the
    /// worker that still maps the file is the source.
    #[test]
    fn an_exiting_task_is_never_the_source_of_the_memory_map() {
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            |t| Ok(if t == 10 { "empty" } else { "maps" }),
            |t| if t == 10 { exiting() } else { live() },
            Want::FirstLiveTask,
        );
        assert_eq!(got, Ok(vec!["maps"]));
    }

    #[test]
    fn every_task_exiting_is_failed_not_gone() {
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            |_| Ok("partial"),
            |_| exiting(),
            Want::EveryTask,
        );
        assert!(
            failed_saying(&got, "no task of a complete listing was live"),
            "{got:?}"
        );
    }

    /// The decision rule for Gone: `POLLHUP` is final, `POLLIN` only with a
    /// later `status` read showing a zombie alone in its group, and the
    /// `status` read is made after the poll, never without a readable one.
    #[test]
    fn exited_only_on_hangup_or_a_readable_poll_confirmed_by_status() {
        let never = || -> Result<Option<StatusFields>, ProcError> { panic!("status read") };
        assert_eq!(has_exited(PidfdPoll::Running, never), Ok(false));
        assert_eq!(has_exited(PidfdPoll::Hangup, never), Ok(true));
        let readable = |shown| has_exited(PidfdPoll::Readable, || shown);
        assert_eq!(readable(status('Z', 1)), Ok(true), "a zombie alone");
        for (shown, why) in [
            (
                status('Z', 2),
                "the old leader of an exec, the exec'ing thread counted",
            ),
            (status('Z', 0), "a task already released"),
            (status('X', 1), "not a zombie"),
            (status('S', 1), "the survivor of an exec"),
            (status('R', 2), "live"),
            // Both seen on the real kernel during exec loops (mped, round 10).
            (status('R', 0), "a running task whose count was read as 0"),
            (status('D', 2), "uninterruptible, with a thread counted"),
            (Ok(None), "status gone: released (before 6.9) or hidden"),
        ] {
            assert_eq!(readable(shown), Ok(false), "{why}");
        }
        let denied = ProcError::Denied(errno(Errno::EACCES));
        assert_eq!(readable(Err(denied.clone())), Err(denied));
    }

    /// Gone comes only from the process's pidfd: at the start, or when a
    /// complete listing holds nothing live, or when the listing vanished.
    #[test]
    fn gone_only_when_the_pidfd_says_so() {
        let hangup = Pidfd {
            poll: || Ok(PidfdPoll::Hangup),
            status: no_status,
        };
        let got = scan_exited(
            10,
            || panic!("no listing"),
            hangup,
            |_| panic!("no read"),
            |_| panic!("no stat"),
            Want::EveryTask,
        );
        assert_eq!(got, Err(ProcError::Gone));
        // A zombie group: the leader is all that is left.
        let zombie_alone = Pidfd {
            poll: polls(vec![PidfdPoll::Running, PidfdPoll::Readable]),
            status: || status('Z', 1),
        };
        let got = scan_exited(
            10,
            rounds(vec![complete(&[10])]),
            zombie_alone,
            |_| Ok(""),
            |_| zombie(),
            Want::EveryTask,
        );
        assert_eq!(got, Err(ProcError::Gone));
        // The same listing, but the pidfd never says readable: never Gone.
        let got = scan(
            10,
            rounds(vec![complete(&[10])]),
            |_| Ok(""),
            |_| zombie(),
            Want::EveryTask,
        );
        assert!(failed(&got), "{got:?}");
        // The listing vanished and the process was released.
        let released = Pidfd {
            poll: polls(vec![PidfdPoll::Running, PidfdPoll::Hangup]),
            status: no_status,
        };
        let got = scan_exited(
            10,
            || Ok(None),
            released,
            |_| Ok(""),
            |_| live(),
            Want::EveryTask,
        );
        assert_eq!(got, Err(ProcError::Gone));
    }

    /// Every incomplete round asks the pidfd before reading another
    /// listing, including a changed task count with a live task.
    #[test]
    fn incomplete_round_polls_before_the_next_listing() {
        let polls = Cell::new(0);
        let pidfd = Pidfd {
            poll: || {
                let next = polls.get() + 1;
                polls.set(next);
                Ok(if next == 1 {
                    PidfdPoll::Running
                } else {
                    PidfdPoll::Hangup
                })
            },
            status: no_status,
        };
        let got = scan_exited(
            10,
            rounds(vec![counted(&[10], 2)]),
            pidfd,
            |_| Ok("fd"),
            |_| live(),
            Want::EveryTask,
        );
        assert_eq!(got, Err(ProcError::Gone));
        assert_eq!(polls.get(), 2);
    }

    /// Round 9 (Codex, mped): `pidfd_poll` can say readable while the process
    /// lives, when it found the old leader of a non-leader exec just before
    /// the swap. One such poll at the start, with the live survivor's
    /// `status` read after it, is never Gone.
    #[test]
    fn one_transient_readable_poll_is_never_gone() {
        let transient = Pidfd {
            poll: polls(vec![PidfdPoll::Readable, PidfdPoll::Running]),
            status: || status('S', 1),
        };
        let got = scan_exited(
            10,
            rounds(vec![complete(&[10])]),
            transient,
            |_| Ok("held"),
            |_| live(),
            Want::EveryTask,
        );
        assert_eq!(got, Ok(vec!["held"]));
        // Readable in every round while the old leader shows Z with the
        // exec'ing thread still counted: Failed, not Gone.
        let window = Pidfd {
            poll: || Ok(PidfdPoll::Readable),
            status: || status('Z', 2),
        };
        let got = scan_exited(
            10,
            rounds(vec![complete(&[10, 11])]),
            window,
            |_| Ok(""),
            |_| zombie(),
            Want::EveryTask,
        );
        assert!(failed(&got), "{got:?}");
    }

    /// Codex 1 (round 8): a stalled exec CPU keeps the swap window open, and
    /// both tids keep showing the old leader as Z. The process lives on.
    #[test]
    fn a_zombie_window_held_open_is_never_gone() {
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            |_| Ok(""),
            |_| zombie(),
            Want::EveryTask,
        );
        assert!(failed(&got), "{got:?}");
    }

    /// Codex 2 (round 8): [10] with a count of 1, then [10] with a count of 2
    /// (U created and released while listed, V created and never listed).
    #[test]
    fn a_later_count_above_the_listing_is_never_gone() {
        let got = scan(
            10,
            rounds(vec![counted(&[10], 1), counted(&[10], 2)]),
            |_| Ok(""),
            |_| zombie(),
            Want::EveryTask,
        );
        assert!(failed(&got), "{got:?}");
    }

    /// Codex 3 (round 8): hidepid=2 and PR_SET_DUMPABLE(0): every lookup says
    /// ENOENT and the pid is gone from /proc, but the process lives.
    #[test]
    fn invisible_in_proc_is_never_gone() {
        let got = scan(10, || Ok(None), |_| Ok(""), |_| live(), Want::EveryTask);
        assert!(failed_saying(&got, "its task listing vanished"), "{got:?}");
    }

    #[test]
    fn an_incomplete_listing_is_never_the_answer() {
        let got = scan(
            10,
            rounds(vec![counted(&[10], 2)]),
            |_| Ok("x"),
            |_| live(),
            Want::EveryTask,
        );
        assert!(
            failed_saying(&got, "not complete (1 tasks, 2 counted)"),
            "{got:?}"
        );
        let got = scan(
            10,
            rounds(vec![Listing::new(vec![10, 11], 2, &[10])]),
            |_| Ok("x"),
            |_| live(),
            Want::EveryTask,
        );
        assert!(
            failed_saying(&got, "its task listing changed between reads"),
            "unconfirmed: {got:?}"
        );
        let got = scan(
            10,
            rounds(vec![complete(&[]), complete(&[10])]),
            |_| Ok("held"),
            |_| live(),
            Want::EveryTask,
        );
        assert_eq!(got, Ok(vec!["held"]), "a count of 0, then a real listing");
    }

    /// mped (round 4): listings cut short (a task released before it was
    /// emitted); the count exposes them, and thread 13 is read once listed.
    #[test]
    fn a_listing_shorter_than_the_thread_count_is_not_trusted() {
        let got = scan(
            10,
            rounds(vec![
                counted(&[10, 11], 3),
                counted(&[10, 11], 3),
                complete(&[10, 11, 13]),
            ]),
            |tid| Ok(if tid == 13 { "private" } else { "shared" }),
            |_| live(),
            Want::EveryTask,
        );
        assert!(
            got.as_ref().is_ok_and(|v| v.contains(&"private")),
            "{got:?}"
        );
    }

    /// mped (round 8): the exec'ing thread is never listed (the first count
    /// exposes it; later listings are [10] with a count of 1) and the
    /// leader's tid was read before the swap. The leader is read again in
    /// every round, so the stale read is never the whole answer.
    #[test]
    fn a_leader_read_is_never_carried_across_an_unseen_exec() {
        let swapped = Cell::new(false);
        let got = scan(
            10,
            rounds(vec![counted(&[10], 2), complete(&[10])]),
            |_| {
                Ok(if swapped.get() {
                    "survivor's private table"
                } else {
                    "old leader's table"
                })
            },
            |_| {
                swapped.set(true);
                live()
            },
            Want::EveryTask,
        );
        assert!(
            got.as_ref()
                .is_ok_and(|v| v.contains(&"survivor's private table")),
            "{got:?}"
        );
    }

    /// The in-round leader re-read (Codex, round 7): the round listed a third
    /// thread 20, which the exec killed (`de_thread` waits for every other
    /// thread before the swap), so it has vanished when read. The leader's
    /// tid is read live before the swap, the exec'ing thread's old tid 11 in
    /// the swap window (Z). The round's listing is complete, so without
    /// reading the leader's tid again in this round the old leader's read
    /// would be the answer and the survivor's private table missed.
    #[test]
    fn a_thread_seen_ended_makes_the_leader_be_read_again_in_the_round() {
        let swapped = Cell::new(false);
        let leader_reads = Cell::new(0);
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11, 20])]),
            |tid| match (tid, swapped.get()) {
                (10, false) => {
                    leader_reads.set(leader_reads.get() + 1);
                    Ok("old leader's data")
                }
                (10, true) => {
                    leader_reads.set(leader_reads.get() + 1);
                    Ok("survivor's private table")
                }
                (20, _) => Err(errno(Errno::ENOENT)),
                _ => Ok(""),
            },
            |tid| match tid {
                11 => {
                    swapped.set(true);
                    zombie()
                }
                20 => Err(errno(Errno::ENOENT)),
                _ => live(),
            },
            Want::EveryTask,
        );
        assert!(
            failed_saying(&got, "task 20 changed identity"),
            "an already vanished pinned task cannot establish the round: {got:?}"
        );
        assert!(
            leader_reads.get() >= 2,
            "the leader must still be read again after seeing another task end"
        );
    }

    /// The same with the old leader already marked dead (X).
    #[test]
    fn a_thread_seen_dead_makes_the_leader_be_read_again_in_the_round() {
        let swapped = Cell::new(false);
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            |tid| {
                Ok(if tid == 10 && swapped.get() {
                    "survivor's table"
                } else {
                    "old leader's table"
                })
            },
            |tid| {
                if tid == 10 {
                    live()
                } else {
                    swapped.set(true);
                    dead()
                }
            },
            Want::EveryTask,
        );
        assert!(
            got.as_ref().is_ok_and(|v| v.contains(&"survivor's table")),
            "{got:?}"
        );
    }

    /// A non-leader exec seen from the start: the leader's tid shows the old
    /// leader as Z, the exec'ing thread's old tid then shows it too (swap
    /// window), and the leader's tid read again is the survivor.
    #[test]
    fn a_tid_taken_over_by_exec_is_read_as_the_survivor() {
        let swapped = Cell::new(false);
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            |tid| {
                Ok(if tid == 10 && swapped.get() {
                    "survivor's table"
                } else {
                    ""
                })
            },
            |tid| match tid {
                10 if swapped.get() => live(),
                10 => zombie(),
                _ => {
                    swapped.set(true);
                    zombie()
                }
            },
            Want::EveryTask,
        );
        assert!(
            got.as_ref().is_ok_and(|v| v.contains(&"survivor's table")),
            "{got:?}"
        );
    }

    /// Round 5: during a non-leader exec, maps of the leader's tid can be
    /// bound to the dead old leader (empty) while its stat shows the live
    /// exec'ing thread. An empty map is never the answer; once released, the
    /// survivor's map is read.
    #[test]
    fn an_empty_memory_map_of_a_live_task_is_never_the_answer() {
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11]), complete(&[10])]),
            reads(vec![
                (10, vec![Ok(""), Ok(""), Ok("mapping")]),
                (11, vec![Ok("")]),
            ]),
            |tid| if tid == 10 { live() } else { dead() },
            Want::FirstLiveTask,
        );
        assert_eq!(got, Ok(vec!["mapping"]));
    }

    /// The cost of that rule for the environment: a process whose environment
    /// is really empty (`env -i`) is Failed, and the error says why.
    #[test]
    fn an_empty_environment_is_never_the_answer() {
        let got = scan(
            10,
            rounds(vec![complete(&[10])]),
            |_| Ok(""),
            |_| live(),
            Want::FirstLiveTask,
        );
        assert!(
            failed_saying(
                &got,
                "task 10 is live but gave an empty or incomplete table"
            ),
            "{got:?}"
        );
    }

    #[test]
    fn a_live_task_that_cannot_be_inspected_is_failed_not_gone() {
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            reads(vec![
                (10, vec![Err(errno(Errno::ENOENT))]),
                (11, vec![Ok("x")]),
            ]),
            |_| live(),
            Want::EveryTask,
        );
        assert!(failed(&got), "{got:?}");
    }

    /// The leader's entry vanishing (a lookup racing a non-leader exec) is
    /// not trusted; the next round reads it.
    #[test]
    fn the_leaders_entry_vanishing_is_read_again() {
        let stats = Cell::new(0);
        let got = scan(
            10,
            rounds(vec![complete(&[10])]),
            |_| Ok("held"),
            |_| {
                stats.set(stats.get() + 1);
                if stats.get() == 1 {
                    Err(errno(Errno::ENOENT))
                } else {
                    live()
                }
            },
            Want::EveryTask,
        );
        assert!(got.as_ref().is_ok_and(|v| v.contains(&"held")), "{got:?}");
    }

    #[test]
    fn a_stat_error_propagates_instead_of_meaning_exited() {
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            |_| Ok("x"),
            |tid| {
                if tid == 11 {
                    Err(errno(Errno::EACCES))
                } else {
                    live()
                }
            },
            Want::EveryTask,
        );
        assert!(matches!(got, Err(ProcError::Denied(_))), "{got:?}");
        let got = scan(
            10,
            rounds(vec![complete(&[10])]),
            |_| Ok("x"),
            |_| Err(errno(Errno(5))),
            Want::EveryTask,
        );
        assert!(failed(&got), "{got:?}");
    }

    #[test]
    fn permission_errors_are_denied_and_kernel_threads_hold_nothing() {
        let got = scan(
            10,
            rounds(vec![complete(&[10])]),
            |_| Err(errno(Errno::EACCES)),
            |_| live(),
            Want::EveryTask,
        );
        assert!(matches!(got, Err(ProcError::Denied(_))), "{got:?}");
        let got = scan(
            2,
            rounds(vec![complete(&[2])]),
            |_| Err(errno(Errno::ENOENT)),
            |_| task('S', false, true),
            Want::EveryTask,
        );
        assert_eq!(got, Ok(vec![]));
    }

    /// TASK-306: a kernel thread has no memory map or environment of its
    /// own, and Linux 6.18 answers its `environ` with `ESRCH` (no mm; the
    /// vm-real-kernel check, as root). A read bracketed by `stat`s that show
    /// `PF_KTHREAD` with one start time is the answer: nothing, in one
    /// round, not sixteen.
    #[test]
    fn a_kernel_threads_missing_environment_is_answered_in_one_round() {
        for error in [Errno::ESRCH, Errno::ENOENT] {
            let made = Cell::new(0);
            let got = scan(
                2,
                rounds(vec![complete(&[2])]),
                |_| {
                    made.set(made.get() + 1);
                    Err(errno(error))
                },
                |_| task('S', false, true),
                Want::FirstLiveTask,
            );
            assert_eq!(got, Ok(vec![]), "{error:?}");
            assert_eq!(made.get(), 1, "{error:?}: one read, no retries");
        }
    }

    /// The same `ESRCH` from a user task (live by its `stat`s, no
    /// `PF_KTHREAD`) is not trusted and keeps its retries (TASK-306).
    #[test]
    fn a_user_tasks_missing_environment_is_still_retried() {
        let made = Cell::new(0);
        let got = scan(
            10,
            rounds(vec![complete(&[10])]),
            |_| {
                made.set(made.get() + 1);
                Err(errno(Errno::ESRCH))
            },
            |_| live(),
            Want::FirstLiveTask,
        );
        assert!(
            failed_saying(&got, "task 10 is live but its table said ESRCH"),
            "{got:?}"
        );
        assert_eq!(made.get(), MAX_ROUNDS);
    }

    /// A kernel thread whose task was replaced during the read (another
    /// start time) is not the answer either: that round is untrusted.
    #[test]
    fn a_reused_kernel_thread_tid_is_not_the_answer() {
        let stats = Cell::new(0u64);
        let got = scan(
            2,
            rounds(vec![complete(&[2])]),
            |_| Err(errno(Errno::ESRCH)),
            |_| {
                stats.set(stats.get() + 1);
                let mut stat = task('S', false, true)?;
                stat.start_ticks = stats.get();
                Ok(stat)
            },
            Want::FirstLiveTask,
        );
        assert!(failed_saying(&got, "changed identity"), "{got:?}");
    }

    /// TASK-306 review: `PF_KTHREAD` must show in both `stat`s around the
    /// read; one shown only after it is not the answer.
    #[test]
    fn a_kernel_thread_flag_seen_only_after_the_read_is_not_the_answer() {
        let stats = Cell::new(0u32);
        let got = scan(
            2,
            rounds(vec![complete(&[2])]),
            |_| Err(errno(Errno::ESRCH)),
            |_| {
                stats.set(stats.get() + 1);
                task('S', false, stats.get().is_multiple_of(2))
            },
            Want::FirstLiveTask,
        );
        assert!(failed_saying(&got, "changed identity"), "{got:?}");
    }

    /// Dead (`X`) is never final: neither the answer's source nor "exited".
    #[test]
    fn a_dead_task_is_never_final() {
        let got = scan(
            10,
            rounds(vec![complete(&[10, 11])]),
            |_| Ok(""),
            |_| dead(),
            Want::EveryTask,
        );
        assert!(failed(&got), "{got:?}");
    }

    #[test]
    fn a_task_list_that_never_settles_is_failed_after_the_bound() {
        let calls = Cell::new(0);
        let got = scan(
            10,
            || {
                calls.set(calls.get() + 1);
                Ok(Some(counted(&[10], 2)))
            },
            |_| Ok("x"),
            |_| live(),
            Want::EveryTask,
        );
        assert!(
            failed_saying(&got, &format!("no trustworthy read in {MAX_ROUNDS} rounds")),
            "{got:?}"
        );
        assert_eq!(calls.get(), MAX_ROUNDS);
    }

    /// Only the answering round's reads are returned: a read from an
    /// earlier, incomplete round is not carried into the answer.
    #[test]
    fn only_the_answering_rounds_reads_are_returned() {
        let got = scan(
            10,
            rounds(vec![counted(&[10], 2), complete(&[10])]),
            reads(vec![(10, vec![Ok("first round"), Ok("second round")])]),
            |_| live(),
            Want::EveryTask,
        );
        assert_eq!(got, Ok(vec!["second round"]));
    }

    #[test]
    fn a_round_says_what_it_lacks() {
        let trusted = |tids: &[u32]| -> BTreeMap<u32, Seen> {
            tids.iter()
                .map(|&t| (t, Seen::Trusted(Status::Live)))
                .collect()
        };
        assert_eq!(
            shortfall(&complete(&[10, 11]), &trusted(&[10, 11]), Want::EveryTask),
            None
        );
        assert_eq!(
            shortfall(
                &Listing::new(vec![10, 11], 2, &[10]),
                &trusted(&[10, 11]),
                Want::EveryTask
            ),
            Some(Shortfall::Changed)
        );
        assert_eq!(
            shortfall(&counted(&[10], 2), &trusted(&[10]), Want::EveryTask),
            Some(Shortfall::Miscounted {
                listed: 1,
                counted: 2
            })
        );
        let mut seen = trusted(&[10, 11]);
        seen.insert(11, Seen::Empty);
        assert_eq!(
            shortfall(&complete(&[10, 11]), &seen, Want::EveryTask),
            Some(Shortfall::Untrusted {
                tid: 11,
                seen: Seen::Empty
            })
        );
        let ended: BTreeMap<u32, Seen> = [(10, Seen::Trusted(Status::Exited))].into();
        assert_eq!(
            shortfall(&complete(&[10]), &ended, Want::EveryTask),
            Some(Shortfall::NothingLive)
        );
    }

    /// The owner comes from a task that is live before and after the
    /// read: an exited leader (root-owned, no memory map) is passed over
    /// for a live worker; a task that starts exiting during the read is
    /// not trusted; with no live task there is no answer.
    #[test]
    fn the_fd_owner_comes_from_a_task_live_around_the_read() {
        // Leader 10 exited (pthread_exit): root-owned; worker 11 live.
        let got = owner_through_live_task(
            &[10, 11],
            |tid| if tid == 10 { zombie() } else { live() },
            |tid| Ok(Some(if tid == 10 { 0 } else { 1000 })),
        );
        assert_eq!(got, Ok(Some(1000)));
        // The only task starts exiting while its owner is read.
        let calls = Cell::new(0);
        let got = owner_through_live_task(
            &[10],
            |_| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 { live() } else { exiting() }
            },
            |_| Ok(Some(0)),
        );
        assert_eq!(got, Ok(None), "an owner read during exit is not trusted");
        // Nothing live: no answer; vanished tasks are passed over.
        assert_eq!(
            owner_through_live_task(&[10, 11], |_| dead(), |_| Ok(Some(0))),
            Ok(None)
        );
        let got = owner_through_live_task(
            &[10, 11],
            |_| live(),
            |tid| {
                if tid == 10 {
                    Err(errno(Errno::ENOENT))
                } else {
                    Ok(Some(1000))
                }
            },
        );
        assert_eq!(got, Ok(Some(1000)));
        // Any other error is passed on.
        assert!(owner_through_live_task(&[10], |_| live(), |_| Err(errno(Errno::EIO))).is_err());
    }

    #[test]
    fn fd_owner_skips_a_tid_reused_while_its_directory_is_read() {
        let first_tid_stats = Cell::new(0);
        let got = owner_through_live_task(
            &[10, 11],
            |tid| {
                let mut result = live().expect("live task");
                if tid == 10 {
                    let read = first_tid_stats.get();
                    first_tid_stats.set(read + 1);
                    result.start_ticks = if read == 0 { 1 } else { 2 };
                }
                Ok(result)
            },
            |tid| Ok(Some(if tid == 10 { 0 } else { 1000 })),
        );
        assert_eq!(got, Ok(Some(1000)));
        assert_eq!(first_tid_stats.get(), 2);
    }

    /// TASK-115: a task the kernel says shares the reference's table is
    /// counted by the reference's read, without a read of its own, and the
    /// round is still the answer.
    #[test]
    fn a_task_sharing_the_references_table_is_not_read_again() {
        let reads = RefCell::new(Vec::new());
        let got = scan_sharing(
            10,
            rounds(vec![complete(&[10, 11, 12])]),
            |tid| {
                reads.borrow_mut().push(tid);
                Ok("table")
            },
            |_| live(),
            |reference, tid| reference == 10 && tid != 10,
            Want::EveryTask,
        );
        assert_eq!(got, Ok(vec!["table"]));
        assert_eq!(*reads.borrow(), vec![10], "only the reference is read");
    }

    /// Every way the comparison can fail to show the same table makes the
    /// task be read in full: the kernel says no (or cannot say), the task
    /// is not live around it, its start time changes, the reference is
    /// gone or replaced.
    #[test]
    fn a_task_not_proven_to_share_is_read_in_full() {
        type StatFn = fn(u32, usize) -> Result<TaskStat, HostError>;
        let cases: [(&str, bool, StatFn); 5] = [
            ("kcmp says different or fails", false, |_, _| live()),
            ("the task is a zombie at its second stat", true, |tid, n| {
                if tid == 11 && n >= 1 {
                    zombie()
                } else {
                    live()
                }
            }),
            ("the task's start time changes", true, |tid, n| {
                let mut stat = live().expect("live");
                stat.start_ticks = if tid == 11 && n >= 1 { 2 } else { 1 };
                Ok(stat)
            }),
            (
                "the reference is gone after the comparison",
                true,
                |tid, n| {
                    if tid == 10 && n >= 2 {
                        Err(errno(Errno::ESRCH))
                    } else {
                        live()
                    }
                },
            ),
            (
                "the reference's number names another task",
                true,
                |tid, n| {
                    let mut stat = live().expect("live");
                    stat.start_ticks = if tid == 10 && n >= 2 { 9 } else { 1 };
                    Ok(stat)
                },
            ),
        ];
        for (why, shares, stat) in cases {
            let reads = RefCell::new(Vec::new());
            let stats: RefCell<BTreeMap<u32, usize>> = RefCell::new(BTreeMap::new());
            let _ = scan_sharing(
                10,
                rounds(vec![complete(&[10, 11])]),
                |tid| {
                    reads.borrow_mut().push(tid);
                    Ok("table")
                },
                |tid| {
                    let n = {
                        let mut stats = stats.borrow_mut();
                        let n = stats.entry(tid).or_default();
                        *n += 1;
                        *n - 1
                    };
                    stat(tid, n)
                },
                |_, _| shares,
                Want::EveryTask,
            );
            assert!(
                reads.borrow().contains(&11),
                "{why}: task 11 must be read in full, reads {:?}",
                reads.borrow()
            );
        }
    }

    /// Only a read that was kept, trusted and live can be the reference:
    /// an empty table of a live task is untrusted, so the next task is read
    /// in full and becomes the reference.
    #[test]
    fn an_untrusted_read_is_never_the_reference() {
        let reads = RefCell::new(Vec::new());
        let compared = RefCell::new(Vec::new());
        let _ = scan_sharing(
            10,
            rounds(vec![complete(&[10, 11, 12])]),
            |tid| {
                reads.borrow_mut().push(tid);
                Ok(if tid == 10 { "" } else { "table" })
            },
            |_| live(),
            |reference, tid| {
                compared.borrow_mut().push((reference, tid));
                true
            },
            Want::EveryTask,
        );
        assert!(
            compared
                .borrow()
                .iter()
                .all(|(reference, _)| *reference != 10),
            "the untrusted read of 10 was used as the reference: {:?}",
            compared.borrow()
        );
        assert!(
            reads.borrow().starts_with(&[10, 11]),
            "{:?}",
            reads.borrow()
        );
    }

    /// The leader's re-read after a task was seen ended looks for an
    /// exec's survivor under the leader's tid: it is always a read of its
    /// own, and a reference never crosses into another round.
    #[test]
    fn the_leaders_re_read_and_a_new_round_never_share() {
        let reads = RefCell::new(Vec::new());
        let compared = RefCell::new(Vec::new());
        let _ = scan_sharing(
            10,
            rounds(vec![complete(&[10, 11, 12]), complete(&[10, 12])]),
            |tid| {
                reads.borrow_mut().push(tid);
                Ok("table")
            },
            |tid| if tid == 11 { zombie() } else { live() },
            |reference, tid| {
                compared.borrow_mut().push((reference, tid));
                true
            },
            Want::EveryTask,
        );
        // Round 1: 10 read (reference), 11 a zombie (not live: read), 12
        // shared, then the leader read again in full.
        assert_eq!(reads.borrow()[..3], [10, 11, 10], "{:?}", reads.borrow());
        assert!(
            !compared.borrow().contains(&(10, 10)),
            "the leader was compared with itself: {:?}",
            compared.borrow()
        );
    }

    /// The same when the reference is another task than the leader: the
    /// leader's re-read is a full read even though the kernel would say it
    /// shares the reference's table.
    #[test]
    fn the_leaders_re_read_is_full_also_with_another_reference() {
        let leader_reads = Cell::new(0);
        let _ = scan_sharing(
            10,
            rounds(vec![complete(&[10, 11, 12])]),
            |tid| {
                if tid == 10 {
                    leader_reads.set(leader_reads.get() + 1);
                    // Empty: untrusted, so 11 becomes the reference.
                    return Ok(if leader_reads.get() == 1 {
                        ""
                    } else {
                        "survivor"
                    });
                }
                Ok("table")
            },
            |tid| if tid == 12 { zombie() } else { live() },
            |_, _| true,
            Want::EveryTask,
        );
        assert_eq!(leader_reads.get(), 2, "the leader's re-read was skipped");
    }

    /// The memory map and the environment are read from one live task;
    /// nothing is ever taken from another task's read there.
    #[test]
    fn first_live_reads_never_ask_whether_tasks_share() {
        let got = scan_sharing(
            10,
            rounds(vec![complete(&[10, 11])]),
            |_| Ok("map"),
            |_| live(),
            |_, _| panic!("shares asked for a first-live read"),
            Want::FirstLiveTask,
        );
        assert_eq!(got, Ok(vec!["map"]));
    }

    /// A first-live read needs no counted or confirmed listing: a plain
    /// listing's first live task answers, and when none does, the reason
    /// is the tasks' reads, not the listing.
    #[test]
    fn first_live_reads_use_a_plain_listing() {
        let got = scan(
            10,
            rounds(vec![Listing::candidates(vec![10, 11])]),
            |_| Ok("map"),
            |_| live(),
            Want::FirstLiveTask,
        );
        assert_eq!(got, Ok(vec!["map"]));
        let got = scan(
            10,
            rounds(vec![Listing::candidates(vec![10])]),
            |_| Ok(""),
            |_| live(),
            Want::FirstLiveTask,
        );
        assert!(
            failed_saying(
                &got,
                "task 10 is live but gave an empty or incomplete table"
            ),
            "{got:?}"
        );
        // The same plain listing never makes an every-task round the answer.
        let got = scan(
            10,
            rounds(vec![Listing::candidates(vec![10])]),
            |_| Ok("table"),
            |_| live(),
            Want::EveryTask,
        );
        assert!(failed_saying(&got, "listing changed"), "{got:?}");
    }
}
