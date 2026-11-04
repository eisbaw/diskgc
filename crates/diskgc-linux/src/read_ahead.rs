//! The plan's read-ahead ([`diskgc_core::host::Host::read_ahead`];
//! docs/architecture.md, The `Host` port: Read-ahead; TASK-325).
//!
//! A read-only plan judges its candidates one at a time, and where the
//! kernel has not cached their metadata, each judgement's walk waits on the
//! disk for one directory or inode at a time. The plan names each candidate
//! [`diskgc_core::fs_kind::READ_AHEAD`] places before it judges it; here
//! [`READERS`] threads read that candidate's metadata meanwhile (list its
//! directories, `statx` their entries), so that several reads are in
//! flight and the judgement's own walk finds them cached.
//!
//! What a reader reads is never used: nothing is returned, nothing is
//! decided, and a reader's failure is its own (the judgement reaches and
//! walks the candidate itself and reports what it meets). It still keeps
//! to the walk's rules on what may be touched (docs/safety.md, Path
//! traversal), and reads less than the walk:
//!
//! - the candidate is reached on the calling thread by
//!   [`diskgc_core::resolve::resolve_in`] (mount chain screened, no
//!   symlink), as the walk reaches it, and only a directory diskgc's user
//!   owns is read ahead;
//! - a name that is a mount point in the mount table read for that is
//!   never `statx`ed, so a dead mount cannot hang a reader (as the walk:
//!   docs/safety.md, Before the walk); `systemd-private-*` names are passed
//!   over before any `statx`, as the walk refuses them;
//! - every other name is `statx`ed (`AT_SYMLINK_NOFOLLOW`,
//!   `AT_NO_AUTOMOUNT`), and a directory is opened only when that showed a
//!   directory diskgc's user owns on the candidate's own device and mount,
//!   with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS |
//!   RESOLVE_NO_XDEV`: no symlink followed, no mount crossed or
//!   triggered;
//! - only metadata is read: directories are listed (opened with
//!   `O_NOATIME`, which Linux allows on what the user owns) and entries
//!   `statx`ed; no file is opened, so no access time moves;
//! - at most [`MAX_ENTRIES`] entries per candidate and
//!   [`MAX_PENDING_BYTES`] of paths waiting to be listed per reader, and a
//!   candidate the plan has already judged is not read at all
//!   ([`Counts::behind`]).
//!
//! diskgc's own descriptors are never counted by its in-use scan
//! (docs/safety.md, In use), so a reader's open directory cannot make a
//! candidate look in use to this run; another diskgc process may see it.
//! Descriptors are bounded: a queue of at most `READ_AHEAD` reached
//! candidates, and two per reader.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use diskgc_core::engine::PRIVATE_TMP_PREFIX;
use diskgc_core::fs_kind::READ_AHEAD;
use diskgc_core::host::{Dev, FileType, MountEntry};
use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags};

use crate::host::{for_each_name, open_with_noatime};
use crate::stat::{STATX_MASK, stat_from_statx};

/// How many threads read ahead: enough requests in flight for a disk that
/// serves several at once (an NVMe drive), measured on the development
/// laptop (TASK-325).
pub(crate) const READERS: usize = 8;

/// The most entries read ahead in one candidate. A larger candidate's
/// metadata would not stay cached until its judgement anyway, and the
/// bound keeps a reader's pending directories small.
pub const MAX_ENTRIES: usize = 100_000;

/// The most bytes of path a reader holds for directories it has found but
/// not listed yet. Past it, a directory found is `statx`ed but not entered:
/// a wide tree of long names cannot make the readers hold much memory.
pub const MAX_PENDING_BYTES: usize = 1 << 20;

/// How much one candidate's read-ahead may read and hold.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bounds {
    /// Entries `statx`ed.
    pub(crate) entries: usize,
    /// Bytes of path held for directories found but not listed yet.
    pub(crate) pending_bytes: usize,
}

impl Bounds {
    /// The readers' bounds.
    pub(crate) const READER: Self = Self {
        entries: MAX_ENTRIES,
        pending_bytes: MAX_PENDING_BYTES,
    };
}

/// One reached candidate for a reader.
pub(crate) struct Job {
    /// The candidate directory (`O_PATH`).
    pub(crate) top: OwnedFd,
    /// The device, mount and owner every directory read below it must have.
    pub(crate) dev: Dev,
    pub(crate) mnt_id: u64,
    pub(crate) uid: u32,
    /// The mount points below it in the mount table, relative (`./a/b`):
    /// never `statx`ed.
    pub(crate) mounted: HashSet<PathBuf>,
    /// Its place among the hints ([`ReadAhead::hinted`]).
    pub(crate) hint: u64,
}

impl Job {
    /// The mount points of `mounts` strictly below `candidate`, relative
    /// to it as a reader names them (`./a/b`).
    pub(crate) fn mounted_below(mounts: &[MountEntry], candidate: &Path) -> HashSet<PathBuf> {
        mounts
            .iter()
            .filter_map(|m| m.mount_point.strip_prefix(candidate).ok())
            .filter(|rel| !rel.as_os_str().is_empty())
            .map(|rel| Path::new(".").join(rel))
            .collect()
    }
}

/// What the read-ahead did, for tests and measurements; nothing decides on
/// it.
#[derive(Debug, Default)]
pub struct Counts {
    /// Candidates handed to a reader.
    pub queued: AtomicU64,
    /// Candidates not reached (gone, not a directory, another user's,
    /// refused by the path rules): not read ahead.
    pub unreached: AtomicU64,
    /// Candidates not read ahead because the queue was full.
    pub queue_full: AtomicU64,
    /// Queued candidates dropped unread: the plan had judged them already.
    pub behind: AtomicU64,
    /// Directories the readers listed.
    pub directories: AtomicU64,
    /// Entries the readers `statx`ed.
    pub entries: AtomicU64,
}

/// The readers and their queue. Dropping it stops them: a reader finishes
/// the directory it is listing, then exits.
#[derive(Debug)]
pub(crate) struct ReadAhead {
    queue: Option<SyncSender<Job>>,
    stop: Arc<AtomicBool>,
    /// Hints given so far: the plan judges the candidate of hint `n` when
    /// `n + READ_AHEAD` hints have been given.
    hinted: Arc<AtomicU64>,
    pub(crate) counts: Arc<Counts>,
}

impl ReadAhead {
    /// Starts [`READERS`] threads (fewer if the system refuses some; with
    /// none, every candidate offered counts as `queue_full`).
    pub(crate) fn start() -> Self {
        let (queue, jobs) = mpsc::sync_channel::<Job>(READ_AHEAD);
        let jobs = Arc::new(Mutex::new(jobs));
        let stop = Arc::new(AtomicBool::new(false));
        let hinted = Arc::new(AtomicU64::new(0));
        let counts = Arc::new(Counts::default());
        let started = (0..READERS)
            .filter(|_| {
                let (jobs, stop, hinted, counts) =
                    (jobs.clone(), stop.clone(), hinted.clone(), counts.clone());
                std::thread::Builder::new()
                    .name("diskgc-ahead".into())
                    .spawn(move || reader(&jobs, &stop, &hinted, &counts))
                    .is_ok()
            })
            .count();
        Self {
            queue: (started > 0).then_some(queue),
            stop,
            hinted,
            counts,
        }
    }

    /// Counts one hint and returns its place.
    pub(crate) fn hint(&self) -> u64 {
        self.hinted.fetch_add(1, Ordering::Relaxed)
    }

    /// Hands `job` to a reader, or counts it `queue_full` when every
    /// reader is busy and the queue is full (the plan is ahead of the
    /// readers: the judgement reads it itself).
    pub(crate) fn offer(&self, job: Job) {
        let sent = self.queue.as_ref().map(|queue| queue.try_send(job));
        let count = match sent {
            Some(Ok(())) => &self.counts.queued,
            Some(Err(TrySendError::Full(_) | TrySendError::Disconnected(_))) | None => {
                &self.counts.queue_full
            }
        };
        count.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a candidate that was not reached.
    pub(crate) fn unreached(&self) {
        self.counts.unreached.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for ReadAhead {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Closing the queue ends each reader's wait for a job.
        self.queue = None;
    }
}

/// A reader thread: takes jobs until the queue closes or `stop` is set,
/// dropping those the plan has judged already. Signals need nothing here:
/// the handlers only set a flag, and Linux gives a signal sent to the
/// process to its main thread first when that thread does not block it,
/// as for the in-use scan's threads.
fn reader(jobs: &Mutex<Receiver<Job>>, stop: &AtomicBool, hinted: &AtomicU64, counts: &Counts) {
    loop {
        // The lock is held only while waiting for the next job.
        let job = match jobs.lock() {
            Ok(jobs) => jobs.recv(),
            Err(_) => return,
        };
        let Ok(job) = job else { return };
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if job.hint + READ_AHEAD as u64 <= hinted.load(Ordering::Relaxed) {
            counts.behind.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        warm(&job, Bounds::READER, stop, counts, &mut |_| {}, &mut |_| {});
    }
}

/// Reads the metadata below `job.top` (see the module docs): every
/// directory of `job.uid` on its device and mount listed, every name but a
/// mount point or `systemd-private-*` `statx`ed, within `bounds`, until
/// `stop`. Anything that fails is passed over. `listed` sees each
/// directory listed and `statted` each name `statx`ed, relative to the
/// candidate (for tests; the readers pass no-ops).
pub(crate) fn warm(
    job: &Job,
    bounds: Bounds,
    stop: &AtomicBool,
    counts: &Counts,
    listed: &mut dyn FnMut(&Path),
    statted: &mut dyn FnMut(&Path),
) {
    let limit = bounds.entries;
    let resolve = ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_MAGICLINKS
        | ResolveFlags::NO_XDEV;
    // Readable opens are non-blocking and take no terminal
    // (docs/safety.md, Path traversal), though a directory is all
    // `O_DIRECTORY` lets through.
    let open = OFlags::RDONLY
        | OFlags::DIRECTORY
        | OFlags::NOFOLLOW
        | OFlags::CLOEXEC
        | OFlags::NONBLOCK
        | OFlags::NOCTTY;
    let mut entries = 0usize;
    // Paths relative to the candidate: one directory open at a time.
    let mut pending = vec![PathBuf::from(".")];
    let mut pending_bytes = 1usize;
    while let Some(rel) = pending.pop() {
        pending_bytes -= rel.as_os_str().len();
        if entries >= limit || stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(dir) = open_with_noatime(open, |flags| {
            rustix::fs::openat2(&job.top, &rel, flags, Mode::empty(), resolve)
        }) else {
            continue;
        };
        counts.directories.fetch_add(1, Ordering::Relaxed);
        listed(&rel);
        // A listing that fails part-way is as good as it got.
        let _partial = for_each_name(&dir, |name| {
            if entries >= limit {
                return false;
            }
            if name.starts_with(PRIVATE_TMP_PREFIX.as_bytes()) {
                return true;
            }
            let name = Path::new(OsStr::from_bytes(name));
            let path = rel.join(name);
            if job.mounted.contains(&path) {
                return true;
            }
            entries += 1;
            statted(&path);
            let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT;
            if let Ok(x) = rustix::fs::statx(&dir, name, flags, STATX_MASK) {
                let stat = stat_from_statx(&x);
                if stat.file_type == Some(FileType::Directory)
                    && stat.dev == job.dev
                    && stat.mnt_id == Some(job.mnt_id)
                    && stat.uid == Some(job.uid)
                    && pending_bytes + path.as_os_str().len() <= bounds.pending_bytes
                {
                    pending_bytes += path.as_os_str().len();
                    pending.push(path);
                }
            }
            true
        });
    }
    counts.entries.fetch_add(
        u64::try_from(entries).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    use diskgc_core::host::Host;
    use rustix::fs::{Mode, OFlags};

    use super::{Bounds, Counts, Job, MAX_PENDING_BYTES, ReadAhead, warm};
    use crate::LinuxHost;
    use crate::stat::{STATX_MASK, stat_from_statx};

    /// A fresh directory under the test temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("dg-ra-{}-{serial}", std::process::id()));
            std::fs::create_dir(&dir).expect("create test dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn euid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    /// The job the host would make for `path`, against the live mount
    /// table.
    fn job(path: &Path) -> Job {
        let top: OwnedFd = rustix::fs::open(
            path,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open the candidate");
        let x = rustix::fs::statx(&top, "", rustix::fs::AtFlags::EMPTY_PATH, STATX_MASK)
            .expect("statx the candidate");
        let stat = stat_from_statx(&x);
        let mounts = LinuxHost::new()
            .expect("host setup")
            .mountinfo()
            .expect("mountinfo");
        Job {
            top,
            dev: stat.dev,
            mnt_id: stat.mnt_id.expect("the kernel reports mount ids"),
            uid: euid(),
            mounted: Job::mounted_below(&mounts, path),
            hint: 0,
        }
    }

    /// Warms `path` with `limit`: the counts, the directories listed and
    /// the names `statx`ed (relative, sorted).
    fn warmed(path: &Path, limit: usize) -> (Counts, Vec<PathBuf>, Vec<PathBuf>) {
        let counts = Counts::default();
        let (mut dirs, mut names) = (Vec::new(), Vec::new());
        let bounds = Bounds {
            entries: limit,
            pending_bytes: MAX_PENDING_BYTES,
        };
        warm(
            &job(path),
            bounds,
            &AtomicBool::new(false),
            &counts,
            &mut |rel| dirs.push(rel.to_owned()),
            &mut |rel| names.push(rel.to_owned()),
        );
        dirs.sort();
        names.sort();
        (counts, dirs, names)
    }

    fn file(path: &Path) {
        std::fs::write(path, b"x").expect("write fixture file");
    }

    /// TASK-325: every entry below the candidate is `statx`ed and every
    /// directory listed, except what the walk would refuse to enter: a
    /// symlink is `statx`ed but never followed (the outside directory it
    /// names holds three files no count sees), and a `systemd-private-*`
    /// name is passed over before its `statx` (neither it nor its file is
    /// counted).
    #[test]
    fn a_reader_reads_the_tree_and_follows_no_symlink_or_private_tmp() {
        let tmp = TempDir::new();
        let outside = tmp.0.join("outside");
        std::fs::create_dir(&outside).expect("mkdir");
        for name in ["o1", "o2", "o3"] {
            file(&outside.join(name));
        }
        let candidate = tmp.0.join("c");
        std::fs::create_dir_all(candidate.join("a/b")).expect("mkdir");
        std::fs::create_dir(candidate.join("systemd-private-x")).expect("mkdir");
        file(&candidate.join("systemd-private-x/hidden"));
        file(&candidate.join("f"));
        file(&candidate.join("a/g"));
        file(&candidate.join("a/b/h"));
        std::os::unix::fs::symlink(&outside, candidate.join("link")).expect("symlink");
        let (counts, dirs, names) = warmed(&candidate, usize::MAX);
        assert_eq!(dirs, [".", "./a", "./a/b"].map(PathBuf::from));
        assert_eq!(
            names,
            ["./a", "./a/b", "./a/b/h", "./a/g", "./f", "./link"].map(PathBuf::from)
        );
        assert_eq!(counts.entries.load(Ordering::Relaxed), 6);
        assert_eq!(counts.directories.load(Ordering::Relaxed), 3);
    }

    /// Reading ahead moves no access time: of a file (never opened) or of
    /// a directory of the user's (listed with `O_NOATIME`). docs/safety.md,
    /// Project dependency directories, rule 2, reads file atimes.
    #[test]
    fn a_reader_moves_no_access_time() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.0.join("d/e")).expect("mkdir");
        file(&tmp.0.join("d/f"));
        file(&tmp.0.join("d/e/g"));
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let paths = ["d", "d/e", "d/f", "d/e/g"].map(|p| tmp.0.join(p));
        for path in &paths {
            let times = std::fs::FileTimes::new().set_accessed(old);
            std::fs::File::open(path)
                .and_then(|f| f.set_times(times))
                .expect("backdate the access time");
        }
        let (counts, _, _) = warmed(&tmp.0.join("d"), usize::MAX);
        assert_eq!(counts.entries.load(Ordering::Relaxed), 3);
        for path in &paths {
            let accessed = std::fs::symlink_metadata(path)
                .and_then(|m| m.accessed())
                .expect("atime");
            assert_eq!(accessed, old, "{} was read", path.display());
        }
    }

    /// The entry bound holds, and a stopped read-ahead reads nothing more.
    #[test]
    fn a_reader_stops_at_its_bound_and_when_stopped() {
        let tmp = TempDir::new();
        for i in 0..10 {
            std::fs::create_dir(tmp.0.join(format!("d{i}"))).expect("mkdir");
        }
        let (counts, _, _) = warmed(&tmp.0, 4);
        assert_eq!(counts.entries.load(Ordering::Relaxed), 4);
        let counts = Counts::default();
        let mut dirs = 0;
        warm(
            &job(&tmp.0),
            Bounds::READER,
            &AtomicBool::new(true),
            &counts,
            &mut |_| dirs += 1,
            &mut |_| {},
        );
        assert_eq!((dirs, counts.entries.load(Ordering::Relaxed)), (0, 0));
    }

    /// No mount point is even `statx`ed (a dead FUSE or NFS server must
    /// not hang a reader; docs/safety.md, Before the walk), and no
    /// directory of another user is entered: below the host's `/dev`, the
    /// mounts on names in it (`/dev/shm`, `/dev/pts`, ...) are never named
    /// to `statx`, and its root-owned subdirectories are not listed.
    /// Skipped where nothing is mounted directly below `/dev`.
    #[test]
    fn a_reader_stats_no_mount_point_and_enters_no_other_users_directory() {
        let host = LinuxHost::new().expect("host setup");
        let mounts = host.mountinfo().expect("mountinfo");
        let below: Vec<PathBuf> = mounts
            .iter()
            .filter(|m| m.mount_point.parent() == Some(Path::new("/dev")))
            .map(|m| Path::new(".").join(m.mount_point.file_name().expect("a name")))
            .collect();
        if below.is_empty() {
            eprintln!("skipped: nothing is mounted directly below /dev");
            return;
        }
        let (counts, dirs, names) = warmed(Path::new("/dev"), 100_000);
        assert!(counts.entries.load(Ordering::Relaxed) > 0, "/dev was read");
        for mount in &below {
            assert!(
                !names.contains(mount),
                "{} is a mount point and was statx-ed",
                mount.display()
            );
        }
        if euid() != 0 {
            assert_eq!(dirs, [PathBuf::from(".")], "root's directories entered");
        }
    }

    /// The paths waiting to be listed stay within their bound: 40 sibling
    /// directories of 250-byte names hold more than a 4 KiB bound
    /// allows, so only the first are entered (all are `statx`ed).
    #[test]
    fn a_reader_holds_a_bounded_number_of_paths() {
        let tmp = TempDir::new();
        for i in 0..40 {
            let name = format!("{i:03}{}", "n".repeat(247));
            std::fs::create_dir(tmp.0.join(name)).expect("mkdir");
        }
        let counts = Counts::default();
        let mut dirs = 0usize;
        let bounds = Bounds {
            entries: usize::MAX,
            pending_bytes: 4096,
        };
        warm(
            &job(&tmp.0),
            bounds,
            &AtomicBool::new(false),
            &counts,
            &mut |_| dirs += 1,
            &mut |_| {},
        );
        assert_eq!(counts.entries.load(Ordering::Relaxed), 40);
        // The candidate, and the 16 that fit in 4 KiB (252 bytes each).
        assert_eq!(dirs, 1 + 4095 / 252);
    }

    /// A queued candidate the plan has already judged is dropped unread.
    #[test]
    fn a_reader_drops_a_candidate_the_plan_has_passed() {
        let tmp = TempDir::new();
        file(&tmp.0.join("f"));
        let readers = ReadAhead::start();
        for _ in 0..=diskgc_core::fs_kind::READ_AHEAD {
            readers.hint();
        }
        readers.offer(job(&tmp.0));
        let deadline = Instant::now() + Duration::from_secs(10);
        while readers.counts.behind.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(readers.counts.behind.load(Ordering::Relaxed), 1);
        assert_eq!(readers.counts.entries.load(Ordering::Relaxed), 0);
    }

    /// Through the port: a directory is handed to a reader and read; a
    /// symlink, a regular file and a missing path are not reached (the
    /// path rules of [`diskgc_core::resolve::resolve_in`]); a host built
    /// without the read-ahead starts nothing.
    #[test]
    fn the_host_reads_ahead_only_directories_reached_by_the_path_rules() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("d");
        std::fs::create_dir(&dir).expect("mkdir");
        for i in 0..5 {
            file(&dir.join(format!("f{i}")));
        }
        std::os::unix::fs::symlink(&dir, tmp.0.join("link")).expect("symlink");
        file(&tmp.0.join("plain"));
        let host = LinuxHost::new().expect("host setup");
        for name in ["link", "plain", "missing", "d"] {
            host.read_ahead(&tmp.0.join(name));
        }
        let counts = host.read_ahead_counts().expect("started by the first hint");
        let deadline = Instant::now() + Duration::from_secs(10);
        while counts.entries.load(Ordering::Relaxed) < 5 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(counts.unreached.load(Ordering::Relaxed), 3);
        assert_eq!(counts.queued.load(Ordering::Relaxed), 1);
        assert_eq!(counts.entries.load(Ordering::Relaxed), 5);
        let off = LinuxHost::new().expect("host setup").without_read_ahead();
        off.read_ahead(&dir);
        assert!(off.read_ahead_counts().is_none(), "nothing started");
    }
}
