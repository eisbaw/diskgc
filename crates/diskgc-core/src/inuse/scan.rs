//! The process scan: every visible process incarnation read through the `/proc`
//! readers of the port, then classified (docs/safety.md, In use).
//!
//! Shape of a scan: rounds, each one a listing of the processes, then the
//! reads it calls for, until a listing calls for nothing:
//!
//! 1. **Every new process incarnation in the listing is read.** A process P listed and then
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
//! gone. A scan-lifetime identity pin distinguishes a replacement even when
//! the numeric pid listing is unchanged; release is checked after reads
//! and before accepting the final listing. After [`MAX_LISTINGS`] listings
//! that each brought new incarnations, the
//! scan is *unsettled* and every candidate counts as in use; listings made
//! only for re-reads do not count against that bound (they are bounded by
//! the re-reads per process).
//!
//! Each process is read by [`crate::host::read_proc_view`] (fds first; the
//! first reader that fails decides the process, which keeps the cost of
//! the many processes of other users at one failed read each), through
//! [`Host::proc_views`], which `LinuxHost` runs on several processes at
//! once.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::host::{
    DevIno, Environ, Errno, FileType, Host, HostError, MonoTime, Pid, ProcError, ProcLink,
    ProcStatus, ProcView,
};

use std::os::unix::ffi::OsStrExt;

use super::{ScanError, environ};
use crate::executor::CAP_SYS_PTRACE;
use crate::host::PidNamespace;
use crate::liveness::PidTable;
use crate::liveness::pytest::{self, Lookup, ProcessFacts};

/// The most listings that bring new process incarnations a scan makes before it gives up
/// on a process list that keeps growing. Listings that only lead to
/// re-reads of processes already listed are not counted: those are bounded
/// by the re-reads per process ([`REREAD_PAUSES`]). A machine that starts processes faster
/// than a listing and the reads of its new incarnations take is unsettled; every
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

/// An object a process holds through a magic link (an fd, its cwd, root
/// or exe), kept with its link text and mount id for the project
/// contracts' path tests (docs/safety.md, A process's paths); read only
/// when a selected source has one ([`super::Options::project_facts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub how: How,
    pub id: DevIno,
    /// `stx_mnt_id` of the object through the link, if reported.
    pub mnt_id: Option<u64>,
    /// The link's text, in diskgc's view (` (deleted)` included).
    pub text: OsString,
    /// The object is a directory: it is climbed, not path-tested.
    pub dir: bool,
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
    /// The mount it was opened through (`stx_mnt_id` of the descriptor),
    /// if reported: what places it in a filesystem's space
    /// ([`crate::places::space_of`]).
    pub mnt_id: Option<u64>,
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
    /// (device, inode) of each executable.
    pub exe_ids: Vec<DevIno>,
    /// (device, inode) of each root directory: whether its link texts are
    /// paths of diskgc's view (the toolchain guard's working-directory
    /// lookup, docs/safety.md, Toolchain pins).
    pub roots: Vec<DevIno>,
    /// Some read gave its initial environment.
    pub environ_read: bool,
    /// The toolchain managers' selection variables in its initial
    /// environment ([`TOOLCHAIN_VARS`]), from the latest read that gave it.
    pub toolchain_env: Vec<(OsString, OsString)>,
}

/// The variables that select a toolchain for every rustup or elan command
/// a process runs (docs/safety.md, Toolchain pins).
pub const TOOLCHAIN_VARS: [&str; 2] = ["RUSTUP_TOOLCHAIN", "ELAN_TOOLCHAIN"];

/// What a readable process uses.
#[derive(Debug, Clone, Default)]
pub(crate) struct Read {
    pub seen: Seen,
    pub uses: Vec<(DevIno, How)>,
    pub environ: Vec<(OsString, PathBuf)>,
    /// Held open with no name left: identity, mount id, bytes, link text.
    pub deleted: Vec<(DevIno, Option<u64>, u64, OsString)>,
    /// The environment could not be read (`Failed`) while every other
    /// reader worked; the heuristic then has no evidence from it.
    pub environ_failed: Option<HostError>,
    /// The whole initial environment of the latest read that gave one
    /// (the pytest contract reads its variables, not only its paths).
    pub environ_raw: Option<Environ>,
    /// The pytest contract's facts of the latest read that gave them
    /// (boxed: most reads carry none).
    pub facts: Option<Box<crate::host::ViewFacts>>,
    /// The latest read's facts lack its uids or command line (a reader
    /// failed): read again, as for a failed environment.
    pub facts_failed: bool,
    /// Two reads' facts disagreed on the uids or the command line (or a
    /// later one could not read what an earlier one did): neither stands,
    /// both are unknown (the process changed between the reads; an earlier
    /// value must not hide a later one, nor the reverse).
    pub facts_unstable: bool,
    /// Two reads gave different initial environments: neither stands, the
    /// environment is unknown to the pytest contract (as for uids).
    pub environ_unstable: bool,
    /// What it holds, for the project contracts (empty unless asked).
    pub held: Vec<Held>,
}

/// One process's read.
#[derive(Debug, Clone)]
pub(crate) enum Outcome {
    /// Boxed: a read is far larger than the other variants.
    Read(Box<Read>),
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
    /// The pids of the last listing (diskgc's own included): every process
    /// alive when the scan settled was in it.
    pub listed: Vec<Pid>,
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
                if read.environ_failed.is_none() && !read.facts_failed {
                    self.environ_read = true;
                }
                self.found.add(*read);
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
        for (mine, theirs) in [
            (&mut self.seen.exe_ids, other.seen.exe_ids),
            (&mut self.seen.roots, other.seen.roots),
        ] {
            for id in theirs {
                if !mine.contains(&id) {
                    mine.push(id);
                }
            }
        }
        if other.seen.environ_read {
            self.seen.environ_read = true;
            self.seen.toolchain_env = other.seen.toolchain_env;
        }
        self.uses.extend(other.uses);
        self.uses.sort_unstable();
        self.uses.dedup();
        // One environment can name thousands of paths. `Vec::contains` here
        // made even the first process read quadratic in that path count and
        // could exhaust the one-second deletion scan freshness window; a set
        // of copies cost two allocations per path on every merge (TASK-115).
        // So: a set of borrows decides which of `other`'s to keep, then they
        // are moved.
        let keep: Vec<bool> = {
            let mut seen: HashSet<&(OsString, PathBuf)> = self.environ.iter().collect();
            other.environ.iter().map(|e| seen.insert(e)).collect()
        };
        self.environ.extend(
            other
                .environ
                .into_iter()
                .zip(keep)
                .filter_map(|(e, keep)| keep.then_some(e)),
        );
        for d in other.deleted {
            if !self.deleted.iter().any(|(id, ..)| *id == d.0) {
                self.deleted.push(d);
            }
        }
        self.environ_failed = other.environ_failed;
        if let Some(new) = other.environ_raw {
            if self.environ_raw.as_ref().is_some_and(|old| *old != new) {
                self.environ_unstable = true;
            }
            self.environ_raw = Some(new);
        }
        // The latest read's facts stand; uids or a command line that
        // differ from an earlier read's (a failure included) are unknown,
        // and so is a lookup whose answer differs (the process's mount view
        // changed between the reads: an earlier match must not be lost to a
        // later miss, nor the reverse; Codex review of step 7).
        if let Some(mut new) = other.facts {
            if let Some(old) = &self.facts {
                self.facts_unstable |= old.uids != new.uids || old.cmdline != new.cmdline;
                merge_lookups(&old.lookups, &mut new.lookups);
            }
            self.facts = Some(new);
            self.facts_failed = other.facts_failed;
        }
        self.facts_unstable |= other.facts_unstable;
        self.environ_unstable |= other.environ_unstable;
        for held in other.held {
            if !self
                .held
                .iter()
                .any(|h| h.how == held.how && h.id == held.id)
            {
                self.held.push(held);
            }
        }
    }
}

/// One lookup of [`crate::host::ViewFacts::lookups`]: the normalised
/// path, whether it was climbed, the answer.
type RootLookup = (PathBuf, bool, Result<Vec<DevIno>, ProcError>);

/// Every read only adds, for root lookups: an answer two reads disagree on
/// becomes unresolvable (the verdict's rows 8 and 9 then fail closed), and
/// an answer only the earlier read gave is kept.
fn merge_lookups(old: &[RootLookup], new: &mut Vec<RootLookup>) {
    for (path, climb, answer) in new.iter_mut() {
        let earlier = old.iter().find(|(p, c, _)| p == path && c == climb);
        if earlier.is_some_and(|(_, _, a)| a != answer) {
            *answer = Err(ProcError::Failed(HostError::invalid(
                "look up in the process's root",
                path.display(),
                "two reads gave different answers",
            )));
        }
    }
    for entry in old {
        if !new.iter().any(|(p, c, _)| *p == entry.0 && *c == entry.1) {
            new.push(entry.clone());
        }
    }
}

/// Reads every visible process but diskgc's own (see the module docs).
/// `users`: the uids whose files candidates may hold. Fails
/// ([`ScanError::Failed`]) only when the process list itself cannot be
/// read, when diskgc runs out of file descriptors on any read of the scan
/// ([`out_of_descriptors`]), or when the host answers a read of several
/// processes with another number of results. Stops
/// ([`ScanError::DeadlinePassed`], [`ScanError::Interrupted`]) when
/// `deadline` has passed or a signal arrived, checked before every listing
/// and after every pause: a scan of up to [`MAX_LISTINGS`] growing
/// listings and their re-reads can take seconds on a busy host (TASK-189).
pub(crate) fn gather<H: Host>(
    host: &H,
    users: &[u32],
    facts: bool,
    project: bool,
    deadline: Option<MonoTime>,
) -> Result<Gathered, ScanError> {
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
    // [`Excuse::Privileged`]). Unknown: no such excuse; but diskgc out of
    // descriptors is an error of the run, not an unknown status (TASK-185).
    let own_status = match host.proc_status(own) {
        Ok(status) => Some(status),
        Err(error) => {
            if let Some(exhausted) = out_of_descriptors(&error) {
                return Err(ScanError::Failed(exhausted));
            }
            None
        }
    };
    let mut entries: BTreeMap<Pid, Entry> = BTreeMap::new();
    let mut identities: BTreeMap<Pid, H::ProcIdentity> = BTreeMap::new();
    let mut listings = 0;
    let mut growing = 0;
    let mut last_listing: Vec<Pid> = Vec::new();
    macro_rules! identity_or_unsettled {
        ($label:lifetime, $answer:expr) => {
            match $answer {
                Ok(value) => value,
                Err(error) => {
                    if let Some(exhausted) = out_of_descriptors(&error) {
                        return Err(ScanError::Failed(exhausted));
                    }
                    break $label Some(format!(
                        "a process identity could not be checked: {}",
                        identity_error(error)
                    ))
                }
            }
        };
    }
    let unsettled = 'scan: loop {
        // Bounded: at most MAX_LISTINGS growing listings, and listings for
        // re-reads bounded by REREAD_PAUSES per process; each is as long
        // as its reads, so the deadline and signals are checked per round.
        ScanError::stop_if_due(host, deadline)?;
        let listing = host.pids()?;
        last_listing.clone_from(&listing);
        let listed: BTreeSet<Pid> = listing.iter().copied().collect();
        listings += 1;
        let mut batch: Vec<Pid> = Vec::new();
        let mut vanished_before_read = 0;
        for pid in listing {
            if pid == own {
                continue;
            }
            let released = if let Some(identity) = identities.get(&pid) {
                identity_or_unsettled!('scan, host.proc_identity_released(identity))
            } else {
                true
            };
            if released {
                identities.remove(&pid);
                let entry = entries.entry(pid).or_default();
                *entry = Entry {
                    found: std::mem::take(&mut entry.found),
                    ..Entry::default()
                };
                match host.proc_identity(pid) {
                    Ok(identity) => {
                        identities.insert(pid, identity);
                        batch.push(pid);
                    }
                    Err(ProcError::Gone) => {
                        entry.latest = Some(Latest::Gone);
                        entry.settled = true;
                        vanished_before_read += 1;
                    }
                    Err(e) => identity_or_unsettled!('scan, Err::<(), _>(e)),
                }
            }
        }
        // A process absent from the listing may have been released while
        // another process forked a replacement at the same number. Clear
        // its settled state now; the replacement must be seen in a later
        // listing before this scan can finish.
        let mut absent_released = Vec::new();
        for (pid, identity) in &identities {
            if !listed.contains(pid)
                && identity_or_unsettled!('scan, host.proc_identity_released(identity))
            {
                absent_released.push(*pid);
            }
        }
        for pid in absent_released {
            identities.remove(&pid);
            let entry = entries.get_mut(&pid).expect("pinned process was listed");
            entry.latest = Some(Latest::Gone);
            entry.settled = true;
            vanished_before_read += 1;
        }
        let new = batch.len() + vanished_before_read;
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
        if new > 0 {
            growing += 1;
            if growing == MAX_LISTINGS {
                break Some(format!(
                    "the process list was still growing or replacing pids after {MAX_LISTINGS} listings that \
                     brought new process incarnations ({new} new in the last one): a process the scan did not \
                     read could have inherited any file from one it read"
                ));
            }
        }
        if batch.is_empty() && pending.is_empty() {
            if vanished_before_read > 0 {
                // A listed process vanished before it could be read. A
                // child it forked can be absent from this listing.
                continue;
            }
            // A release after the listing (including a replacement with
            // exactly the same numeric pid set) invalidates the apparent
            // fixed point. Re-list before accepting it.
            let mut released = false;
            for identity in identities.values() {
                released |= identity_or_unsettled!('scan, host.proc_identity_released(identity));
            }
            if released {
                continue;
            }
            break None;
        }
        let mut now = host.now();
        if batch.is_empty() {
            // Only re-reads are left: wait for the first one due.
            let first = pending.iter().map(|(_, due)| *due).min().expect("pending");
            if first > now {
                host.sleep(first.saturating_since(now));
                ScanError::stop_if_due(host, deadline)?;
                now = host.now();
            }
        }
        batch.extend(
            pending
                .iter()
                .filter(|(_, due)| *due <= now)
                .map(|(pid, _)| *pid),
        );
        let views = host.proc_views(&batch, facts);
        if views.len() != batch.len() {
            return Err(ScanError::Failed(HostError::invalid(
                "proc_views",
                format!("{} processes", batch.len()),
                format!("the host answered with {} results", views.len()),
            )));
        }
        if let Some(exhausted) = views.iter().find_map(view_out_of_descriptors) {
            return Err(ScanError::Failed(exhausted));
        }
        let mut read_back = Vec::new();
        for (pid, view) in batch.iter().zip(views) {
            let entry = entries.get_mut(pid).expect("listed");
            entry.merge(outcome(*pid, view, project));
            let released = if let Some(identity) = identities.get(pid) {
                identity_or_unsettled!('scan, host.proc_identity_released(identity))
            } else {
                false
            };
            if released {
                // The read may have crossed into a replacement. Keep its
                // uses, but discard all settled/excuse state and re-list.
                identities.remove(pid);
                entry.latest = Some(Latest::Gone);
                entry.settled = true;
                continue;
            }
            settle(
                host,
                *pid,
                entry,
                &process_users,
                own_status.as_ref(),
                host.effective_uid() == 0,
            );
            if let Some(Err(error)) = &entry.status
                && let Some(exhausted) = out_of_descriptors(error)
            {
                return Err(ScanError::Failed(exhausted));
            }
            let released = if let Some(identity) = identities.get(pid) {
                identity_or_unsettled!('scan, host.proc_identity_released(identity))
            } else {
                false
            };
            if released {
                identities.remove(pid);
                entry.latest = Some(Latest::Gone);
                entry.settled = true;
                entry.excuses.clear();
                continue;
            }
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
        listed: last_listing,
    })
}

/// Owner PID's lookup table (docs/safety.md, Producer liveness, Owner PID
/// step 5) of a scan whose listings ran after `before` was read: the
/// `NSpid` of every process of its last listing. The caller confirms the
/// namespace facts with a read made afterwards
/// ([`crate::liveness::confirmed_namespace`]): facts that changed between the two
/// reads (a `/proc` remounted mid-scan) or failed either read are unknown,
/// so the listings they guard are never taken as complete.
///
/// Ordering the caller relies on: a process alive when the last listing
/// was made is in it; one that ends after it has ended for good; a process
/// born after it with the same number is a reuse, which only the probe in
/// diskgc's namespace sees. So the candidate whose owner is looked up must
/// have been found before the scan's first listing (TASK-112 step 3
/// asserts it where the table is used).
///
/// When the facts already fail a precondition, no `NSpid` is read: every
/// answer is unknown anyway. Otherwise unreadable processes are read again
/// in rounds, one pause of [`REREAD_PAUSES`] per round for all of them (at
/// most 40 ms per scan, whatever their number); one that stays unreadable,
/// whatever its user, is recorded and makes every answer unknown. diskgc
/// out of file descriptors in any of these reads is an error of the run
/// (TASK-185), never an unknown answer; the deadline and signals are
/// checked after each pause, as in [`gather`].
pub(crate) fn pid_table<H: Host>(
    host: &H,
    before: Result<PidNamespace, String>,
    listed: &[Pid],
    settled: bool,
    deadline: Option<MonoTime>,
) -> Result<PidTable, ScanError> {
    let mut table = PidTable::new(before, settled);
    if !table.preconditions_hold() {
        return Ok(table);
    }
    let mut pending: Vec<Pid> = listed.to_vec();
    let mut pauses = REREAD_PAUSES.iter();
    loop {
        let mut failed = Vec::new();
        for pid in pending {
            match host.proc_nspid(pid) {
                Ok(levels) => table.add_levels(&levels),
                Err(ProcError::Gone) => {}
                Err(ProcError::Denied(_) | ProcError::Failed(_)) => failed.push(pid),
                Err(ProcError::Exhausted(e)) => {
                    return Err(ScanError::Failed(with_descriptor_advice(e)));
                }
            }
        }
        if failed.is_empty() {
            break;
        }
        match pauses.next() {
            Some(pause) => {
                host.sleep(*pause);
                ScanError::stop_if_due(host, deadline)?;
                pending = failed;
            }
            None => {
                for pid in failed {
                    table.add_unreadable(pid);
                }
                break;
            }
        }
    }
    Ok(table)
}

/// The PID namespace facts as [`PidTable`] and the in-use check's
/// visibility take them (a failed read: its reason, which makes every
/// answer unknown and blocks every candidate), or diskgc out of file
/// descriptors as an error of the run (TASK-185).
pub(crate) fn namespace_or_run_error(
    read: Result<PidNamespace, HostError>,
) -> Result<Result<PidNamespace, String>, ScanError> {
    match read {
        Ok(facts) => Ok(Ok(facts)),
        Err(e) if e.is_out_of_descriptors() => Err(ScanError::Failed(with_descriptor_advice(e))),
        Err(e) => Ok(Err(e.to_string())),
    }
}

/// The pytest contract's process facts (docs/safety.md, pytest sessions:
/// terms; A process's paths) of a gathered scan. Every process some read
/// of which gave [`crate::host::ViewFacts`] (read within the scan's rounds,
/// [`crate::host::read_proc_view`], so a child forked before the process
/// ended is listed and read too; every read only adds: the latest read
/// that gave facts stands, also for a process found gone later) and whose
/// uids are among `users` or unknown: its uids, command line, initial
/// environment, working directories and lookups; and every unreadable
/// process not excused at a privilege boundary, as
/// [`ProcessFacts::unreadable`] (it already blocks every candidate in the
/// in-use check). A reader that failed is `None` (or an unresolvable
/// lookup), which the verdict treats as undecidable. Pure: no I/O.
pub(crate) fn pytest_facts(gathered: &Gathered, users: &[u32]) -> Vec<ProcessFacts> {
    let blocking: BTreeSet<Pid> = gathered
        .unreadable
        .iter()
        .filter(|u| u.excuse.is_none())
        .map(|u| u.pid)
        .collect();
    let mut facts = Vec::new();
    for (pid, read) in &gathered.read {
        if blocking.contains(pid) {
            continue;
        }
        let Some(view) = &read.facts else {
            // Facts come with every successful read when asked for; a read
            // that found something without them would be a bug: fail
            // closed (undecidable), never drop the process.
            if !read.uses.is_empty() || read.environ_raw.is_some() {
                debug_assert!(false, "pid {pid}: a successful read without facts");
                facts.push(ProcessFacts::unreadable(*pid));
            }
            continue;
        };
        let stable = !read.facts_unstable;
        let uids = view.uids.as_ref().ok().copied().filter(|_| stable);
        if uids.is_some_and(|ids| !is_users_ids(ids, users)) {
            continue;
        }
        let environ: Option<Environ> = (read.environ_failed.is_none() && !read.environ_unstable)
            .then(|| read.environ_raw.clone())
            .flatten();
        let cmdline = view.cmdline.as_ref().ok().cloned().filter(|_| stable);
        let answer = |path: &Path, climb: bool| -> Lookup {
            match view
                .lookups
                .iter()
                .find(|(p, c, _)| p == path && *c == climb)
            {
                Some((_, _, Ok(chain))) => Lookup::Resolved(chain.clone()),
                Some((_, _, Err(e))) => Lookup::Unresolvable(e.to_string()),
                None => Lookup::Unresolvable("not looked up in this read".to_owned()),
            }
        };
        let lookup = |value: &OsStr, climb: bool| match pytest::normalize(value.as_bytes()) {
            None => Lookup::Relative,
            Some(path) => answer(&path, climb),
        };
        let mut process = ProcessFacts {
            pid: *pid,
            uids,
            cmdline,
            environ,
            cwds: read
                .uses
                .iter()
                .filter(|(_, how)| *how == How::Cwd)
                .map(|(id, _)| *id)
                .collect(),
            temp_roots: Vec::new(),
            fallback_roots: Vec::new(),
            fallback_unresolved: Vec::new(),
            basetemps: Vec::new(),
        };
        for value in process
            .environ
            .as_ref()
            .map(pytest::temp_root_values)
            .unwrap_or_default()
        {
            let found = lookup(&value, false);
            process.temp_roots.push((value, found));
        }
        for root in pytest::FALLBACK_ROOTS {
            match view
                .lookups
                .iter()
                .find(|(p, c, _)| p.as_os_str() == root && !*c)
                .map(|(_, _, answer)| answer)
            {
                Some(Ok(chain)) => process.fallback_roots.extend(chain.first()),
                // Missing: not a root Python could use. ESRCH: the process's
                // root is a dead process's /proc directory (Chromium's
                // sandbox chroots its renderers into one, 66 processes on
                // the development host), where nothing resolves for the
                // process either.
                Some(Err(ProcError::Failed(e) | ProcError::Denied(e)))
                    if matches!(
                        e.kernel_errno(),
                        Some(Errno::ENOENT | Errno::ENOTDIR | Errno::ESRCH)
                    ) => {}
                Some(Err(e)) => process.fallback_unresolved.push(format!("{root}: {e}")),
                None => process
                    .fallback_unresolved
                    .push(format!("{root}: not looked up in this read")),
            }
        }
        for (source, value) in
            pytest::basetemp_values(process.cmdline.as_deref(), process.environ.as_ref())
        {
            let found = lookup(&value, true);
            process.basetemps.push((source, value, found));
        }
        facts.push(process);
    }
    for u in &gathered.unreadable {
        if u.excuse.is_none() {
            let mut process = ProcessFacts::unreadable(u.pid);
            process.uids = u.status.as_ref().map(|s| s.uid);
            facts.push(process);
        }
    }
    facts.sort_by_key(|p| p.pid);
    facts
}

/// Whether any of the four uids is one of `users`.
pub(crate) fn is_users_ids(ids: crate::host::Ids, users: &[u32]) -> bool {
    ids.all().iter().any(|uid| users.contains(uid))
}

/// diskgc itself out of file descriptors ([`ProcError::Exhausted`]): an
/// error of the run, never a fact about the process being read. It says
/// nothing about this process and would hit the next one too, so taking it
/// as an unreadable process or an unsettled scan would quietly skip every
/// candidate (TASK-166); the walk's rule for the same errors is the same
/// (docs/safety.md, Fail closed on anything unreadable). The error comes
/// back with what the operator can do about it.
fn out_of_descriptors(error: &ProcError) -> Option<HostError> {
    match error {
        ProcError::Exhausted(e) => Some(with_descriptor_advice(e.clone())),
        ProcError::Gone | ProcError::Denied(_) | ProcError::Failed(_) => None,
    }
}

/// `e` (`EMFILE` or `ENFILE`) with what the operator can do about it.
pub(crate) fn with_descriptor_advice(e: HostError) -> HostError {
    let advice = if e.is(Errno::EMFILE) {
        "the in-use scan holds a descriptor for every visible process and ran out under \
         diskgc's open-file limit (RLIMIT_NOFILE): raise its hard limit"
    } else if e.is(Errno::ENFILE) {
        "the in-use scan holds a descriptor for every visible process and the system's \
         open-file table (fs.file-max) is full"
    } else {
        "diskgc ran out of file descriptors"
    };
    let mut annotated = e;
    annotated.detail = if annotated.detail.is_empty() {
        advice.to_owned()
    } else {
        format!("{}; {advice}", annotated.detail)
    };
    annotated
}

/// [`out_of_descriptors`] of any reader of one process's view: the view
/// itself, its environment, and the pytest contract's facts.
fn view_out_of_descriptors(view: &Result<ProcView, ProcError>) -> Option<HostError> {
    let view = match view {
        Ok(view) => view,
        Err(error) => return out_of_descriptors(error),
    };
    let facts = view.facts.iter().flat_map(|facts| {
        [facts.uids.as_ref().err(), facts.cmdline.as_ref().err()]
            .into_iter()
            .chain(
                facts
                    .lookups
                    .iter()
                    .map(|(_, _, answer)| answer.as_ref().err()),
            )
    });
    std::iter::once(view.environ.as_ref().err())
        .chain(facts)
        .flatten()
        .find_map(out_of_descriptors)
}

fn identity_error(error: ProcError) -> HostError {
    match error {
        ProcError::Gone => HostError::invalid(
            "check process identity",
            "held process",
            "a held process identity returned Gone instead of a release result",
        ),
        ProcError::Denied(e) | ProcError::Failed(e) | ProcError::Exhausted(e) => e,
    }
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
    is_users_ids(status.uid, users)
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
/// None of the last three when diskgc's effective set holds
/// `CAP_SYS_PTRACE` (`--allow-root` requires it): the kernel then refuses
/// none of them, so a refusal is unexplained and blocks.
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
    // With CAP_SYS_PTRACE the kernel's ptrace-read check passes for every
    // process of diskgc's user namespace, setuid, more capable or not
    // dumpable alike: the refusal came from something else (an LSM), and
    // excuses none of the three below.
    if own.is_some_and(|own| own.cap_effective & (1 << CAP_SYS_PTRACE) != 0) {
        return None;
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
pub(crate) fn outcome(pid: Pid, view: Result<ProcView, ProcError>, project: bool) -> Outcome {
    match view.and_then(|view| uses_of(pid, view, project)) {
        Ok(read) => Outcome::Read(Box::new(read)),
        Err(ProcError::Gone) => Outcome::Gone,
        Err(e) => Outcome::Unreadable(e),
    }
}

/// The uses in a process's view. A failed environment is kept as no
/// evidence (`environ_failed`) when it is `Failed`; `Denied` there makes
/// the process unreadable.
fn uses_of(pid: Pid, view: ProcView, project: bool) -> Result<Read, ProcError> {
    let mut read = Read::default();
    let mut hold = |how: How, id: DevIno, target: &crate::host::LinkTarget| {
        if project {
            read.held.push(Held {
                how,
                id,
                mnt_id: target.stat.mnt_id,
                text: target.text.clone(),
                dir: target.stat.file_type == Some(FileType::Directory),
            });
        }
    };
    for fd in &view.fds {
        let id = placed(pid, &fd.target.stat, || format!("fd {}", fd.fd))?;
        hold(How::Fd(fd.fd), id, &fd.target);
    }
    for (targets, which, how) in [
        (&view.cwd, ProcLink::Cwd, How::Cwd),
        (&view.root, ProcLink::Root, How::Root),
        (&view.exe, ProcLink::Exe, How::Exe),
    ] {
        for target in targets {
            hold(
                how,
                placed(pid, &target.stat, || which.name().to_owned())?,
                target,
            );
        }
    }
    for fd in &view.fds {
        let id = placed(pid, &fd.target.stat, || format!("fd {}", fd.fd))?;
        read.uses.push((id, How::Fd(fd.fd)));
        if fd.target.stat.nlink == Some(0) && fd.target.stat.file_type == Some(FileType::Regular) {
            let bytes = fd.target.stat.blocks.unwrap_or(0).saturating_mul(512);
            if !read.deleted.iter().any(|(seen, ..)| *seen == id) {
                let mnt_id = fd.target.stat.mnt_id;
                read.deleted
                    .push((id, mnt_id, bytes, fd.target.text.clone()));
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
                    if !read.seen.exe_ids.contains(&id) {
                        read.seen.exe_ids.push(id);
                    }
                }
                How::Cwd if !read.seen.cwds.contains(&text) => read.seen.cwds.push(text),
                How::Root if !read.seen.roots.contains(&id) => read.seen.roots.push(id),
                _ => {}
            }
        }
    }
    read.uses.sort_unstable();
    read.uses.dedup();
    if let Some(facts) = view.facts {
        read.facts_failed = facts.uids.is_err() || facts.cmdline.is_err();
        read.facts = Some(Box::new(facts));
    }
    match view.environ {
        Ok(env) => {
            read.environ = environ::paths(&env);
            read.seen.environ_read = true;
            read.seen.toolchain_env = env
                .vars()
                .filter(|(name, _)| TOOLCHAIN_VARS.iter().any(|v| *name == OsStr::new(v)))
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect();
            read.environ_raw = Some(env);
        }
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

    #[test]
    fn merging_many_environment_paths_keeps_first_seen_order_and_deduplicates() {
        use std::os::unix::ffi::OsStrExt;
        use std::path::Path;

        let mut merged = Read::default();
        let mut first = Read {
            environ: (0..4_500)
                .map(|i| ("TMPDIR".into(), format!("/tmp/branch-{i}/leaf").into()))
                .collect(),
            ..Read::default()
        };
        first.environ.push(first.environ[17].clone());
        merged.add(first);
        assert_eq!(merged.environ.len(), 4_500);

        let raw = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
        let second = Read {
            environ: vec![
                ("TMPDIR".into(), "/tmp/branch-17/leaf".into()),
                ("TMPDIR".into(), "/tmp/new/leaf".into()),
                ("OTHER".into(), "/tmp/branch-17/leaf".into()),
                ("RAW".into(), raw.clone()),
                ("RAW".into(), raw.clone()),
            ],
            ..Read::default()
        };
        merged.add(second);
        assert_eq!(merged.environ.len(), 4_503);
        assert_eq!(merged.environ[17].1, Path::new("/tmp/branch-17/leaf"));
        assert_eq!(merged.environ[4_500].1, Path::new("/tmp/new/leaf"));
        assert_eq!(merged.environ[4_501].0, "OTHER");
        assert_eq!(merged.environ[4_502], ("RAW".into(), raw));
    }

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

    /// Codex review of c8a0945: facts that two reads disagree on (or that a
    /// later read failed to read) are unknown, never an earlier value.
    #[test]
    fn facts_that_change_between_reads_are_unknown() {
        use crate::host::ViewFacts;
        let read = |uid: u32, cmd: Result<Vec<OsString>, ProcError>| {
            let facts = ViewFacts {
                uids: Ok(Ids::same(uid)),
                cmdline: cmd,
                lookups: Vec::new(),
            };
            Read {
                facts_failed: facts.cmdline.is_err(),
                facts: Some(Box::new(facts)),
                environ_raw: Some(Environ::default()),
                ..Read::default()
            }
        };
        let pytest = || Ok(vec![OsString::from("pytest")]);
        let mut same = read(1000, pytest());
        same.add(read(1000, pytest()));
        assert!(!same.facts_unstable);
        let mut changed_uid = read(0, pytest());
        changed_uid.add(read(1000, pytest()));
        assert!(changed_uid.facts_unstable);
        let mut lost_cmdline = read(1000, pytest());
        lost_cmdline.add(read(
            1000,
            Err(ProcError::Failed(HostError::invalid("read", "x", "y"))),
        ));
        assert!(lost_cmdline.facts_unstable);
        let gathered = Gathered {
            finished: MonoTime(Duration::ZERO),
            listings: 1,
            unsettled: None,
            read: [(Pid(7), changed_uid)].into_iter().collect(),
            unreadable: Vec::new(),
            environ_failed: Vec::new(),
            gone: 0,
            listed: vec![Pid(7)],
        };
        let facts = pytest_facts(&gathered, &[2000]);
        assert_eq!(facts.len(), 1, "an unstable uid is unknown: in every set");
        assert_eq!(facts[0].uids, None);
        assert_eq!(facts[0].cmdline, None);
    }

    /// Codex review of step 7: a live `--basetemp` or temp-root match by
    /// identity in one read must not be lost to a later read in which the
    /// process's mount view changed (the lookup now misses), nor the
    /// reverse; nor may an environment that changed between reads stand.
    /// Both become undecidable (rows 8 and 9 fail closed).
    #[test]
    fn lookups_and_environments_that_change_between_reads_are_unknown() {
        use crate::host::{Dev, DevIno, ViewFacts};
        use crate::liveness::pytest::Lookup;
        let root = PathBuf::from("/tmp/nix-shell.AB");
        let id = DevIno {
            dev: Dev {
                major: 0,
                minor: 40,
            },
            ino: 7,
        };
        let read = |env: &[u8], answer: Result<Vec<DevIno>, ProcError>| Read {
            facts: Some(Box::new(ViewFacts {
                uids: Ok(Ids::same(1000)),
                cmdline: Ok(vec![OsString::from("pytest")]),
                lookups: vec![(root.clone(), false, answer)],
            })),
            environ_raw: Some(Environ::from_block(env.to_vec())),
            ..Read::default()
        };
        let env = b"TMPDIR=/tmp/nix-shell.AB\0";
        let missing = || {
            Err(ProcError::Failed(HostError::invalid(
                "openat2",
                "/tmp/nix-shell.AB",
                "ENOENT",
            )))
        };
        let facts_of = |read: Read| {
            let gathered = Gathered {
                finished: MonoTime(Duration::ZERO),
                listings: 1,
                unsettled: None,
                read: [(Pid(7), read)].into_iter().collect(),
                unreadable: Vec::new(),
                environ_failed: Vec::new(),
                gone: 0,
                listed: vec![Pid(7)],
            };
            let facts = pytest_facts(&gathered, &[1000]);
            assert_eq!(facts.len(), 1);
            facts.into_iter().next().expect("one process")
        };

        let mut steady = read(env, Ok(vec![id]));
        steady.add(read(env, Ok(vec![id])));
        assert_eq!(
            facts_of(steady).temp_roots[0].1,
            Lookup::Resolved(vec![id]),
            "the same answer twice stands"
        );
        for (first, second) in [(Ok(vec![id]), missing()), (missing(), Ok(vec![id]))] {
            let mut moved = read(env, first);
            moved.add(read(env, second));
            assert!(
                matches!(facts_of(moved).temp_roots[0].1, Lookup::Unresolvable(_)),
                "a lookup whose answer changed between reads is unresolvable"
            );
        }
        let mut only_earlier = read(env, Ok(vec![id]));
        let mut later = read(env, Ok(vec![id]));
        if let Some(facts) = later.facts.as_mut() {
            facts.lookups.clear();
        }
        only_earlier.add(later);
        assert_eq!(
            facts_of(only_earlier).temp_roots[0].1,
            Lookup::Resolved(vec![id]),
            "an answer only the earlier read gave is kept"
        );
        let mut changed_env = read(env, Ok(vec![id]));
        changed_env.add(read(b"TMPDIR=/tmp/other\0", Ok(vec![id])));
        assert_eq!(
            facts_of(changed_env).environ,
            None,
            "an environment that changed between reads is unknown"
        );
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
