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

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::engine::{
    self, Candidate, DeleteOptions, Deletion, Identity, IpcKind, Reason, Rules, Skip, WalkRecord,
};
use crate::host::{
    Access, Base, BoundSocket, Delete, Dev, DevIno, Errno, FileType, Host, HostError, LockMode,
    Locked, MonoTime, Pid, TryLock,
};
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
/// between candidates before the deletion freshness limit is reached.
pub const REUSE_LIMIT: Duration = Duration::from_millis(500);

/// A scan that blocks every candidate is kept for less time after it finishes,
/// so a transient process problem cannot mark a whole plan or reclaim `in_use`.
/// A slow blocking scan is still shared by later candidates for that interval.
pub const BLOCKED_REUSE_LIMIT: Duration = Duration::from_millis(500);

/// A read-only plan may reuse a clear scan longer: it does not delete on
/// the scan's word. A blocking scan is kept only for [`BLOCKED_REUSE_LIMIT`].
pub const PLAN_REUSE_LIMIT: Duration = Duration::from_secs(60);

/// One short pause before retrying a process list that kept growing. The
/// retry is a complete scan; neither scan can clear a candidate unless it
/// settles. A persistent fork storm stays a conservative `in_use` skip.
pub const UNSETTLED_RETRY_PAUSE: Duration = Duration::from_millis(20);

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
    retried_unsettled: Option<String>,
    processes: usize,
    gone: usize,
    /// Every process read in full, by pid (ordered, so reports are).
    seen: BTreeMap<Pid, Seen>,
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
        retried_unsettled: None,
        processes: gathered.read.len(),
        gone: gathered.gone,
        seen: BTreeMap::new(),
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
        s.seen.insert(pid, read.seen);
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
    fn blocks_all(&self) -> bool {
        self.unsettled.is_some() || self.unreadable.iter().any(|u| u.excuse.is_none())
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
        self.seen
            .get(&pid)
            .map(|s| &s.command)
            .filter(|c| !c.is_empty())
            .map_or("?".into(), |c| c.to_string_lossy().into_owned())
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
        let mut per_pid: BTreeMap<Pid, u64> = BTreeMap::new();
        for held in self.held_deleted.iter().filter(|h| h.id.dev == dev) {
            *per_pid.entry(held.pid).or_default() += held.bytes;
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
        if let Some(why) = self.blocked() {
            return in_use(path.to_owned(), why);
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
/// point for checked candidates (through [`clear_and_delete`]: the `fs`
/// kind's protocol and the Cargo kind's).
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
    /// tests of the guard in [`Checker::serves`]).
    pub fn with_options(options: Options<'a>, reuse_limit: Duration) -> Self {
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
        let reusable = self.reusable_at(now);
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

    /// A scan younger than the reuse limit: the kept one, or a new one.
    /// For judging without deleting (a plan, a survey): refusing needs no
    /// freshness, and nothing it clears is deleted on its word (deleting
    /// goes through [`Checker::check`] and its [`Clear`]).
    pub fn current<H: Host>(&mut self, host: &H) -> Result<&Scan, HostError> {
        let now = host.now();
        let reusable = self.reusable_at(now);
        if !reusable {
            self.rescan(host)?;
        }
        Ok(self.latest.as_ref().expect("made above"))
    }

    fn rescan<H: Host>(&mut self, host: &H) -> Result<(), HostError> {
        self.latest = None;
        let mut latest = scan(host, &self.options)?;
        if let Some(why) = latest.unsettled.take() {
            host.sleep(UNSETTLED_RETRY_PAUSE);
            latest = scan(host, &self.options)?;
            latest.retried_unsettled = Some(why);
        }
        self.latest = Some(latest);
        Ok(())
    }

    fn reusable_at(&self, now: MonoTime) -> bool {
        self.latest.as_ref().is_some_and(|scan| {
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
    /// In use (or refused by the caller's own check on the same scan).
    Skipped(Skip),
    /// An error of the run: the process list could not be read, or no scan
    /// stayed fresh until the deletion could start.
    Failed(HostError),
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
/// `record` against a scan within the freshness rules ([`Checker::check`];
/// an unsettled scan is retried once, and a persistently blocking scan is
/// kept for at most the checker's short reuse window), the caller's own
/// check on the same scan (`also`: the Cargo process heuristics; nothing
/// for kind `fs`), then [`delete_cleared`]
/// right away. A clear that expired before the delete could start is
/// checked once more against a new scan; twice is an error of the run.
pub fn clear_and_delete<H: Delete>(
    host: &H,
    checker: &mut Checker<'_>,
    candidate: &Candidate<'_, H::Fd>,
    record: &WalkRecord,
    options: DeleteOptions,
    also: &mut dyn FnMut(&Scan) -> Result<(), Skip>,
) -> Result<Deletion, NotDeleted> {
    let failed = |detail: String| {
        NotDeleted::Failed(HostError::invalid(
            "in-use check",
            candidate.path().display(),
            detail,
        ))
    };
    for attempt in 0..2 {
        let clear = checker
            .check(host, candidate, record)
            .map_err(NotDeleted::Failed)?
            .map_err(NotDeleted::Skipped)?;
        also(checker.latest().expect("the check made or kept one")).map_err(NotDeleted::Skipped)?;
        match delete_cleared(host, clear, candidate, record, options) {
            Ok(done) => return Ok(done),
            Err(NotCleared::Expired(_)) if attempt == 0 => checker.forget(),
            Err(e) => return Err(failed(format!("the clear could not be used: {e:?}"))),
        }
    }
    Err(failed(
        "a fresh check expired before the deletion could start, twice".to_owned(),
    ))
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
