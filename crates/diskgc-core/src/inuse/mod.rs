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
//!   older than its reuse limit; [`delete_unused`] runs the whole step for
//!   one candidate in the order of docs/safety.md, Deletion: lock the
//!   directory, walk, check, delete, while the check is fresh.
//!
//! Matching is by (device, inode) only, never by path text, which is wrong
//! in other mount namespaces. Memory maps on overlayfs and btrfs report the
//! device (and on older kernels the inode) of the layer or volume rather
//! than what `statx` reports, so on those filesystems a mapping matches by
//! inode number alone: more matches, the conservative side.

mod environ;
pub mod scan;

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::engine::{
    self, Candidate, DeleteOptions, Deletion, Identity, IpcKind, Reason, Rules, Skip, WalkRecord,
    WalkStop,
};
use crate::host::{
    Access, Base, BoundSocket, Delete, Dev, DevIno, Errno, FileType, Host, HostError, LockMode,
    Locked, MonoTime, Pid, TryLock,
};
use crate::resolve::{ResolveError, Want, open_beneath};

pub use scan::{EnvPath, Excuse, HeldDeleted, How, MAX_LISTINGS, REREAD_PAUSES, Unreadable, Use};

/// How old a scan may be when the deletion it cleared starts
/// (docs/safety.md, In use: Freshness).
pub const FRESHNESS: Duration = Duration::from_secs(1);

/// How long a [`Checker`] reuses a scan for further candidates by default:
/// half the freshness bound, so the deletion after a check has the other
/// half to start.
pub const REUSE_LIMIT: Duration = Duration::from_millis(500);

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
}

impl<'a> Options<'a> {
    /// The options matching the walk's `rules`: the users are its owners,
    /// and bound sockets are asked for exactly when the walk records
    /// sockets (`block_ipc` off).
    pub fn from_rules(rules: &Rules<'a>) -> Self {
        Self {
            users: rules.owners,
            sockets: !rules.block_ipc,
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
    processes: usize,
    gone: usize,
    commands: HashMap<Pid, OsString>,
    uses: HashMap<DevIno, Vec<Use>>,
    /// Memory-map uses by inode number alone (see [`MAPS_BY_INODE`]).
    maps_by_inode: HashMap<u64, Vec<Pid>>,
    environ: Vec<EnvPath>,
    unreadable: Vec<Unreadable>,
    environ_failed: Vec<(Pid, HostError)>,
    held_deleted: Vec<HeldDeleted>,
    sockets: Option<Result<Vec<BoundSocket>, HostError>>,
}

/// Reads every visible process (see [`self::scan`] for the order of
/// listings, reads and re-reads). Fails only when the process list itself
/// cannot be read: an error of the run.
pub fn scan<H: Host>(host: &H, options: &Options<'_>) -> Result<Scan, HostError> {
    // The scan is as old as its first read.
    let started = host.now();
    let sockets = options.sockets.then(|| host.bound_unix_sockets());
    let gathered = scan::gather(host, options.users)?;
    let mut s = Scan {
        started,
        finished: gathered.finished,
        listings: gathered.listings,
        unsettled: gathered.unsettled,
        processes: gathered.read.len(),
        gone: gathered.gone,
        commands: HashMap::new(),
        uses: HashMap::new(),
        maps_by_inode: HashMap::new(),
        environ: Vec::new(),
        unreadable: gathered.unreadable,
        environ_failed: gathered.environ_failed,
        held_deleted: Vec::new(),
        sockets,
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
        s.commands.insert(pid, read.command);
    }
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
        if let Some(why) = &self.unsettled {
            notes.push(format!("process scan unsettled: {why}"));
        }
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

    fn command(&self, pid: Pid) -> String {
        self.commands
            .get(&pid)
            .filter(|c| !c.is_empty())
            .map_or("?".into(), |c| c.to_string_lossy().into_owned())
    }

    /// The open-but-deleted files of the visible processes on the
    /// filesystem `dev`, per process (docs/reference.md,
    /// `held_by_deleted_files`).
    pub fn held_by_deleted_files(&self, dev: Dev) -> Vec<HeldByDeleted> {
        let mut per_pid: BTreeMap<Pid, u64> = BTreeMap::new();
        for held in self.held_deleted.iter().filter(|h| h.id.dev == dev) {
            *per_pid.entry(held.pid).or_default() += held.bytes;
        }
        per_pid
            .into_iter()
            .map(|(pid, bytes)| HeldByDeleted {
                pid,
                command: self.commands.get(&pid).cloned().unwrap_or_default(),
                bytes,
            })
            .collect()
    }

    /// Judges `candidate`, whose eligibility walk gave `record`, against
    /// this scan (no I/O; freshness is the caller's, see [`Checker`]). In
    /// order, the first that applies:
    ///
    /// 1. the scan is unsettled, or a process that blocks could not be
    ///    read: `in_use` (it could be using anything);
    /// 2. a process uses one of the record's objects (fd, cwd, root, exe,
    ///    mapping; a FIFO held open is found here): `in_use`;
    /// 3. a process's initial environment names a path inside the
    ///    candidate: `in_use`;
    /// 4. a socket in the record is bound in diskgc's network namespace, or
    ///    whether it is cannot be told (no socket list in this scan, or the
    ///    query failed): `contains_ipc`.
    pub fn check<F>(&self, candidate: &Candidate<'_, F>, record: &WalkRecord) -> Result<(), Skip> {
        let path = candidate.path();
        let in_use = |at: PathBuf, detail: String| Err(Skip::new(Reason::InUse, at, detail));
        if let Some(why) = &self.unsettled {
            return in_use(path.to_owned(), why.clone());
        }
        if let Some(u) = self.unreadable.iter().find(|u| u.excuse.is_none()) {
            let name = u
                .name
                .as_ref()
                .map_or("?".into(), |n| n.to_string_lossy().into_owned());
            return in_use(
                path.to_owned(),
                format!(
                    "pid {} ({name}) cannot be inspected, so it may be using any candidate: {}",
                    u.pid, u.error
                ),
            );
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
                );
            }
        }
        if let Some(e) = self.environ.iter().find(|e| environ::inside(&e.path, path)) {
            return in_use(
                e.path.clone(),
                format!(
                    "pid {} ({}) names it in its initial environment ({}={})",
                    e.pid,
                    self.command(e.pid),
                    e.var.to_string_lossy(),
                    e.path.display()
                ),
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

/// A candidate cleared by the in-use check: only [`Checker::check`] makes
/// one, for one walk record (it keeps the identity of the record's root:
/// device, inode, owner, type, birth time where reported), and
/// [`delete_cleared`] deletes only that record's candidate and only while
/// the clear is fresh (docs/safety.md, Freshness). Neither `Copy` nor
/// `Clone`: one check, one deletion.
#[derive(Debug, PartialEq, Eq)]
pub struct Clear {
    fresh_until: MonoTime,
    root: Identity,
}

/// Why [`delete_cleared`] deleted nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum NotCleared {
    /// The clear is no longer fresh: check again.
    Expired(Clear),
    /// The clear was made for another object (another candidate's root
    /// identity): a bug of the caller. Only the root's identity is
    /// compared, so walking the same object again between the check and
    /// the delete, or a hard-linked file candidate, is not caught here;
    /// delete with the record that was checked.
    OtherCandidate,
}

impl Clear {
    /// The last moment at which the deletion it allows may start.
    pub fn fresh_until(&self) -> MonoTime {
        self.fresh_until
    }
}

/// Deletes what the walk of `candidate` recorded, as [`engine::delete`]
/// does, but only on a [`Clear`] made for that record and still fresh now
/// (the last port call before the delete reads the clock); an expired one
/// comes back ([`NotCleared::Expired`]) for the caller to check again. The only deletion entry
/// point for checked candidates (`delete_unused`, the Cargo protocol).
pub fn delete_cleared<H: Delete>(
    host: &H,
    clear: Clear,
    candidate: &Candidate<'_, H::Fd>,
    record: &WalkRecord,
    options: DeleteOptions,
) -> Result<Deletion, NotCleared> {
    if clear.root != record.root().identity {
        return Err(NotCleared::OtherCandidate);
    }
    if host.now() >= clear.fresh_until {
        return Err(NotCleared::Expired(clear));
    }
    Ok(engine::delete(host, candidate, record, options))
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
    /// tests of the guard in [`delete_unused`]).
    pub fn with_options(options: Options<'a>, reuse_limit: Duration) -> Self {
        Self {
            options,
            reuse_limit: reuse_limit.min(FRESHNESS),
            latest: None,
        }
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

    /// Checks `candidate` against a scan younger than the reuse limit,
    /// scanning anew if the kept one is older (or there is none). A skip
    /// stands whatever the scan's age (refusing is always safe). A clear
    /// stands only if the scan is still fresh after the check, as the last
    /// thing before the caller's deletion starts: otherwise (a suspend
    /// during the scan or right after it, a scan slower than the bound)
    /// the candidate is checked once more against a new scan, and if that
    /// one is not fresh either, nothing can be cleared: an error of the
    /// run, not a skip. The caller starts the deletion right away and
    /// never after [`Clear::fresh_until`].
    pub fn check<H: Host, F>(
        &mut self,
        host: &H,
        candidate: &Candidate<'_, F>,
        record: &WalkRecord,
    ) -> Result<Result<Clear, Skip>, HostError> {
        let now = host.now();
        let reusable = self
            .latest
            .as_ref()
            .is_some_and(|s| now.saturating_since(s.started) < self.reuse_limit);
        if !reusable {
            self.rescan(host)?;
        }
        for attempt in 0..2 {
            let scan = self.latest.as_ref().expect("made above");
            if let Err(skip) = scan.check(candidate, record) {
                return Ok(Err(skip));
            }
            if scan.is_fresh_at(host.now()) {
                return Ok(Ok(Clear {
                    root: record.root().identity,
                    fresh_until: scan.fresh_until(),
                }));
            }
            if attempt == 0 {
                self.rescan(host)?;
            }
        }
        let scan = self.latest.as_ref().expect("made above");
        Err(HostError::invalid(
            "in-use scan",
            format!("{} processes", scan.processes),
            format!(
                "two scans in a row were older than the freshness bound of {FRESHNESS:?} once \
                 finished (the last took {:?}), so no candidate can be cleared",
                scan.took()
            ),
        ))
    }

    fn rescan<H: Host>(&mut self, host: &H) -> Result<(), HostError> {
        self.latest = None;
        self.latest = Some(scan(host, &self.options)?);
        Ok(())
    }
}

/// Why [`delete_unused`] deleted nothing (or stopped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The candidate is not eligible or is in use: a skip with its reason.
    Skipped(Skip),
    /// The walk ended without a record (deadline, error of the run).
    Walk(WalkStop),
    /// A host error of the run (opening or locking the candidate, the
    /// process list, a scan slower than the freshness bound).
    Failed(HostError),
}

impl From<Skip> for Stop {
    fn from(skip: Skip) -> Self {
        Self::Skipped(skip)
    }
}

/// The candidate's `flock` (directories only; dropping it releases it).
#[derive(Debug)]
pub struct CandidateLock<F> {
    locked: Option<Locked<F>>,
}

/// Opens the candidate below its parent and takes an exclusive `flock` on
/// it without waiting (docs/safety.md, Deletion, step 1; a directory
/// another process holds `flock`ed is `locked`). Only directories are
/// locked: a candidate that is not one (`ENOTDIR`, a symlink, a directory
/// that cannot be opened for reading, anything else the open refuses for
/// this path) gets no lock and is left to the walk, which refuses
/// what it must; [`delete_unused`] then deletes nothing that turned out to
/// be a directory without its lock.
pub fn lock_candidate<H: Host>(
    host: &H,
    candidate: &Candidate<'_, H::Fd>,
) -> Result<CandidateLock<H::Fd>, Stop> {
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
            return Ok(CandidateLock { locked: None });
        }
        Err(ResolveError::Host(e)) => return Err(Stop::Failed(e)),
        Err(_) => return Ok(CandidateLock { locked: None }),
    };
    match host
        .flock_try(fd, LockMode::Exclusive)
        .map_err(Stop::Failed)?
    {
        TryLock::Acquired(locked) => Ok(CandidateLock {
            locked: Some(locked),
        }),
        TryLock::Busy(_) => Err(Skip::new(
            Reason::Locked,
            candidate.path().to_owned(),
            "the directory is flock'ed by another process",
        )
        .into()),
    }
}

/// The deletion step for one filesystem candidate, in the order of
/// docs/safety.md, Deletion: lock the candidate directory, run the
/// eligibility walk under the lock, check that the locked directory is the
/// walked one, run the in-use check (`checker`, whose options must be
/// [`Options::from_rules`] of `rules`), and delete what the walk recorded
/// right after the check, which made sure it is fresh. The lock is held
/// until the delete returns.
pub fn delete_unused<H: Delete>(
    host: &H,
    checker: &mut Checker<'_>,
    candidate: &Candidate<'_, H::Fd>,
    rules: &Rules<'_>,
    options: DeleteOptions,
) -> Result<Deletion, Stop> {
    let wanted = Options::from_rules(rules);
    let have = checker.options();
    if have.users != wanted.users || have.sockets != wanted.sockets {
        return Err(Stop::Failed(HostError::invalid(
            "in-use check",
            candidate.path().display(),
            format!(
                "the checker's users {:?} and socket query {} disagree with the walk's owners \
                 {:?} and IPC policy (block any: {}); build the checker's options with \
                 Options::from_rules",
                have.users, have.sockets, rules.owners, rules.block_ipc
            ),
        )));
    }
    let lock = lock_candidate(host, candidate)?;
    let record = engine::walk(host, candidate, rules, options.deadline).map_err(Stop::Walk)?;
    locked_is_walked(host, candidate, &lock, &record)?;
    let mut clear = checker
        .check(host, candidate, &record)
        .map_err(Stop::Failed)??;
    // The check just made sure it is fresh; one more check if the clock
    // moved past it anyway (a suspend in between).
    let not_cleared = |e: NotCleared, why: &str| {
        Stop::Failed(HostError::invalid(
            "in-use check",
            candidate.path().display(),
            format!("{why}: {e:?}"),
        ))
    };
    let done = match delete_cleared(host, clear, candidate, &record, options) {
        Ok(done) => done,
        Err(e @ NotCleared::OtherCandidate) => {
            return Err(not_cleared(e, "the clear is for another walk record"));
        }
        Err(NotCleared::Expired(_)) => {
            checker.forget();
            clear = checker
                .check(host, candidate, &record)
                .map_err(Stop::Failed)??;
            delete_cleared(host, clear, candidate, &record, options).map_err(|e| {
                not_cleared(
                    e,
                    "a fresh check expired before the deletion could start, twice",
                )
            })?
        }
    };
    drop(lock);
    Ok(done)
}

/// A directory candidate is deleted only under its lock, and only if the
/// locked directory is the one the walk recorded (it could have been
/// replaced between the open and the walk); a non-directory is never
/// locked.
fn locked_is_walked<H: Host>(
    host: &H,
    candidate: &Candidate<'_, H::Fd>,
    lock: &CandidateLock<H::Fd>,
    record: &WalkRecord,
) -> Result<(), Stop> {
    let root = record.root().identity;
    let changed =
        |detail: &str| Err(Skip::new(Reason::Changed, candidate.path().to_owned(), detail).into());
    match (&lock.locked, root.file_type == FileType::Directory) {
        (None, false) => Ok(()),
        (None, true) => changed("a directory now, but it could not be opened and locked before"),
        (Some(_), false) => changed("not a directory any more, but a directory was locked"),
        (Some(locked), true) => {
            let stat = host
                .statx(Base::Dir(locked.fd()), Path::new(""))
                .map_err(Stop::Failed)?;
            if stat.dev_ino() == Some(root.id) {
                Ok(())
            } else {
                changed("the locked directory is not the walked one (replaced in between)")
            }
        }
    }
}
