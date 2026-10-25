//! The in-use check of filesystem candidates (docs/safety.md, In use), and
//! the order of the deletion step that uses it (docs/safety.md, Deletion).
//!
//! The data first:
//!
//! - a [`Scan`] is one read of every visible process ([`scan`]; how, in
//!   [`self::scan`]): an index from (device, inode) to the processes using
//!   it (fds, cwd, root, exe, memory maps), the absolute paths their
//!   initial environments name, the processes that could not be read and
//!   why each does or does not block, the files held open after deletion,
//!   and (on request) the sockets bound in diskgc's network namespace. It
//!   records when it started: it serves candidates only while it is
//!   younger than [`FRESHNESS`] (monotonic time including suspend).
//! - [`Scan::check`] judges one candidate's [`WalkRecord`] against it:
//!   pure, no I/O, so one scan can serve several candidates while fresh.
//! - [`Checker`] keeps the latest scan and makes a new one when it is
//!   older than its reuse limit; [`clear_and_delete`] is the last step of
//!   every deletion (check against a fresh scan, then [`delete_cleared`]),
//!   and [`lock_candidate`] and [`locked_is_walked`] are the `flock` of the
//!   `fs` kind's protocol (`fs_kind::reclaim`, docs/safety.md, Deletion).
//!
//! Matching is by (device, inode) only, never by path text, which is wrong
//! in other mount namespaces. Memory maps on overlayfs and btrfs report the
//! device (and on older kernels the inode) of the layer or volume rather
//! than what `statx` reports, so on those filesystems a mapping matches by
//! inode number alone: more matches, the conservative side.

pub(crate) mod environ;
pub mod scan;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::engine::{
    self, Candidate, DeleteOptions, Deletion, IpcKind, Reason, Rules, Skip, WalkRecord,
};
use crate::host::{
    Access, Base, BoundSocket, Delete, Dev, DevIno, Errno, FileType, Host, HostError, LockMode,
    Locked, MonoTime, Pid, TryLock,
};
use crate::liveness::PidTable;
use crate::liveness::pytest::Processes;
use crate::outcome::Stop;
use crate::resolve::{ResolveError, Want, open_beneath};

pub use scan::{
    EnvPath, Excuse, HeldDeleted, How, MAX_LISTINGS, REREAD_PAUSES, Seen, Unreadable, Use,
};

/// How old a scan may be when the deletion it cleared starts
/// (docs/safety.md, In use: Freshness).
pub const FRESHNESS: Duration = Duration::from_secs(1);

/// How long a deleting [`Checker`] may reuse a scan. The final check and
/// [`delete_cleared`] still enforce [`FRESHNESS`] immediately before every
/// deletion, including when a scan expires during the candidate check. The
/// shorter reuse window gives a newly started process a chance to be seen
/// between candidates before the deletion freshness limit is reached; it
/// holds for command actions, whose program may act on anything in the
/// candidate. The parts of one Cargo target, under its locks (TASK-173),
/// and consecutive `fs` candidates of a `delete` (TASK-203) reuse a scan up
/// to [`FRESHNESS`] instead ([`Checker::reusing_for`]).
pub const REUSE_LIMIT: Duration = Duration::from_millis(500);

/// A scan that blocks every candidate is kept for less time after it finishes,
/// so a transient process problem cannot mark a whole plan or reclaim `in_use`.
/// A slow blocking scan is still shared by later candidates for that interval.
pub const BLOCKED_REUSE_LIMIT: Duration = Duration::from_millis(500);

/// A read-only plan may reuse a clear scan longer: it does not delete on
/// the scan's word. A blocking scan is kept only for [`BLOCKED_REUSE_LIMIT`].
pub const PLAN_REUSE_LIMIT: Duration = Duration::from_secs(60);

/// How many attempts a candidate gets at a fresh in-use verdict: a check
/// against a scan still fresh when it ends, and a deletion starting before
/// that clear expires ([`FreshAttempts`]). Then it is skipped
/// (`in_use_unknown`) and the run goes on: a scan slower than
/// [`FRESHNESS`] is a busy host (TASK-144: 1.3 s scans at load ~28), which
/// a few retries spaced out by [`FRESH_RETRY_PAUSES`] may outlast, but
/// one stuck that way stays so; more attempts would only hold the
/// candidate's locks (a Cargo target's) longer for nothing.
pub const FRESH_ATTEMPTS: usize = 3;

/// The pause before each retry of a stale attempt, growing (backoff): a
/// short one for a momentary spike, a longer one for a burst. Together with
/// the scans themselves, at most ~1.25 s of waiting per candidate; never
/// past the run's deadline.
pub const FRESH_RETRY_PAUSES: [Duration; FRESH_ATTEMPTS - 1] =
    [Duration::from_millis(250), Duration::from_millis(1000)];

/// One short pause before retrying a process list that kept growing. The
/// retry is a complete scan; neither scan can clear a candidate unless it
/// settles. A persistent fork storm stays a conservative `in_use_unknown`
/// skip (TASK-190).
pub const UNSETTLED_RETRY_PAUSE: Duration = Duration::from_millis(20);

/// The longest single sleep of a retry pause ([`FRESH_RETRY_PAUSES`]): a
/// signal is noticed within this, not after the whole pause (TASK-189).
/// `Host::sleep` itself is not interruptible, and must not be: loops that
/// poll until a time (a command's grace period) would spin on a sleep that
/// returns early. 50 ms is far below a person's notice and 20 wake-ups for
/// the longest pause.
pub const PAUSE_SLICE: Duration = Duration::from_millis(50);

/// Filesystems whose memory maps may show another device (and, on older
/// kernels, inode) than `statx` of the same file: a mapping matches by
/// inode number alone there.
const MAPS_BY_INODE: &[&str] = &["overlay", "btrfs"];

/// What a scan is for. Build it with [`Options::from_rules`] from the
/// walk's rules: a user list narrower than the walk's owners would excuse
/// an owner's processes as another user's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options<'a> {
    /// The users whose files candidates may hold: the walk's
    /// [`Rules::owners`] (diskgc's own uid, or `owners:` under
    /// `--allow-root`). A process none of whose uids is one of them is
    /// another user's: invisible without root, and never blocking.
    pub users: &'a [u32],
    /// Also ask for the sockets bound in diskgc's network namespace (only
    /// needed when the walk records sockets, [`Rules::block_ipc`] off).
    pub sockets: bool,
    /// Also read every listed process's `NSpid` and the PID namespace
    /// facts for Owner PID ([`Scan::pid_table`]); only when a selected
    /// source has a liveness contract (one more `status` read per process).
    pub pid_lookup: bool,
    /// Also read the pytest contract's process facts ([`Scan::pytest_facts`]:
    /// uids, command line, environment, and the temp-root and
    /// `--basetemp` lookups); only when a selected source has
    /// `liveness_contract: pytest`. Implies nothing about `pid_lookup`,
    /// which that contract also needs.
    pub pytest_facts: bool,
}

impl<'a> Options<'a> {
    /// The options matching the walk's `rules`: the users are its owners,
    /// and bound sockets are asked for exactly when the walk records
    /// sockets (`block_ipc` off). No PID lookup: a caller judging a source
    /// with a liveness contract asks for it ([`Options::with_pid_lookup`]).
    pub fn from_rules(rules: &Rules<'a>) -> Self {
        Self {
            users: rules.owners,
            sockets: !rules.block_ipc,
            pid_lookup: false,
            pytest_facts: false,
        }
    }
}

impl Options<'_> {
    /// These options, also reading what Owner PID needs.
    #[must_use]
    pub fn with_pid_lookup(self, pid_lookup: bool) -> Self {
        Self { pid_lookup, ..self }
    }

    /// These options, also reading the pytest contract's process facts.
    #[must_use]
    pub fn with_pytest_facts(self, pytest_facts: bool) -> Self {
        Self {
            pytest_facts,
            ..self
        }
    }
}

/// One read of every visible process (see the module docs).
#[derive(Debug, Clone)]
pub struct Scan {
    started: MonoTime,
    finished: MonoTime,
    listings: usize,
    unsettled: Option<String>,
    retried_unsettled: Option<String>,
    /// What the `/proc` readers reported ([`Host::take_proc_read_notes`]).
    reader_notes: Vec<String>,
    processes: usize,
    gone: usize,
    /// Every process read in full, by pid (ordered, so reports are).
    seen: BTreeMap<Pid, Seen>,
    uses: HashMap<DevIno, Vec<Use>>,
    /// Memory-map uses by inode number alone (see [`MAPS_BY_INODE`]).
    maps_by_inode: HashMap<u64, Vec<Pid>>,
    environ: Vec<EnvPath>,
    /// First environment-path match per candidate prefix, built once per
    /// scan. The original vector remains the source of truth for reports.
    environ_index: environ::EnvPathIndex,
    unreadable: Vec<Unreadable>,
    environ_failed: Vec<(Pid, HostError)>,
    held_deleted: Vec<HeldDeleted>,
    sockets: Option<Result<Vec<BoundSocket>, HostError>>,
    /// Owner PID's lookup table, when the options asked for it.
    pid_table: Option<PidTable>,
    /// The pytest contract's process facts, when the options asked for
    /// them (sorted by pid), with the users they were read for.
    pytest_facts: Option<(Vec<u32>, Processes)>,
}

/// Why a scan ended without a result: an error of the run, or the run's
/// own end (its deadline, a signal). Never a fact about a candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    /// The process list could not be read, or diskgc ran out of file
    /// descriptors (`EMFILE`, `ENFILE`) in any read of the scan (TASK-166,
    /// TASK-185).
    Failed(HostError),
    /// The run's deadline passed before the scan was done (TASK-189).
    DeadlinePassed,
    /// SIGINT or SIGTERM arrived during the scan or its pauses.
    Interrupted,
}

impl ScanError {
    /// This end of a scan made for the candidate at `path`, as the run's
    /// [`Stop`].
    pub fn at(self, path: &Path) -> Stop {
        let path = path.to_owned();
        match self {
            Self::Failed(error) => Stop::Failed { path, error },
            Self::DeadlinePassed => Stop::DeadlinePassed { path },
            Self::Interrupted => Stop::Interrupted { path },
        }
    }

    /// `Err` once a signal arrived or `deadline` passed (in that order: an
    /// interrupted run reports the signal).
    pub(crate) fn stop_if_due<H: Host>(host: &H, deadline: Option<MonoTime>) -> Result<(), Self> {
        if host.interrupted().is_some() {
            return Err(Self::Interrupted);
        }
        if deadline.is_some_and(|d| host.now() >= d) {
            return Err(Self::DeadlinePassed);
        }
        Ok(())
    }
}

impl From<HostError> for ScanError {
    fn from(error: HostError) -> Self {
        Self::Failed(error)
    }
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(e) => write!(f, "{e}"),
            Self::DeadlinePassed => f.write_str("the deadline passed during the process scan"),
            Self::Interrupted => f.write_str("interrupted during the process scan"),
        }
    }
}

/// Reads every visible process (see [`self::scan`] for the order of
/// listings, reads and re-reads), stopping when `deadline` passes or a
/// signal arrives ([`ScanError`]). Fails only when the process list itself
/// cannot be read or diskgc runs out of file descriptors (`EMFILE`,
/// `ENFILE`) in any read of the scan: an error of the run (TASK-166).
pub fn scan<H: Host>(
    host: &H,
    options: &Options<'_>,
    deadline: Option<MonoTime>,
) -> Result<Scan, ScanError> {
    ScanError::stop_if_due(host, deadline)?;
    // The scan is as old as its first read.
    let started = host.now();
    let sockets = options.sockets.then(|| match host.bound_unix_sockets() {
        // Out of descriptors for the netlink socket says nothing about any
        // socket: an error of the run, not `contains_ipc` for every
        // candidate that holds one (TASK-185's class).
        Err(e) if e.is_out_of_descriptors() => {
            Err(ScanError::Failed(scan::with_descriptor_advice(e)))
        }
        answer => Ok(answer),
    });
    let sockets = sockets.transpose()?;
    // Owner PID's namespace facts are read before the first listing and
    // again after the NSpid reads, so they cover the window of the
    // listings they qualify ([`scan::pid_table`]).
    let namespace_before = options.pid_lookup.then(|| host.pid_namespace());
    let gathered = scan::gather(host, options.users, options.pytest_facts, deadline)?;
    let pid_table = namespace_before
        .map(|before| {
            scan::pid_table(
                host,
                before,
                &gathered.listed,
                gathered.unsettled.is_none(),
                deadline,
            )
        })
        .transpose()?;
    let pytest_facts = options.pytest_facts.then(|| {
        (
            options.users.to_vec(),
            Processes::new(scan::pytest_facts(&gathered, options.users)),
        )
    });
    let mut s = Scan {
        started,
        finished: gathered.finished,
        listings: gathered.listings,
        unsettled: gathered.unsettled,
        retried_unsettled: None,
        reader_notes: host.take_proc_read_notes(),
        processes: gathered.read.len(),
        gone: gathered.gone,
        seen: BTreeMap::new(),
        uses: HashMap::new(),
        maps_by_inode: HashMap::new(),
        environ: Vec::new(),
        environ_index: environ::EnvPathIndex::default(),
        unreadable: gathered.unreadable,
        environ_failed: gathered.environ_failed,
        held_deleted: Vec::new(),
        sockets,
        pid_table,
        pytest_facts,
    };
    for (pid, read) in gathered.read {
        for (id, how) in read.uses {
            s.uses.entry(id).or_default().push(Use { pid, how });
            if how == How::Map {
                s.maps_by_inode.entry(id.ino).or_default().push(pid);
            }
        }
        for (var, path) in read.environ {
            s.environ.push(EnvPath { pid, var, path });
        }
        for (id, bytes, text) in read.deleted {
            s.held_deleted.push(HeldDeleted {
                pid,
                id,
                bytes,
                text,
            });
        }
        s.seen.insert(pid, read.seen);
    }
    s.environ_index = environ::EnvPathIndex::new(&s.environ);
    Ok(s)
}

/// A process's entry in the `held_by_deleted_files` report
/// (docs/reference.md, Output): the open-but-deleted files of one process
/// on one filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldByDeleted {
    pub pid: Pid,
    pub command: OsString,
    /// Allocated bytes of those files, each counted once.
    pub bytes: u64,
}

impl Scan {
    fn blocks_all(&self) -> bool {
        self.unsettled.is_some() || self.unreadable.iter().any(|u| u.excuse.is_none())
    }

    /// Owner PID's lookup table (`None`: not asked for, see
    /// [`Options::pid_lookup`]).
    pub fn pid_table(&self) -> Option<&PidTable> {
        self.pid_table.as_ref()
    }

    /// The pytest contract's process facts (`None`: not asked for, see
    /// [`Options::pytest_facts`]), with the users whose processes they
    /// cover: a process with none of them as a uid was not read.
    pub fn pytest_facts(&self) -> Option<(&[u32], &Processes)> {
        self.pytest_facts
            .as_ref()
            .map(|(users, facts)| (users.as_slice(), facts))
    }

    /// Whether the process list settled ([`MAX_LISTINGS`]).
    pub fn settled(&self) -> bool {
        self.unsettled.is_none()
    }

    /// When the scan started: its age is measured from here.
    pub fn started(&self) -> MonoTime {
        self.started
    }

    /// How long the scan took.
    pub fn took(&self) -> Duration {
        self.finished.saturating_since(self.started)
    }

    /// The process listings it made.
    pub fn listings(&self) -> usize {
        self.listings
    }

    /// The processes read.
    pub fn processes(&self) -> usize {
        self.processes
    }

    /// The processes found gone.
    pub fn gone(&self) -> usize {
        self.gone
    }

    /// Whether a deletion starting at `now` may still rely on this scan:
    /// it started less than [`FRESHNESS`] before.
    pub fn is_fresh_at(&self, now: MonoTime) -> bool {
        now.saturating_since(self.started) < FRESHNESS
    }

    /// The last moment at which a deletion may start on this scan's word.
    pub fn fresh_until(&self) -> MonoTime {
        self.started
            .checked_add(FRESHNESS)
            .expect("monotonic time plus one second fits")
    }

    /// The processes that could not be read, each with whether it blocks.
    pub fn unreadable(&self) -> &[Unreadable] {
        &self.unreadable
    }

    /// What a run should log about this scan, one line each: an unsettled
    /// process list, how many processes of other users were not inspected
    /// (the visibility limit), every other unreadable process and why it
    /// blocks or not, and every process whose environment could not be
    /// read.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(why) = &self.retried_unsettled {
            notes.push(format!(
                "process scan unsettled; retried after {UNSETTLED_RETRY_PAUSE:?}: {why}"
            ));
        }
        if let Some(why) = &self.unsettled {
            notes.push(format!("process scan unsettled: {why}"));
        }
        notes.extend(self.reader_notes.iter().cloned());
        let others = self
            .unreadable
            .iter()
            .filter(|u| u.excuse == Some(Excuse::OtherUser))
            .count();
        if others > 0 {
            notes.push(format!(
                "{others} processes of other users not inspected (their use of the user's files \
                 is invisible without root)"
            ));
        }
        for u in self
            .unreadable
            .iter()
            .filter(|u| u.excuse != Some(Excuse::OtherUser))
        {
            let name = u
                .name
                .as_ref()
                .map_or("?".into(), |n| n.to_string_lossy().into_owned());
            let ids = u.status.as_ref().map_or("unknown".into(), |s| {
                let map = match &s.uid_map {
                    crate::host::UidMap::Unreadable(why) => format!(", uid_map unreadable: {why}"),
                    crate::host::UidMap::Map(_) | crate::host::UidMap::Absent => String::new(),
                };
                format!("uid {} gid {}{map}", s.uid, s.gid)
            });
            match u.excuse {
                Some(excuse) => notes.push(format!(
                    "pid {} ({name}, {ids}) not inspected, {}; not counted as using \
                     anything: {}",
                    u.pid,
                    excuse.as_str(),
                    u.error
                )),
                None => notes.push(format!(
                    "pid {} ({name}, {ids}) cannot be inspected and is treated as using every \
                     candidate: {}",
                    u.pid, u.error
                )),
            }
        }
        for (pid, e) in &self.environ_failed {
            notes.push(format!(
                "pid {pid} ({}): initial environment unreadable (empty, as after env -i), \
                 no environment evidence from it: {e}",
                self.command(*pid)
            ));
        }
        notes
    }

    /// The command of `pid` for messages (`?` when unknown).
    pub fn command(&self, pid: Pid) -> String {
        self.command_option(pid).unwrap_or_else(|| "?".into())
    }

    /// A command observed at scan time, without inventing a name for an
    /// unreadable process.
    pub fn command_option(&self, pid: Pid) -> Option<String> {
        self.seen
            .get(&pid)
            .map(|s| &s.command)
            .filter(|c| !c.is_empty())
            .map(|c| c.to_string_lossy().into_owned())
    }

    /// The one unreadable process that blocks every candidate, if a scan
    /// settled. An unsettled scan has no complete attribution.
    pub fn blocking_unreadable(&self) -> Option<crate::engine::ProcessBlocker> {
        if self.unsettled.is_some() {
            return None;
        }
        let u = self.unreadable.iter().find(|u| u.excuse.is_none())?;
        Some(crate::engine::ProcessBlocker {
            pid: u.pid.0,
            command: u
                .name
                .as_ref()
                .filter(|n| !n.is_empty())
                .map(|n| n.to_string_lossy().into_owned()),
            evidence: crate::engine::ProcessEvidence::Unreadable,
        })
    }

    /// Every process read in full, in pid order, with what it showed of
    /// itself as text (for heuristics on paths and names, such as the
    /// Cargo kind's, docs/safety.md, `cargo-target`: In use).
    pub fn readable(&self) -> impl Iterator<Item = (Pid, &Seen)> {
        self.seen.iter().map(|(pid, seen)| (*pid, seen))
    }

    /// The uses of the object `id` (fds, cwd, root, exe, mappings), matched
    /// by (device, inode) exactly.
    pub fn uses(&self, id: DevIno) -> &[Use] {
        self.uses.get(&id).map_or(&[], Vec::as_slice)
    }

    /// Every absolute path named in an initial environment (values split
    /// at `:`), with its process and variable.
    pub fn environ_paths(&self) -> &[EnvPath] {
        &self.environ
    }

    /// Lower bound on the environment ancestor index's heap allocation.
    /// This omits HashMap control bytes and PathBuf spare capacity, so it is
    /// useful for scale diagnostics, not a full heap or RSS accounting.
    pub fn environ_index_minimum_bytes(&self) -> usize {
        self.environ_index.minimum_bytes()
    }

    /// The `in_use_unknown` skip of the candidate at `path` when this scan
    /// cannot judge anything ([`Self::blocked`]), naming the unreadable
    /// process that blocks, if one does; `None`: it can judge candidates.
    pub fn unknown(&self, path: &Path) -> Option<Skip> {
        let why = self.blocked()?;
        Some(
            Skip::new(Reason::InUseUnknown, path.to_owned(), why)
                .with_process_blockers(self.blocking_unreadable().into_iter().collect()),
        )
    }

    /// Why this scan cannot clear anything: it is unsettled, or a process
    /// that blocks could not be read (docs/safety.md, In use). `None`: it
    /// can judge candidates.
    pub fn blocked(&self) -> Option<String> {
        if let Some(why) = &self.unsettled {
            return Some(why.clone());
        }
        let u = self.unreadable.iter().find(|u| u.excuse.is_none())?;
        let name = u
            .name
            .as_ref()
            .map_or("?".into(), |n| n.to_string_lossy().into_owned());
        Some(format!(
            "pid {} ({name}) cannot be inspected, so it may be using any candidate: {}",
            u.pid, u.error
        ))
    }

    /// The open-but-deleted files of the visible processes on the
    /// filesystem `dev`, per process (docs/reference.md,
    /// `held_by_deleted_files`).
    pub fn held_by_deleted_files(&self, dev: Dev) -> Vec<HeldByDeleted> {
        self.held_by_deleted_files_on(&[dev])
    }

    /// As [`Self::held_by_deleted_files`], accepting the devices of all
    /// visible mounts sharing a space pool (btrfs subvolumes have distinct
    /// `statx` devices). The caller establishes that relationship.
    pub fn held_by_deleted_files_on(&self, devices: &[Dev]) -> Vec<HeldByDeleted> {
        let mut per_pid: BTreeMap<Pid, u64> = BTreeMap::new();
        let mut counted = HashSet::new();
        let devices: HashSet<Dev> = devices.iter().copied().collect();
        for held in self
            .held_deleted
            .iter()
            .filter(|h| devices.contains(&h.id.dev))
        {
            // Keep every holder visible, but attribute shared bytes to one
            // process only. Scan collection visits pids in ascending order.
            let bytes = per_pid.entry(held.pid).or_default();
            if counted.insert(held.id) {
                *bytes = bytes.saturating_add(held.bytes);
            }
        }
        per_pid
            .into_iter()
            .map(|(pid, bytes)| HeldByDeleted {
                pid,
                command: self
                    .seen
                    .get(&pid)
                    .map(|s| s.command.clone())
                    .unwrap_or_default(),
                bytes,
            })
            .collect()
    }

    /// Judges `candidate`, whose eligibility walk gave `record`, against
    /// this scan (no I/O; freshness is the caller's, see [`Checker`]). In
    /// order, the first that applies:
    ///
    /// 1. the scan is unsettled, or a process that blocks could not be
    ///    read: `in_use_unknown` (it could be using anything, and whether
    ///    it does cannot be told; TASK-190: not `in_use`, which would give
    ///    a harness evidence that does not exist);
    /// 2. a process uses one of the record's objects (fd, cwd, root, exe,
    ///    mapping; a FIFO held open is found here): `in_use`;
    /// 3. a process's initial environment names a path inside the
    ///    candidate: `in_use`;
    /// 4. a socket in the record is bound in diskgc's network namespace, or
    ///    whether it is cannot be told (no socket list in this scan, or the
    ///    query failed): `contains_ipc`.
    pub fn check<F>(&self, candidate: &Candidate<'_, F>, record: &WalkRecord) -> Result<(), Skip> {
        let path = candidate.path();
        use crate::engine::{ProcessBlocker, ProcessEvidence};
        let blocker = |pid: Pid, evidence| ProcessBlocker {
            pid: pid.0,
            command: self.command_option(pid),
            evidence,
        };
        let in_use = |at: PathBuf, detail: String, process: Option<ProcessBlocker>| {
            Err(Skip::new(Reason::InUse, at, detail)
                .with_process_blockers(process.into_iter().collect()))
        };
        if let Some(skip) = self.unknown(path) {
            return Err(skip);
        }
        let by_inode = MAPS_BY_INODE.contains(&candidate.fs_type());
        for id in record.objects() {
            let at = || below(path, &record.path_of(id).unwrap_or_default());
            if let Some(u) = self.uses.get(&id).and_then(|uses| uses.first()) {
                return in_use(
                    at(),
                    format!(
                        "pid {} ({}) has it {} ({id})",
                        u.pid,
                        self.command(u.pid),
                        u.how
                    ),
                    Some(blocker(
                        u.pid,
                        match u.how {
                            How::Fd(_) => ProcessEvidence::Fd,
                            How::Cwd => ProcessEvidence::Cwd,
                            How::Root => ProcessEvidence::Root,
                            How::Exe => ProcessEvidence::Exe,
                            How::Map => ProcessEvidence::Map,
                        },
                    )),
                );
            }
            if by_inode && let Some(pid) = self.maps_by_inode.get(&id.ino).and_then(|p| p.first()) {
                return in_use(
                    at(),
                    format!(
                        "pid {pid} ({}) maps an object with its inode number {} ({} maps may \
                         show another device)",
                        self.command(*pid),
                        id.ino,
                        candidate.fs_type()
                    ),
                    Some(blocker(*pid, ProcessEvidence::InodeMap)),
                );
            }
        }
        if let Some(e) = self
            .environ_index
            .first_inside(path)
            .map(|i| &self.environ[i])
        {
            return in_use(
                e.path.clone(),
                format!(
                    "pid {} ({}) names it in its initial environment ({}={})",
                    e.pid,
                    self.command(e.pid),
                    e.var.to_string_lossy(),
                    e.path.display()
                ),
                Some(blocker(e.pid, ProcessEvidence::Environment)),
            );
        }
        for ipc in record.ipc_entries() {
            if ipc.kind != IpcKind::Socket {
                // A FIFO is live when a visible process holds it open:
                // found by step 2 above.
                continue;
            }
            let at = below(path, &ipc.rel);
            let live = match &self.sockets {
                None => Err("the scan did not ask for bound sockets".to_owned()),
                Some(Err(e)) => Err(format!("the socket query failed: {e}")),
                Some(Ok(bound)) => Ok(bound.iter().any(|s| s.is(ipc.id))),
            };
            match live {
                Ok(false) => {}
                Ok(true) => {
                    return Err(Skip::new(
                        Reason::ContainsIpc,
                        at,
                        format!("a socket bound in diskgc's network namespace ({})", ipc.id),
                    ));
                }
                Err(why) => {
                    return Err(Skip::new(
                        Reason::ContainsIpc,
                        at,
                        format!("a socket whose liveness cannot be told, so live: {why}"),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// `rel` below the candidate's `path` (empty: the candidate itself, without
/// the trailing `/` that joining an empty path adds).
fn below(path: &Path, rel: &Path) -> PathBuf {
    if rel.as_os_str().is_empty() {
        path.to_owned()
    } else {
        path.join(rel)
    }
}

/// A candidate cleared by the in-use check: only [`Checker::with_fresh_clear`]
/// makes one, borrowing the exact candidate and walk record it checked.
/// [`delete_cleared`] can only consume this checked pair while the clear is
/// fresh (docs/safety.md, Freshness). Neither `Copy` nor `Clone`: one check,
/// one deletion. The borrow also prevents splitting the record meanwhile.
#[derive(Debug)]
pub struct Clear<'checked, 'parent, F> {
    fresh_until: MonoTime,
    candidate: &'checked Candidate<'parent, F>,
    record: &'checked WalkRecord,
}

/// Why [`delete_cleared`] deleted nothing.
#[derive(Debug)]
pub enum NotCleared<'checked, 'parent, F> {
    /// The clear is no longer fresh: check again.
    Expired(Clear<'checked, 'parent, F>),
}

impl<F> Clear<'_, '_, F> {
    /// The last moment at which the deletion it allows may start.
    pub fn fresh_until(&self) -> MonoTime {
        self.fresh_until
    }
}

/// Deletes the candidate and record borrowed by `clear`, as
/// [`engine::delete_unchecked`] does, but only while the check is fresh now
/// (the last port call before the delete reads the clock); an expired one
/// comes back ([`NotCleared::Expired`]) for the caller to check again. The only deletion entry
/// point for checked candidates (through [`clear_and_delete`]: the `fs`
/// kind's protocol and the Cargo kind's).
pub fn delete_cleared<'checked, 'parent, H: Delete>(
    host: &H,
    clear: Clear<'checked, 'parent, H::Fd>,
    options: DeleteOptions,
) -> Result<Deletion, NotCleared<'checked, 'parent, H::Fd>> {
    if host.now() >= clear.fresh_until {
        return Err(NotCleared::Expired(clear));
    }
    Ok(engine::delete_unchecked(
        host,
        clear.candidate,
        clear.record,
        options,
    ))
}

/// Keeps the latest [`Scan`] and decides when a new one is needed.
#[derive(Debug)]
pub struct Checker<'a> {
    options: Options<'a>,
    reuse_limit: Duration,
    latest: Option<Scan>,
}

impl<'a> Checker<'a> {
    /// A checker for candidates walked with `rules` (its scans use
    /// [`Options::from_rules`]), reusing a scan for up to `reuse_limit`
    /// after it started (at most [`FRESHNESS`]; `Duration::ZERO`: a scan
    /// per check).
    pub fn new(rules: &Rules<'a>, reuse_limit: Duration) -> Self {
        Self::with_options(Options::from_rules(rules), reuse_limit)
    }

    /// A checker with explicit options (for scans without a walk, and for
    /// tests of the guard in [`Checker::serves`]).
    pub(crate) fn with_options(options: Options<'a>, reuse_limit: Duration) -> Self {
        Self {
            options,
            reuse_limit: reuse_limit.min(FRESHNESS),
            latest: None,
        }
    }

    /// A read-only plan's checker. Its verdicts are advisory; a reclaim
    /// always uses a separate deleting checker with the freshness bound.
    pub fn for_plan(rules: &Rules<'a>) -> Self {
        Self {
            options: Options::from_rules(rules),
            reuse_limit: PLAN_REUSE_LIMIT,
            latest: None,
        }
    }

    /// This checker, its scans also reading what Owner PID needs when
    /// `pid_lookup` ([`Options::with_pid_lookup`]): set when a source it
    /// serves has a liveness contract.
    #[must_use]
    pub fn with_pid_lookup(mut self, pid_lookup: bool) -> Self {
        self.options = self.options.with_pid_lookup(pid_lookup);
        self.latest = None;
        self
    }

    /// This checker, its scans also reading the pytest contract's process
    /// facts ([`Options::with_pytest_facts`]): set when a source it serves
    /// has `liveness_contract: pytest`.
    #[must_use]
    pub fn with_pytest_facts(mut self, pytest_facts: bool) -> Self {
        self.options = self.options.with_pytest_facts(pytest_facts);
        self.latest = None;
        self
    }

    /// Runs `within` with this checker reusing a scan for up to `limit`
    /// after it started, then restores its own reuse limit. For a Cargo
    /// target's parts under its locks (docs/safety.md, `cargo-target`:
    /// Reclaim protocol, step 3) and an `fs` candidate's delete (In use:
    /// Freshness): there one scan serves every part or candidate whose
    /// deletion starts within [`FRESHNESS`]. The bound itself is unchanged
    /// ([`Checker::with_fresh_clear`], [`delete_cleared`]); `limit` above
    /// it is a bug.
    pub fn reusing_for<T>(&mut self, limit: Duration, within: impl FnOnce(&mut Self) -> T) -> T {
        assert!(
            limit <= FRESHNESS,
            "a reuse limit of {limit:?} is past the freshness bound {FRESHNESS:?}"
        );
        let own = std::mem::replace(&mut self.reuse_limit, limit);
        let out = within(self);
        self.reuse_limit = own;
        out
    }

    /// The scan the last check used.
    pub fn latest(&self) -> Option<&Scan> {
        self.latest.as_ref()
    }

    /// Drops the kept scan: the next check scans afresh.
    pub fn forget(&mut self) {
        self.latest = None;
    }

    /// The options its scans use.
    pub fn options(&self) -> Options<'a> {
        self.options
    }

    /// The one freshness loop of every deletion and command action
    /// (docs/safety.md, In use: Freshness; TASK-196 item 3). Checks
    /// `candidate` against a scan younger than the reuse limit (scanning
    /// anew if the kept one is older, or there is none; a scan stops at the
    /// deadline of `attempts` or a signal), and hands a clear that is still
    /// fresh after the check to `act`, with the scan, as the last thing
    /// before the caller's action starts. A skip stands whatever the scan's
    /// age (refusing is always safe).
    ///
    /// `act` returns `Some` when it acted (or decided), `None` when the
    /// clear expired before the action could start. A stale scan (a suspend
    /// during the scan or right after it, a scan slower than the bound) or
    /// an expired clear spends one of the candidate's `attempts`
    /// ([`FreshAttempts::retry`]: a pause, then a new scan); once they are
    /// spent the candidate is skipped (`in_use_unknown`), never cleared. A
    /// scan kept from an earlier check that reaches the bound meanwhile has
    /// reached the end of its reuse: checked again on a new scan, no attempt
    /// spent.
    pub fn with_fresh_clear<'checked, 'parent, H: Host, T, E>(
        &mut self,
        host: &H,
        candidate: &'checked Candidate<'parent, H::Fd>,
        record: &'checked WalkRecord,
        attempts: &mut FreshAttempts,
        mut act: impl FnMut(&Scan, Clear<'checked, 'parent, H::Fd>) -> Result<Option<T>, E>,
    ) -> Result<T, E>
    where
        E: From<Skip> + From<Stop>,
    {
        let path = candidate.path();
        if !record.belongs_to(candidate) {
            return Err(Stop::Failed {
                path: path.to_owned(),
                error: HostError::invalid(
                    "in-use check",
                    path.display(),
                    "the walk record was made for another candidate name or mount",
                ),
            }
            .into());
        }
        // Bounded: each pass ends in a verdict or an action, or spends one
        // attempt, except at most one pass whose kept scan reached the end
        // of its reuse (only the first pass can reuse: every later one
        // follows a `forget`).
        for _ in 0..=FRESH_ATTEMPTS {
            let reused = self.reusable_at(host.now());
            if !reused {
                self.rescan(host, attempts.deadline)
                    .map_err(|e| e.at(path))?;
            }
            let scan = self.latest.as_ref().expect("made above");
            scan.check(candidate, record)?;
            let what = if scan.is_fresh_at(host.now()) {
                let clear = Clear {
                    candidate,
                    record,
                    fresh_until: scan.fresh_until(),
                };
                match act(scan, clear)? {
                    Some(done) => return Ok(done),
                    None => "its clear expired before the action could start",
                }
            } else {
                "its scan was older than the freshness bound once the check finished"
            };
            if reused {
                self.forget();
                continue;
            }
            let stale = Stale::of(self.latest.as_ref().expect("checked above"), what);
            attempts.retry::<H, E>(host, self, path, stale)?;
        }
        unreachable!("each pass decides or spends one of {FRESH_ATTEMPTS} attempts, but one reuse")
    }

    /// [`Checker::with_fresh_clear`] for a caller that acts on the clear
    /// itself: the clear, or the candidate's skip, or an end of the run.
    pub fn check<'checked, 'parent, H: Host>(
        &mut self,
        host: &H,
        candidate: &'checked Candidate<'parent, H::Fd>,
        record: &'checked WalkRecord,
        attempts: &mut FreshAttempts,
    ) -> Result<Result<Clear<'checked, 'parent, H::Fd>, Skip>, Stop> {
        match self.with_fresh_clear(host, candidate, record, attempts, |_, clear| {
            Ok::<_, NotDeleted>(Some(clear))
        }) {
            Ok(clear) => Ok(Ok(clear)),
            Err(NotDeleted::Skipped(skip)) => Ok(Err(skip)),
            Err(NotDeleted::Stopped(stop)) => Err(stop),
        }
    }

    /// A scan younger than the reuse limit: the kept one, or a new one
    /// (stopping at `deadline` or a signal). For judging without deleting
    /// (a plan, a survey): refusing needs no freshness, and nothing it
    /// clears is deleted on its word (deleting goes through
    /// [`Checker::with_fresh_clear`] and its [`Clear`]).
    pub fn current<H: Host>(
        &mut self,
        host: &H,
        deadline: Option<MonoTime>,
    ) -> Result<&Scan, ScanError> {
        let now = host.now();
        let reusable = self.reusable_at(now);
        if !reusable {
            self.rescan(host, deadline)?;
        }
        Ok(self.latest.as_ref().expect("made above"))
    }

    fn rescan<H: Host>(&mut self, host: &H, deadline: Option<MonoTime>) -> Result<(), ScanError> {
        self.latest = None;
        let mut latest = scan(host, &self.options, deadline)?;
        if let Some(why) = latest.unsettled.take() {
            host.sleep(UNSETTLED_RETRY_PAUSE);
            latest = scan(host, &self.options, deadline)?;
            latest.retried_unsettled = Some(why);
        }
        self.latest = Some(latest);
        Ok(())
    }

    fn reusable_at(&self, now: MonoTime) -> bool {
        self.latest.as_ref().is_some_and(|scan| {
            // An unsettled listing did not inspect every process. Retry for
            // each candidate, including read-only plans with a long limit.
            if scan.unsettled.is_some() {
                return false;
            }
            let blocked = scan.blocks_all();
            let limit = if blocked {
                BLOCKED_REUSE_LIMIT.min(self.reuse_limit)
            } else {
                self.reuse_limit
            };
            let since = if blocked { scan.finished } else { scan.started };
            now.saturating_since(since) < limit
        })
    }
}

/// The candidate's `flock` (directories only; dropping it releases it).
#[derive(Debug)]
pub struct CandidateLock<F> {
    locked: Option<Locked<F>>,
}

impl<F> CandidateLock<F> {
    /// The lock held on the candidate directory; `None` when it is not a
    /// directory that could be opened (left to the walk, which refuses
    /// what it must, and to [`locked_is_walked`]).
    pub fn locked(&self) -> Option<&Locked<F>> {
        self.locked.as_ref()
    }
}

/// Opens the candidate below its parent and takes an exclusive `flock` on
/// it without waiting (docs/safety.md, Deletion, step 3; a directory
/// another process holds `flock`ed is `locked`). Only directories are
/// locked: a candidate that is not one (`ENOTDIR`, a symlink, a directory
/// that cannot be opened for reading, anything else the open refuses for
/// this path) gets no lock and is left to the walk, which refuses
/// what it must; [`locked_is_walked`] then refuses a candidate that turned
/// out to be a directory without its lock.
pub fn lock_candidate<H: Host>(
    host: &H,
    candidate: &Candidate<'_, H::Fd>,
) -> Result<Result<CandidateLock<H::Fd>, Skip>, HostError> {
    let opened = open_beneath(
        host,
        candidate.parent(),
        candidate.mount_id(),
        Path::new(candidate.name()),
        Access::Read,
        Want::Directory,
    );
    let fd = match opened {
        Ok((fd, _)) => fd,
        // Permission (a directory of the user's own with mode 000): the
        // walk refuses it as `unreadable`.
        Err(ResolveError::Host(e))
            if matches!(e.kernel_errno(), Some(Errno::EACCES | Errno::EPERM)) =>
        {
            return Ok(Ok(CandidateLock { locked: None }));
        }
        Err(ResolveError::Host(e)) => return Err(e),
        Err(_) => return Ok(Ok(CandidateLock { locked: None })),
    };
    match host.flock_try(fd, LockMode::Exclusive)? {
        TryLock::Acquired(locked) => Ok(Ok(CandidateLock {
            locked: Some(locked),
        })),
        TryLock::Busy(_) => Ok(Err(Skip::new(
            Reason::Locked,
            candidate.path().to_owned(),
            "the directory is flock'ed by another process",
        ))),
    }
}

/// Why [`clear_and_delete`] deleted nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotDeleted {
    /// In use, unknown (or refused by the caller's own check on the same
    /// scan).
    Skipped(Skip),
    /// An error of the run, or its end: the process list could not be read,
    /// diskgc ran out of file descriptors, the deadline passed, a signal
    /// arrived, or a caller's last safety check hit one of these.
    Stopped(Stop),
}

impl From<Skip> for NotDeleted {
    fn from(skip: Skip) -> Self {
        Self::Skipped(skip)
    }
}

impl From<Stop> for NotDeleted {
    fn from(stop: Stop) -> Self {
        Self::Stopped(stop)
    }
}

impl Checker<'_> {
    /// Whether this checker's scans fit walks made with `rules`
    /// ([`Options::from_rules`]): a narrower user list would excuse an
    /// owner's processes as another user's. A mismatch is a bug of the
    /// caller, an error of the run.
    pub fn serves(&self, rules: &Rules<'_>) -> Result<(), HostError> {
        let wanted = Options::from_rules(rules);
        if self.options.users == wanted.users && self.options.sockets == wanted.sockets {
            return Ok(());
        }
        Err(HostError::invalid(
            "in-use check",
            format!("{:?}", self.options.users),
            format!(
                "the checker's users {:?} and socket query {} disagree with the walk's owners \
                 {:?} and IPC policy (block any: {}); build it with Checker::new (Options::from_rules) \
                 from the same rules",
                self.options.users, self.options.sockets, rules.owners, rules.block_ipc
            ),
        ))
    }
}

/// The last step of every deletion of a checked candidate (docs/safety.md,
/// Deletion; the Cargo protocol's parts): the in-use check of exactly
/// `record` against a scan within the freshness rules
/// ([`Checker::with_fresh_clear`]; an unsettled scan is retried once, and a
/// persistently blocking scan is kept for at most the checker's short reuse
/// window), the caller's own check on the same scan (`also`: the Cargo
/// process heuristics, the `fs` kind's final path and policy checks), then
/// [`delete_cleared`] right away. A stale scan or a clear that expired
/// before the delete could start spends one of the candidate's `attempts`;
/// once they are spent it is skipped (`in_use_unknown`). The candidate is
/// what the caller reclaims as one: a Cargo target passes the same budget
/// for each of its parts.
pub fn clear_and_delete<H: Delete>(
    host: &H,
    checker: &mut Checker<'_>,
    candidate: &Candidate<'_, H::Fd>,
    record: &WalkRecord,
    options: DeleteOptions,
    attempts: &mut FreshAttempts,
    also: &mut dyn FnMut(&Scan) -> Result<(), NotDeleted>,
) -> Result<Deletion, NotDeleted> {
    checker.with_fresh_clear(host, candidate, record, attempts, |scan, clear| {
        also(scan)?;
        match delete_cleared(host, clear, options) {
            Ok(done) => Ok(Some(done)),
            Err(NotCleared::Expired(_)) => Ok(None),
        }
    })
}

/// Why one attempt at a fresh verdict was spent, for the skip's detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stale {
    /// What went stale.
    pub what: &'static str,
    /// How long the attempt's scan took.
    pub took: Duration,
    /// The processes it read.
    pub processes: usize,
}

impl Stale {
    /// An attempt spent on `scan` because of `what`.
    pub fn of(scan: &Scan, what: &'static str) -> Self {
        Self {
            what,
            took: scan.took(),
            processes: scan.processes,
        }
    }
}

/// One candidate's budget of [`FRESH_ATTEMPTS`] at a fresh in-use verdict
/// (docs/safety.md, In use: Freshness), shared by every check and expired
/// clear of that candidate, so the retries of the check and of its caller
/// cannot multiply.
#[derive(Debug)]
pub struct FreshAttempts {
    spent: usize,
    deadline: Option<MonoTime>,
    last: Option<Stale>,
    /// Gave up early: the next pause would have reached the deadline.
    cut_by_deadline: bool,
}

impl FreshAttempts {
    /// A fresh budget for a run ending at `deadline`: no retry pause runs
    /// to or past it, and no scan of the candidate's checks goes past it.
    pub fn new(deadline: Option<MonoTime>) -> Self {
        Self {
            spent: 0,
            deadline,
            last: None,
            cut_by_deadline: false,
        }
    }

    /// Spends one attempt on `stale`. With attempts left and the pause
    /// ([`FRESH_RETRY_PAUSES`]) ending before the deadline: drops the
    /// checker's scan so the next check scans afresh, pauses (a signal ends
    /// the pause and the run: [`Stop::Interrupted`]), and returns `Ok`.
    /// Otherwise the candidate's `in_use_unknown` skip, the stale scan kept
    /// as the checker's latest.
    fn retry<H: Host, E: From<Skip> + From<Stop>>(
        &mut self,
        host: &H,
        checker: &mut Checker<'_>,
        path: &Path,
        stale: Stale,
    ) -> Result<(), E> {
        assert!(self.spent < FRESH_ATTEMPTS, "a spent budget is not retried");
        self.spent += 1;
        self.last = Some(stale);
        let Some(pause) = FRESH_RETRY_PAUSES.get(self.spent - 1).copied() else {
            return Err(self.spent(path).into());
        };
        let in_time = match (self.deadline, host.now().checked_add(pause)) {
            (None, _) => true,
            (Some(deadline), Some(resume)) => resume < deadline,
            (Some(_), None) => false,
        };
        if !in_time {
            self.cut_by_deadline = true;
            return Err(self.spent(path).into());
        }
        // Only now: a skip keeps the last scan as `latest` (its notes).
        checker.forget();
        sleep_unless_interrupted(host, pause).map_err(|()| {
            Stop::Interrupted {
                path: path.to_owned(),
            }
            .into()
        })
    }

    /// The `in_use_unknown` skip of `path`, saying why in plain words.
    pub fn spent(&self, path: &Path) -> Skip {
        let last = self.last.map_or_else(String::new, |stale| {
            format!(
                "; last: {} (the scan of {} processes took {} ms)",
                stale.what,
                stale.processes,
                stale.took.as_millis()
            )
        });
        let cut = if self.cut_by_deadline {
            ", no time left before the deadline for another"
        } else {
            ""
        };
        Skip::new(
            Reason::InUseUnknown,
            path.to_owned(),
            format!(
                "the host was too busy to check whether it is in use: {} attempt(s) found no \
                 in-use scan within the freshness bound of {} ms{cut}{last}; kept, run again \
                 when the host is less busy",
                self.spent,
                FRESHNESS.as_millis()
            ),
        )
    }
}

/// Sleeps `pause` in slices of at most [`PAUSE_SLICE`], ending early
/// (`Err`) once a signal arrived (before or during it).
fn sleep_unless_interrupted<H: Host>(host: &H, pause: Duration) -> Result<(), ()> {
    let mut left = pause;
    // Bounded: each pass sleeps a nonzero slice of what is left.
    while !left.is_zero() {
        if host.interrupted().is_some() {
            return Err(());
        }
        let slice = left.min(PAUSE_SLICE);
        host.sleep(slice);
        left -= slice;
    }
    if host.interrupted().is_some() {
        return Err(());
    }
    Ok(())
}

/// A directory candidate is deleted only under its lock, and only if the
/// locked directory is the one the walk recorded (it could have been
/// replaced between the open and the walk); a non-directory is never
/// locked. Used by the `fs` kind's reclaim.
pub fn locked_is_walked<H: Host>(
    host: &H,
    candidate: &Candidate<'_, H::Fd>,
    lock: &CandidateLock<H::Fd>,
    record: &WalkRecord,
) -> Result<Result<(), Skip>, HostError> {
    let root = record.root().identity;
    let changed = |detail: &str| {
        Ok(Err(Skip::new(
            Reason::Changed,
            candidate.path().to_owned(),
            detail,
        )))
    };
    match (&lock.locked, root.file_type == FileType::Directory) {
        (None, false) => Ok(Ok(())),
        (None, true) => changed("a directory now, but it could not be opened and locked before"),
        (Some(_), false) => changed("not a directory any more, but a directory was locked"),
        (Some(locked), true) => {
            let stat = host.statx(Base::Dir(locked.fd()), Path::new(""))?;
            if stat.dev_ino() == Some(root.id) {
                Ok(Ok(()))
            } else {
                changed("the locked directory is not the walked one (replaced in between)")
            }
        }
    }
}
