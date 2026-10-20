//! The process scan: every visible process read once through the `/proc`
//! readers of the port, then classified (docs/safety.md, In use).
//!
//! Shape of a scan: rounds, each one a listing of the processes, then the
//! reads it calls for, until a listing calls for nothing:
//!
//! 1. **Every pid new in the listing is read.** A process P listed and then
//!    read after it forked C (C inherits P's fd on X) and closed X shows
//!    nothing, but C was born before P's read, so a later listing shows it
//!    and it is read. This is the process-level analogue of the per-task
//!    rounds of the readers.
//! 2. **Every process not settled is read again**, after the pause of
//!    [`REREAD_PAUSES`] for its attempt: one that could not be read
//!    (`Failed` or `Denied`: an exiting process turns `Gone`, one caught in
//!    an exec turns readable), and one whose only failed reader was its
//!    environment. Its status is read with each failed read. Only another
//!    user's process (refused, none of its uids the user's) settles at
//!    once; every other refusal is read again, since an exec makes any
//!    process briefly look non-dumpable.
//! 3. A process settles when it was read (environment included), found
//!    gone, excused as another user's, or out of attempts; then it is
//!    classified ([`Unreadable`], [`excuse`], [`consistent`]): a
//!    non-dumpable process whose ids are all the user's, a setuid process
//!    and one more capable than diskgc while both are in the initial user
//!    namespace are noted and do not block, if every status read of it
//!    said so; anything else blocks every candidate.
//!
//! Re-reads and the status reads that can find a process gone are made
//! before a listing, never after the last one, so a child forked during
//! them is listed and read. Every read only adds: the uses found by any
//! read of a process count, also when a later read of it fails or finds it
//! gone. After [`MAX_LISTINGS`] listings that each brought new pids, the
//! scan is *unsettled* and every candidate counts as in use; listings made
//! only for re-reads do not count against that bound (they are bounded by
//! the re-reads per process).
//!
//! Each process is read by [`crate::host::read_proc_view`] (fds first; the
//! first reader that fails decides the process, which keeps the cost of
//! the many processes of other users at one failed read each), through
//! [`Host::proc_views`], which `LinuxHost` runs on several processes at
//! once.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::time::Duration;

use crate::host::{
    DevIno, FileType, Host, HostError, MonoTime, Pid, ProcError, ProcLink, ProcStatus, ProcView,
};

use super::environ;

/// The most listings that bring new pids a scan makes before it gives up
/// on a process list that keeps growing. Listings that only lead to
/// re-reads of processes already listed are not counted: those are bounded
/// by the re-reads per process ([`REREAD_PAUSES`]; once more for a pid
/// found gone and listed again). A machine that starts processes faster
/// than a listing and the reads of its new pids take is unsettled; every
/// candidate then counts as in use.
pub const MAX_LISTINGS: usize = 16;

/// Pauses before the re-reads of a process that could not be read, one
/// re-read after each (40 ms in all): an exiting process turns `Gone` and
/// an exec finishes well within that.
pub const REREAD_PAUSES: [Duration; 3] = [
    Duration::from_millis(2),
    Duration::from_millis(8),
    Duration::from_millis(30),
];

/// How a process uses an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum How {
    /// An open file descriptor of some task.
    Fd(u32),
    Cwd,
    Root,
    Exe,
    /// A memory mapping.
    Map,
}

impl std::fmt::Display for How {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fd(n) => write!(f, "open as fd {n}"),
            Self::Cwd => f.write_str("as its working directory"),
            Self::Root => f.write_str("as its root directory"),
            Self::Exe => f.write_str("as its executable"),
            Self::Map => f.write_str("mapped"),
        }
    }
}

/// One use of an object by a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use {
    pub pid: Pid,
    pub how: How,
}

/// A path named in a process's initial environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvPath {
    pub pid: Pid,
    pub var: OsString,
    /// One absolute component of the value (values are split at `:`).
    pub path: PathBuf,
}

/// A file a process holds open after its last name was removed (link count
/// 0): its space comes back only when the process closes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldDeleted {
    pub pid: Pid,
    pub id: DevIno,
    /// Allocated bytes (`blocks` × 512; 0 if not reported).
    pub bytes: u64,
    /// The path the process opened it by, with ` (deleted)`; for messages.
    pub text: OsString,
}

/// Why a process that could not be read does not block anything; each one
/// is reported ([`super::Scan::notes`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Excuse {
    /// Refused, and none of its ids is one of the users: another user's
    /// process, whose use of the users' files is not visible
    /// (docs/safety.md, Visibility).
    OtherUser,
    /// Non-dumpable, every uid one of the users (`ssh-agent`, `gpg-agent`,
    /// `(sd-pam)`): denied by the kernel, accepted unseen (user decision
    /// 2026-10-17).
    NonDumpable,
    /// Its effective uid is not one of the users while another of its ids
    /// is (a setuid program run by the user: `sudo`, `fusermount3`):
    /// accepted unseen (user decision 2026-10-17).
    SetUid,
    /// Both it and diskgc in the initial user namespace (both `uid_map`s
    /// the full identity map) and holding permitted capabilities that
    /// diskgc's effective set lacks, so the kernel refuses to show it to
    /// diskgc (`cap_ptrace_access_check`): more privileged than the user,
    /// like a setuid process (`systemd --user` holds `CAP_WAKE_ALARM`).
    /// Accepted unseen, as the setuid case.
    Privileged,
}

impl Excuse {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OtherUser => "another user's process",
            Self::NonDumpable => "non-dumpable process of the user",
            Self::SetUid => "setuid process (effective uid not the user's)",
            Self::Privileged => "process holding capabilities diskgc lacks",
        }
    }
}

/// A process that could not be read, after every re-read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub pid: Pid,
    /// The command name, when its status could be read.
    pub name: Option<OsString>,
    /// The reader's error.
    pub error: ProcError,
    /// What its status says (`None`: the status could not be read either).
    pub status: Option<ProcStatus>,
    /// Why it does not block; `None`: it blocks every candidate.
    pub excuse: Option<Excuse>,
}

/// What a readable process showed of itself as text: its command and the
/// link text of its working directories and executables (one per task that
/// has its own). Text is a heuristic's material only (it is wrong in other
/// mount namespaces); uses are matched by (device, inode).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    /// The command: the last name of its executable's path, without
    /// ` (deleted)` (for messages and name heuristics).
    pub command: OsString,
    /// Link texts of `cwd`, as `/proc` shows them.
    pub cwds: Vec<PathBuf>,
    /// Link texts of `exe` (may end in ` (deleted)`).
    pub exes: Vec<PathBuf>,
}

/// What a readable process uses.
#[derive(Debug, Clone, Default)]
pub(crate) struct Read {
    pub seen: Seen,
    pub uses: Vec<(DevIno, How)>,
    pub environ: Vec<(OsString, PathBuf)>,
    pub deleted: Vec<(DevIno, u64, OsString)>,
    /// The environment could not be read (`Failed`) while every other
    /// reader worked; the heuristic then has no evidence from it.
    pub environ_failed: Option<HostError>,
}

/// One process's read.
#[derive(Debug, Clone)]
pub(crate) enum Outcome {
    Read(Read),
    Gone,
    Unreadable(ProcError),
}

/// The result of reading every process.
#[derive(Debug, Clone)]
pub(crate) struct Gathered {
    pub finished: MonoTime,
    pub listings: usize,
    /// `None` when a listing added nothing; otherwise why the scan is
    /// unsettled.
    pub unsettled: Option<String>,
    pub read: BTreeMap<Pid, Read>,
    pub unreadable: Vec<Unreadable>,
    /// Readable processes whose environment no read gave.
    pub environ_failed: Vec<(Pid, HostError)>,
    /// Processes found gone.
    pub gone: usize,
}

/// One process during a scan.
#[derive(Debug, Default)]
struct Entry {
    /// The union of the uses found by every read of it.
    found: Read,
    /// Some read gave its environment.
    environ_read: bool,
    /// What the latest read said (`None`: not read yet).
    latest: Option<Latest>,
    /// The status read with the latest failed read.
    status: Option<Result<ProcStatus, ProcError>>,
    /// The excuse each status read gave, in order (`None`: none, or the
    /// status could not be read): see [`consistent`].
    excuses: Vec<Option<Excuse>>,
    attempts: usize,
    settled: bool,
    /// When the next re-read is due.
    due: Option<MonoTime>,
    /// The pid was found gone and listed again (reused) in this scan, and
    /// is being read as a new process.
    reused: bool,
}

#[derive(Debug, Clone)]
enum Latest {
    Read,
    Gone,
    Unreadable(ProcError),
}

impl Entry {
    /// Adds what one read found.
    fn merge(&mut self, outcome: Outcome) {
        self.attempts += 1;
        self.latest = Some(match outcome {
            Outcome::Read(read) => {
                if read.environ_failed.is_none() {
                    self.environ_read = true;
                }
                self.found.add(read);
                Latest::Read
            }
            Outcome::Gone => Latest::Gone,
            Outcome::Unreadable(e) => Latest::Unreadable(e),
        });
    }
}

impl Read {
    /// Adds `other`'s uses, environment paths and held deleted files to
    /// these (a later read never takes away what an earlier one found).
    fn add(&mut self, other: Read) {
        if self.seen.command.is_empty() {
            self.seen.command = other.seen.command;
        }
        for (mine, theirs) in [
            (&mut self.seen.cwds, other.seen.cwds),
            (&mut self.seen.exes, other.seen.exes),
        ] {
            for text in theirs {
                if !mine.contains(&text) {
                    mine.push(text);
                }
            }
        }
        self.uses.extend(other.uses);
        self.uses.sort_unstable();
        self.uses.dedup();
        for e in other.environ {
            if !self.environ.contains(&e) {
                self.environ.push(e);
            }
        }
        for d in other.deleted {
            if !self.deleted.iter().any(|(id, _, _)| *id == d.0) {
                self.deleted.push(d);
            }
        }
        self.environ_failed = other.environ_failed;
    }
}

/// Reads every visible process but diskgc's own (see the module docs).
/// `users`: the uids whose files candidates may hold. Fails only when the
/// process list itself cannot be read, or the host answers a read of
/// several processes with another number of results.
pub(crate) fn gather<H: Host>(host: &H, users: &[u32]) -> Result<Gathered, HostError> {
    // `users` is the candidate ownership allowlist, not the complete set of
    // processes that can use their files. In root mode a root process can
    // open any listed user's candidate, so an unreadable root process must
    // not get the OtherUser excuse. This changes only process inspection;
    // the eligibility walk still admits entries owned solely by `users`.
    let mut process_users = users.to_vec();
    if host.effective_uid() == 0 && !process_users.contains(&0) {
        process_users.push(0);
    }
    let own = host.own_pid();
    // diskgc's own capabilities and uid_map, to recognise a more capable
    // process while both are in the initial user namespace (see
    // [`Excuse::Privileged`]). Unknown: no such excuse.
    let own_status = host.proc_status(own).ok();
    let mut entries: BTreeMap<Pid, Entry> = BTreeMap::new();
    let mut listings = 0;
    let mut growing = 0;
    let unsettled = loop {
        let listing = host.pids()?;
        listings += 1;
        let mut batch: Vec<Pid> = Vec::new();
        for pid in listing {
            if pid == own {
                continue;
            }
            match entries.get_mut(&pid) {
                None => {
                    entries.insert(pid, Entry::default());
                    batch.push(pid);
                }
                // Found gone, listed again: the pid was reused (or the
                // process is a zombie not yet reaped). Read it as new,
                // once per scan, keeping what was found under it.
                Some(e) if matches!(e.latest, Some(Latest::Gone)) && !e.reused => {
                    *e = Entry {
                        found: std::mem::take(&mut e.found),
                        reused: true,
                        ..Entry::default()
                    };
                    batch.push(pid);
                }
                Some(_) => {}
            }
        }
        let new = batch.len();
        let pending: Vec<(Pid, MonoTime)> = entries
            .iter()
            .filter(|(_, e)| !e.settled && e.attempts > 0)
            .map(|(pid, e)| {
                (
                    *pid,
                    e.due.expect("an unsettled read process has a due time"),
                )
            })
            .collect();
        if batch.is_empty() && pending.is_empty() {
            break None;
        }
        if new > 0 {
            growing += 1;
            if growing == MAX_LISTINGS {
                break Some(format!(
                    "the process list was still growing after {MAX_LISTINGS} listings that \
                     brought new pids ({new} new in the last one): a process the scan did not \
                     read could have inherited any file from one it read"
                ));
            }
        }
        let mut now = host.now();
        if batch.is_empty() {
            // Only re-reads are left: wait for the first one due.
            let first = pending.iter().map(|(_, due)| *due).min().expect("pending");
            if first > now {
                host.sleep(first.saturating_since(now));
                now = host.now();
            }
        }
        batch.extend(
            pending
                .iter()
                .filter(|(_, due)| *due <= now)
                .map(|(pid, _)| *pid),
        );
        let views = host.proc_views(&batch);
        if views.len() != batch.len() {
            return Err(HostError::invalid(
                "proc_views",
                format!("{} processes", batch.len()),
                format!("the host answered with {} results", views.len()),
            ));
        }
        let mut read_back = Vec::new();
        for (pid, view) in batch.iter().zip(views) {
            let entry = entries.get_mut(pid).expect("listed");
            entry.merge(outcome(*pid, view));
            settle(
                host,
                *pid,
                entry,
                &process_users,
                own_status.as_ref(),
                host.effective_uid() == 0,
            );
            if !entry.settled {
                read_back.push(*pid);
            }
        }
        let after = host.now();
        for pid in read_back {
            let entry = entries.get_mut(&pid).expect("listed");
            let pause = REREAD_PAUSES[(entry.attempts - 1).min(REREAD_PAUSES.len() - 1)];
            entry.due = Some(after.checked_add(pause).expect("a due time fits"));
        }
    };

    let mut read = BTreeMap::new();
    let mut unreadable = Vec::new();
    let mut environ_failed = Vec::new();
    let mut gone = 0;
    for (pid, entry) in entries {
        match &entry.latest {
            Some(Latest::Gone) => gone += 1,
            Some(Latest::Unreadable(error)) => {
                let status = entry.status.clone().and_then(Result::ok);
                unreadable.push(Unreadable {
                    pid,
                    name: status.as_ref().map(|s| s.name.clone()),
                    excuse: consistent(&entry.excuses),
                    error: error.clone(),
                    status,
                });
            }
            Some(Latest::Read) if !entry.environ_read => {
                if let Some(e) = &entry.found.environ_failed {
                    environ_failed.push((pid, e.clone()));
                }
            }
            Some(Latest::Read) | None => {}
        }
        read.insert(pid, entry.found);
    }
    Ok(Gathered {
        finished: host.now(),
        listings,
        unsettled,
        read,
        unreadable,
        environ_failed,
        gone,
    })
}

/// The excuse of a process whose status was read `excuses.len()` times in
/// the scan: only one that every read gave. One read that shows no excuse
/// (dumpable after all, readable status gone missing, another error) is
/// enough to block: a read made while an exec swaps a dead leader and its
/// live survivor under one tid can show a false excuse once (task-6 review,
/// C1), and missing it on every read takes as many timed events as reads.
///
/// One exception, narrow: when the **last** read refused a process none of
/// whose uids is the user's ([`Excuse::OtherUser`], which only a `Denied`
/// read gives), that stands: a process that dropped to another user (a
/// `sudo` or `su` child read before and after its `setresuid`) is another
/// user's now, whatever earlier reads said.
pub(crate) fn consistent(excuses: &[Option<Excuse>]) -> Option<Excuse> {
    if excuses.last() == Some(&Some(Excuse::OtherUser)) {
        return Some(Excuse::OtherUser);
    }
    let first = (*excuses.first()?)?;
    excuses.iter().all(|e| *e == Some(first)).then_some(first)
}

/// Decides, after a read, whether `entry` needs another one. A failed read
/// is followed by a read of the process's status (which may find it gone);
/// only another user's refused process settles at once.
fn settle<H: Host>(
    host: &H,
    pid: Pid,
    entry: &mut Entry,
    users: &[u32],
    own: Option<&ProcStatus>,
    root_mode: bool,
) {
    let out_of_attempts = entry.attempts > REREAD_PAUSES.len();
    entry.settled = match entry.latest.clone() {
        Some(Latest::Gone) => true,
        Some(Latest::Read) => entry.environ_read || out_of_attempts,
        Some(Latest::Unreadable(error)) => {
            let status = host.proc_status(pid);
            let settled = match &status {
                Err(ProcError::Gone) => {
                    entry.latest = Some(Latest::Gone);
                    true
                }
                Ok(s) => {
                    let given = excuse(&error, s, users, own, root_mode);
                    entry.excuses.push(given);
                    refused_enough(entry) || given == Some(Excuse::OtherUser)
                }
                Err(_) => {
                    entry.excuses.push(None);
                    refused_enough(entry)
                }
            };
            entry.status = Some(status);
            settled
        }
        None => false,
    };
}

/// Whether a refused process has had its status read with as many failed
/// reads as a process gets in all: refusals are counted on their own, so a
/// process whose earlier reads failed only for its environment still gets
/// every classification of a late refusal.
fn refused_enough(entry: &Entry) -> bool {
    entry.excuses.len() > REREAD_PAUSES.len()
}

/// Whether any id of the process (real, effective, saved or filesystem
/// uid) is one of `users`: it may hold their files.
pub(crate) fn is_users(status: &ProcStatus, users: &[u32]) -> bool {
    status.uid.all().iter().any(|uid| users.contains(uid))
}

/// Why an unreadable process does not block (see [`Excuse`]); `None`: it
/// blocks. Only a refusal (`Denied`) is ever excused, never a `Failed`
/// read (a churn read says nothing about who may read the process):
///
/// - [`Excuse::OtherUser`]: none of its uids is a user's;
/// - [`Excuse::SetUid`]: its effective uid is not a user's;
/// - [`Excuse::Privileged`]: diskgc's and its `uid_map` both read as the
///   full identity map (`0 0 4294967295`), taken as both being in the
///   initial user namespace, and it holds a permitted capability that
///   diskgc's effective set lacks, which is when the kernel refuses it to
///   diskgc. Equal map text alone proves nothing (two sibling namespaces
///   can show `1000 1000 1` each), and only real root can make another
///   namespace show the full identity map (a documented limit); in a user
///   namespace diskgc's user created, the kernel lets diskgc read a
///   process whatever it holds, so a refusal there is something else and
///   blocks. A missing map: no such excuse;
/// - [`Excuse::NonDumpable`]: every uid is a user's and the owner of a
///   live task's `fd` directory is not its effective uid. Unknown owner
///   (no live task found): no excuse.
///
/// A same-uid process refused while dumpable and unprivileged (e.g. a shell
/// running with another group, `newgrp`, which the kernel's check also
/// compares) blocks. In root mode, any uid 0 in a refused process blocks:
/// root can hold an allowed user's file regardless of setuid or capabilities.
pub(crate) fn excuse(
    error: &ProcError,
    status: &ProcStatus,
    users: &[u32],
    own: Option<&ProcStatus>,
    root_mode: bool,
) -> Option<Excuse> {
    if !matches!(error, ProcError::Denied(_)) {
        return None;
    }
    if root_mode && status.uid.all().contains(&0) {
        return None;
    }
    if !is_users(status, users) {
        return Some(Excuse::OtherUser);
    }
    if !users.contains(&status.uid.effective) {
        return Some(Excuse::SetUid);
    }
    if own.is_some_and(|own| {
        is_initial_namespace(own.uid_map.text())
            && is_initial_namespace(status.uid_map.text())
            && status.cap_permitted & !own.cap_effective != 0
    }) {
        return Some(Excuse::Privileged);
    }
    let all_users = status.uid.all().iter().all(|uid| users.contains(uid));
    let non_dumpable = status
        .fd_dir_owner
        .is_some_and(|owner| owner != status.uid.effective);
    (all_users && non_dumpable).then_some(Excuse::NonDumpable)
}

/// Whether `uid_map` is the full identity map, `0 0 4294967295` (any
/// spacing): the initial user namespace as read from it.
fn is_initial_namespace(uid_map: Option<&str>) -> bool {
    uid_map
        .is_some_and(|map| map.split_whitespace().collect::<Vec<_>>() == ["0", "0", "4294967295"])
}

/// What one process's read means for the scan.
pub(crate) fn outcome(pid: Pid, view: Result<ProcView, ProcError>) -> Outcome {
    match view.and_then(|view| uses_of(pid, view)) {
        Ok(read) => Outcome::Read(read),
        Err(ProcError::Gone) => Outcome::Gone,
        Err(e) => Outcome::Unreadable(e),
    }
}

/// The uses in a process's view. A failed environment is kept as no
/// evidence (`environ_failed`) when it is `Failed`; `Denied` there makes
/// the process unreadable.
fn uses_of(pid: Pid, view: ProcView) -> Result<Read, ProcError> {
    let mut read = Read::default();
    for fd in &view.fds {
        let id = placed(pid, &fd.target.stat, || format!("fd {}", fd.fd))?;
        read.uses.push((id, How::Fd(fd.fd)));
        if fd.target.stat.nlink == Some(0) && fd.target.stat.file_type == Some(FileType::Regular) {
            let bytes = fd.target.stat.blocks.unwrap_or(0).saturating_mul(512);
            if !read.deleted.iter().any(|(seen, _, _)| *seen == id) {
                read.deleted.push((id, bytes, fd.target.text.clone()));
            }
        }
    }
    // Inode 0: anonymous memory, the stack, the vDSO.
    for map in view.maps.iter().filter(|m| m.ino != 0) {
        let id = DevIno {
            dev: map.dev,
            ino: map.ino,
        };
        read.uses.push((id, How::Map));
    }
    for (targets, which, how) in [
        (&view.cwd, ProcLink::Cwd, How::Cwd),
        (&view.root, ProcLink::Root, How::Root),
        (&view.exe, ProcLink::Exe, How::Exe),
    ] {
        for target in targets {
            let id = placed(pid, &target.stat, || which.name().to_owned())?;
            read.uses.push((id, how));
            let text = PathBuf::from(&target.text);
            match how {
                How::Exe => {
                    if read.seen.command.is_empty() {
                        read.seen.command = command_of(&target.text);
                    }
                    if !read.seen.exes.contains(&text) {
                        read.seen.exes.push(text);
                    }
                }
                How::Cwd if !read.seen.cwds.contains(&text) => read.seen.cwds.push(text),
                _ => {}
            }
        }
    }
    read.uses.sort_unstable();
    read.uses.dedup();
    match view.environ {
        Ok(env) => read.environ = environ::paths(&env),
        Err(ProcError::Failed(e)) => read.environ_failed = Some(e),
        Err(e) => return Err(e),
    }
    Ok(read)
}

/// The (device, inode) of a link's target; a target without an inode
/// number cannot be matched, so the process counts as unreadable.
fn placed(
    pid: Pid,
    stat: &crate::host::Stat,
    what: impl FnOnce() -> String,
) -> Result<DevIno, ProcError> {
    stat.dev_ino().ok_or_else(|| {
        ProcError::Failed(HostError::invalid(
            "statx",
            format!("/proc/{pid}/{}", what()),
            "the kernel reported no inode number, so its use cannot be matched",
        ))
    })
}

/// The last name of an executable's path, without ` (deleted)`, byte for
/// byte (a name heuristic may test it: no lossy conversion).
fn command_of(exe: &OsStr) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    let bytes = exe.as_bytes();
    let bytes = bytes.strip_suffix(b" (deleted)").unwrap_or(bytes);
    let name = bytes.rsplit(|b| *b == b'/').next().unwrap_or(bytes);
    OsStr::from_bytes(name).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Ids, UidMap};

    const INIT: &str = "         0          0 4294967295\n";
    const SIBLING: &str = "      1000       1000          1\n";

    fn status(uid: u32, euid: u32, caps: u64, map: &str, owner: Option<u32>) -> ProcStatus {
        ProcStatus {
            name: "p".into(),
            uid: Ids {
                real: uid,
                effective: euid,
                saved: euid,
                fs: euid,
            },
            gid: Ids::same(100),
            cap_permitted: caps,
            cap_effective: 0,
            uid_map: if map == "none" {
                UidMap::Absent
            } else {
                UidMap::Map(map.to_owned())
            },
            fd_dir_owner: owner,
        }
    }

    fn denied() -> ProcError {
        ProcError::Denied(HostError::errno(
            "open",
            "/proc/9/fd",
            crate::host::Errno::EACCES,
        ))
    }

    /// The table of [`excuse`], each line a case the review asked for.
    #[test]
    fn excuses_need_a_refusal_and_a_lasting_explanation() {
        let me = [1000];
        let own = status(1000, 1000, 0, INIT, Some(1000));
        let own_capable = ProcStatus {
            cap_effective: 1 << 35,
            ..own.clone()
        };
        let own_sibling = ProcStatus {
            uid_map: UidMap::Map(SIBLING.to_owned()),
            ..own.clone()
        };
        let failed = ProcError::Failed(HostError::invalid("read", "x", "churn"));
        let cases = [
            // Another user's, refused: excused; failed: not (B: should-fix 4).
            (
                denied(),
                status(1001, 1001, 0, INIT, Some(1001)),
                Some(&own),
                Some(Excuse::OtherUser),
            ),
            (
                failed.clone(),
                status(1001, 1001, 0, INIT, Some(1001)),
                Some(&own),
                None,
            ),
            (
                denied(),
                status(1000, 0, 0, INIT, Some(0)),
                Some(&own),
                Some(Excuse::SetUid),
            ),
            // More capable in diskgc's namespace: excused, against diskgc's
            // EFFECTIVE set; in another namespace, or with the capability
            // in diskgc's effective set, or unknown own status: not.
            (
                denied(),
                status(1000, 1000, 1 << 35, INIT, Some(1000)),
                Some(&own),
                Some(Excuse::Privileged),
            ),
            (
                denied(),
                status(1000, 1000, 1 << 35, "0 1000 1\n", Some(1000)),
                Some(&own),
                None,
            ),
            (
                denied(),
                status(1000, 1000, 1 << 35, INIT, Some(1000)),
                Some(&own_capable),
                None,
            ),
            (
                denied(),
                status(1000, 1000, 1 << 35, INIT, Some(1000)),
                None,
                None,
            ),
            // C2: equal text of another namespace is no identity (two
            // siblings can both show `1000 1000 1`); diskgc outside the
            // initial namespace, or a map that cannot be read: no excuse.
            (
                denied(),
                status(1000, 1000, 1 << 35, SIBLING, Some(1000)),
                Some(&own_sibling),
                None,
            ),
            (
                denied(),
                status(1000, 1000, 1 << 35, INIT, Some(1000)),
                Some(&own_sibling),
                None,
            ),
            (
                denied(),
                status(1000, 1000, 1 << 35, "none", Some(1000)),
                Some(&own),
                None,
            ),
            // Non-dumpable through a live task; unknown owner (no live
            // task, B3): no excuse.
            (
                denied(),
                status(1000, 1000, 0, INIT, Some(0)),
                Some(&own),
                Some(Excuse::NonDumpable),
            ),
            (
                denied(),
                status(1000, 1000, 0, INIT, None),
                Some(&own),
                None,
            ),
            (
                denied(),
                status(1000, 1000, 0, INIT, Some(1000)),
                Some(&own),
                None,
            ),
            (
                failed,
                status(1000, 1000, 0, INIT, Some(0)),
                Some(&own),
                None,
            ),
        ];
        // QA X1: non-dumpable is excused only when ALL uids are the user's;
        // a root daemon running for the moment as the user (saved uid 0)
        // is not.
        let half = ProcStatus {
            uid: Ids {
                real: 1000,
                effective: 1000,
                saved: 0,
                fs: 1000,
            },
            ..status(1000, 1000, 0, INIT, Some(0))
        };
        assert_eq!(
            excuse(&denied(), &half, &me, Some(&own), false),
            None,
            "{half:?}"
        );
        for (i, (error, st, own, want)) in cases.into_iter().enumerate() {
            assert_eq!(
                excuse(&error, &st, &me, own, false),
                want,
                "case {i}: {st:?}"
            );
        }
    }

    /// C1: an excuse stands only if every status read gave it; one read
    /// that showed none (a dumpable owner after all) blocks.
    #[test]
    fn an_excuse_needs_every_status_read_to_agree() {
        let nd = Some(Excuse::NonDumpable);
        assert_eq!(consistent(&[nd, nd, nd, nd]), nd);
        assert_eq!(
            consistent(&[Some(Excuse::OtherUser)]),
            Some(Excuse::OtherUser)
        );
        // Owner root in one read, the user in another (an exec's tid swap
        // seen once): not excused.
        assert_eq!(consistent(&[nd, None, nd, nd]), None);
        assert_eq!(consistent(&[None, nd]), None);
        assert_eq!(consistent(&[nd, Some(Excuse::SetUid)]), None);
        assert_eq!(consistent(&[]), None);
        // mped round 3 (2): a process last refused as another user's (it
        // dropped to another user, as a sudo or su child) is that.
        let other = Some(Excuse::OtherUser);
        assert_eq!(consistent(&[Some(Excuse::SetUid), other]), other);
        assert_eq!(consistent(&[nd, None, other]), other);
        // Only as the last read: earlier, it does not excuse.
        assert_eq!(consistent(&[other, Some(Excuse::SetUid)]), None);
        assert_eq!(consistent(&[other, None]), None);
    }
}
