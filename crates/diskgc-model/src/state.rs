//! The loaded model: an inode graph per filesystem, the mounts that attach
//! them, open file descriptions (with their `flock`s) and processes. Plain
//! data with the operations `ModelHost` and events need; nothing here knows
//! about calls, traces or checks.
//!
//! Derived, never stored: link counts, available space (allocated blocks
//! of every live inode, each counted once), which locks are held, mount
//! point paths. An inode is live while a directory reaches it or anything
//! (an open file, a process's cwd, root, exe or mapping) refers to it.
//!
//! Cost: reachability and link counts are derived once per filesystem and
//! cached until the filesystem next changes (every change goes through
//! `fs_mut` or `Fs::add`, which drop the cache), so a run of `statx` calls
//! costs one walk of the filesystem, not one per call; the index's
//! 98,000-entry model test (TASK-28) needs that. Other derived values (a
//! path from an inode, available space) are still linear per call.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use diskgc_core::host::{
    Access, Dev, DevIno, Environ, Errno, FileType, Ids, LockMode, MountEntry, Space, Stat,
    Timestamp,
};

use crate::spec::{ProcAccess, StatxField};

/// Allocation unit: files take their size rounded up to it, directories one.
pub const BLOCK: u64 = 4096;

/// Default owner and group of tree entries.
pub const DEFAULT_ID: u32 = 1000;

/// An object as reached through a mount: the mount id and the inode number
/// on that mount's filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Loc {
    pub mnt: u64,
    pub ino: u64,
}

#[derive(Debug, Clone)]
pub enum Kind {
    File {
        cachedir_tag: bool,
        content: Vec<u8>,
    },
    Dir(BTreeMap<OsString, u64>),
    Symlink(PathBuf),
    Socket,
    Fifo,
    /// A character device node (only its type is modelled).
    CharDevice,
}

#[derive(Debug, Clone)]
pub struct Inode {
    pub kind: Kind,
    pub uid: u32,
    pub gid: u32,
    pub perm: u32,
    pub size: u64,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
    pub btime: Timestamp,
}

impl Inode {
    pub fn new(kind: Kind, now: Timestamp) -> Self {
        let perm = if matches!(kind, Kind::Dir(_)) {
            0o755
        } else {
            0o644
        };
        Self {
            kind,
            uid: DEFAULT_ID,
            gid: DEFAULT_ID,
            perm,
            size: 0,
            mtime: now,
            ctime: now,
            btime: now,
        }
    }

    pub fn file_type(&self) -> FileType {
        match self.kind {
            Kind::File { .. } => FileType::Regular,
            Kind::Dir(_) => FileType::Directory,
            Kind::Symlink(_) => FileType::Symlink,
            Kind::Socket => FileType::Socket,
            Kind::Fifo => FileType::Fifo,
            Kind::CharDevice => FileType::CharDevice,
        }
    }

    pub fn entries(&self) -> Option<&BTreeMap<OsString, u64>> {
        match &self.kind {
            Kind::Dir(entries) => Some(entries),
            _ => None,
        }
    }

    fn entries_mut(&mut self) -> Option<&mut BTreeMap<OsString, u64>> {
        match &mut self.kind {
            Kind::Dir(entries) => Some(entries),
            _ => None,
        }
    }

    /// Allocated bytes.
    pub fn allocated(&self) -> u64 {
        match self.kind {
            Kind::File { .. } => self.size.div_ceil(BLOCK) * BLOCK,
            Kind::Dir(_) => BLOCK,
            Kind::Symlink(_) | Kind::Socket | Kind::Fifo | Kind::CharDevice => 0,
        }
    }
}

/// One filesystem (one device).
#[derive(Debug, Clone)]
pub struct Fs {
    pub dev: Dev,
    pub fs_type: String,
    pub size: u64,
    pub other_used: u64,
    pub missing: BTreeSet<StatxField>,
    pub root: u64,
    /// Private, so every change goes through `State::fs_mut` or
    /// [`Fs::add`], which drop the cached [`Links`].
    inodes: BTreeMap<u64, Inode>,
    links: RefCell<Option<Rc<Links>>>,
}

/// What reachability derives, cached per filesystem.
#[derive(Debug, Default)]
struct Links {
    /// Inodes reachable from the root through directory entries.
    reachable: BTreeSet<u64>,
    /// Per inode, the entries naming it in reachable directories.
    named: BTreeMap<u64, usize>,
    /// Per directory, how many of its entries are directories.
    subdirs: BTreeMap<u64, usize>,
}

impl Fs {
    pub fn new(dev: Dev, fs_type: String, size: u64, other_used: u64, now: Timestamp) -> Self {
        let root = 2;
        Self {
            dev,
            fs_type,
            size,
            other_used,
            missing: BTreeSet::new(),
            root,
            inodes: BTreeMap::from([(root, Inode::new(Kind::Dir(BTreeMap::new()), now))]),
            links: RefCell::new(None),
        }
    }

    /// Drops what was derived from the inodes (before any change).
    fn changing(&mut self) {
        *self.links.get_mut() = None;
    }

    fn links(&self) -> Rc<Links> {
        if let Some(links) = &*self.links.borrow() {
            return Rc::clone(links);
        }
        let reachable = self.closure([self.root]);
        let mut named: BTreeMap<u64, usize> = BTreeMap::new();
        for entries in reachable
            .iter()
            .filter_map(|ino| self.inodes[ino].entries())
        {
            for child in entries.values() {
                *named.entry(*child).or_default() += 1;
            }
        }
        let subdirs = self
            .inodes
            .iter()
            .filter_map(|(ino, inode)| {
                let entries = inode.entries()?;
                let dirs = entries
                    .values()
                    .filter(|child| self.inodes[*child].entries().is_some())
                    .count();
                Some((*ino, dirs))
            })
            .collect();
        let links = Rc::new(Links {
            reachable,
            named,
            subdirs,
        });
        *self.links.borrow_mut() = Some(Rc::clone(&links));
        links
    }

    fn add(&mut self, inode: Inode) -> u64 {
        self.changing();
        let ino = self.inodes.keys().next_back().map_or(2, |last| last + 1);
        self.inodes.insert(ino, inode);
        ino
    }

    /// Inodes reachable from the root through directory entries.
    fn reachable(&self) -> BTreeSet<u64> {
        self.links().reachable.clone()
    }

    /// `start` and everything below the directories among them.
    fn closure(&self, start: impl IntoIterator<Item = u64>) -> BTreeSet<u64> {
        let mut seen = BTreeSet::new();
        let mut todo: Vec<u64> = start.into_iter().collect();
        while let Some(ino) = todo.pop() {
            if !seen.insert(ino) {
                continue;
            }
            if let Some(entries) = self.inodes.get(&ino).and_then(Inode::entries) {
                todo.extend(entries.values().copied());
            }
        }
        seen
    }
}

#[derive(Debug, Clone)]
pub struct Mount {
    pub id: u64,
    pub parent: Option<u64>,
    pub dev: Dev,
    /// The directory this mount covers, on its parent mount.
    pub on: Option<Loc>,
    pub read_only: bool,
    /// Its server never answers ([`crate::spec::MountSpec::dead`]).
    pub dead: bool,
}

/// An open file description: what `open` creates and `flock` locks. Shared
/// by every fd that refers to it; gone (with its lock) when the last one
/// closes.
#[derive(Debug, Clone)]
pub struct Ofd {
    pub loc: Loc,
    /// The path it was opened by (for messages and event matching; not
    /// updated by renames).
    pub path: PathBuf,
    pub access: Access,
    pub lock: Option<LockMode>,
}

/// A process's reference to an object (cwd, root, exe, a mapping).
#[derive(Debug, Clone)]
pub struct Ref {
    pub loc: Loc,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Proc {
    /// Monotonic process incarnation, independent of pid and virtual time.
    pub generation: u64,
    pub exe: Option<Ref>,
    pub cwd: Ref,
    pub root: Ref,
    pub environ: Environ,
    /// fd number -> open file description id.
    pub fds: BTreeMap<u32, u64>,
    pub maps: Vec<Ref>,
    pub access: ProcAccess,
    /// What `status` shows (and who owns `/proc/<pid>/fd`).
    pub name: OsString,
    pub uid: Ids,
    pub gid: Ids,
    pub dumpable: bool,
    pub cap_permitted: u64,
    /// `/proc/<pid>/uid_map` as diskgc reads it (`None`: unreadable).
    pub uid_map: Option<String>,
    /// Its `NSpid:` levels, the first its pid (`None`: unreadable).
    pub nspid: Option<Vec<u32>>,
    /// Exited, not reaped (see `ProcessSpec::zombie`).
    pub zombie: bool,
    /// Its command line (`None`: unreadable).
    pub cmdline: Option<Vec<OsString>>,
    /// Its view's prefixes, mapped to the model's paths (see
    /// `ProcessSpec::view`).
    pub view: BTreeMap<PathBuf, PathBuf>,
}

/// What reading a modelled `uid_map` gives (`None`: it does not exist).
pub fn uid_map(text: Option<&str>) -> diskgc_core::host::UidMap {
    text.map_or(diskgc_core::host::UidMap::Absent, |t| {
        diskgc_core::host::UidMap::Map(t.to_owned())
    })
}

/// The `uid_map` of the initial user namespace, as `/proc` prints it:
/// diskgc's own, and every process's unless the model says otherwise.
pub const INIT_UID_MAP: &str = "         0          0 4294967295\n";

/// The `fstatfs` magic of a modelled filesystem type name, as Linux's
/// `include/uapi/linux/magic.h` defines it; every other name is 0, which
/// no check accepts (a model cannot invent a type Owner PID trusts).
pub fn fs_magic(fs_type: &str) -> u64 {
    match fs_type {
        "tmpfs" => 0x0102_1994,
        "ext2" | "ext3" | "ext4" => 0xEF53,
        "xfs" => 0x5846_5342,
        "btrfs" => 0x9123_683E,
        "bcachefs" => 0xCA45_1A4E,
        "f2fs" => 0xF2F5_2010,
        "overlay" => 0x794C_7630,
        "virtiofs" | "fuse" => 0x6573_5546,
        "9p" => 0x0102_1997,
        "zfs" => 0x2FC1_2FC1,
        "nfs" | "nfs4" => 0x6969,
        "proc" => 0x9FA0,
        _ => 0,
    }
}

/// diskgc's own pid when the model does not give one.
pub const DEFAULT_OWN_PID: u32 = 65000;

/// Why a model operation failed: an errno for the code under test, or a
/// mistake in the fixture or test (never returned to the code under test).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    Errno(Errno),
    Model(String),
}

impl From<Errno> for Fault {
    fn from(e: Errno) -> Self {
        Fault::Errno(e)
    }
}

/// How a path walk treats symlinks and mounts (the `openat2` flags that
/// matter to lookup).
#[derive(Debug, Clone, Copy, Default)]
pub struct WalkRules {
    pub beneath: bool,
    pub no_symlinks: bool,
    pub no_xdev: bool,
    /// `O_PATH | O_NOFOLLOW`: a trailing symlink is opened, not refused.
    pub open_trailing_symlink: bool,
    /// A walk for diskgc's own `openat2`, not a fixture's lookup: a name
    /// looked up on a `dead` mount, or a step into one, fails the test
    /// whatever the walk's outcome (on Linux the lookup waits on the server
    /// even when the answer would be ENOENT).
    pub refuse_dead: bool,
}

#[derive(Debug, Clone)]
pub struct State {
    /// First delivered signal, retained for the run.
    pub interrupted: Option<diskgc_core::report::Signal>,
    /// In mount order.
    pub mounts: Vec<Mount>,
    pub fss: BTreeMap<Dev, Fs>,
    pub ofds: BTreeMap<u64, Ofd>,
    /// The open file descriptions diskgc itself holds (one per `ModelFd`).
    pub own_ofds: BTreeSet<u64>,
    pub procs: BTreeMap<u32, Proc>,
    /// Never reused, even when a process exits and its pid is recycled.
    pub next_process_generation: u64,
    /// diskgc's own pid (listed with the processes, see `ModelHost`).
    pub own_pid: u32,
    /// diskgc's own `uid_map` (`None`: unreadable).
    pub own_uid_map: Option<String>,
    /// diskgc's own (effective) user id.
    pub own_uid: u32,
    /// The account database: user name to uid.
    pub users: BTreeMap<String, u32>,
    /// Owner PID's preconditions as the held `/proc` answers them.
    pub pid_namespace_initial: bool,
    pub proc_hidepid: bool,
    /// The modelled sandbox gate.
    pub sandbox_gate: crate::spec::GateSpec,
    /// Exit code of modelled command actions; a real child is never run.
    pub command_exit: u8,
    pub command_error_after_start: bool,
    pub docker: crate::spec::DockerSpec,
    /// The socket files a socket is bound to (in diskgc's network
    /// namespace).
    pub bound_sockets: BTreeSet<DevIno>,
    pub kernel: String,
    pub hostname: String,
    pub wall_start: jiff::Timestamp,
    /// Virtual time since the start of the run.
    pub elapsed: Duration,
}

impl State {
    pub fn new(wall_start: jiff::Timestamp, kernel: String) -> Self {
        Self {
            interrupted: None,
            mounts: Vec::new(),
            fss: BTreeMap::new(),
            ofds: BTreeMap::new(),
            own_ofds: BTreeSet::new(),
            procs: BTreeMap::new(),
            next_process_generation: 1,
            own_pid: DEFAULT_OWN_PID,
            own_uid_map: Some(INIT_UID_MAP.to_owned()),
            own_uid: DEFAULT_ID,
            users: BTreeMap::new(),
            pid_namespace_initial: true,
            proc_hidepid: false,
            sandbox_gate: crate::spec::GateSpec::Open,
            command_exit: 0,
            command_error_after_start: false,
            docker: crate::spec::DockerSpec::default(),
            bound_sockets: BTreeSet::new(),
            kernel,
            hostname: "model-host".to_owned(),
            wall_start,
            elapsed: Duration::ZERO,
        }
    }

    // ---- Time ----

    /// Wall-clock time now, as statx reports it.
    pub fn wall_now(&self) -> Timestamp {
        let now = self
            .wall_start
            .checked_add(jiff::SignedDuration::try_from(self.elapsed).unwrap_or_default())
            .unwrap_or(self.wall_start);
        timestamp(now)
    }

    // ---- Mounts ----

    pub fn mount(&self, id: u64) -> &Mount {
        self.mounts
            .iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("model bug: no mount {id}"))
    }

    pub fn root_mount(&self) -> &Mount {
        self.mounts
            .iter()
            .find(|m| m.parent.is_none())
            .expect("model bug: no root mount")
    }

    pub fn fs(&self, mnt: u64) -> &Fs {
        &self.fss[&self.mount(mnt).dev]
    }

    fn fs_mut(&mut self, mnt: u64) -> &mut Fs {
        let dev = self.mount(mnt).dev;
        let fs = self.fss.get_mut(&dev).expect("model bug: mount without fs");
        fs.changing();
        fs
    }

    pub fn mount_root(&self, mnt: u64) -> Loc {
        Loc {
            mnt,
            ino: self.fs(mnt).root,
        }
    }

    pub fn inode(&self, loc: Loc) -> &Inode {
        self.fs(loc.mnt)
            .inodes
            .get(&loc.ino)
            .unwrap_or_else(|| panic!("model bug: no inode {loc:?}"))
    }

    pub fn inode_mut(&mut self, loc: Loc) -> &mut Inode {
        self.fs_mut(loc.mnt)
            .inodes
            .get_mut(&loc.ino)
            .unwrap_or_else(|| panic!("model bug: no inode {loc:?}"))
    }

    pub fn dev_ino(&self, loc: Loc) -> DevIno {
        DevIno {
            dev: self.mount(loc.mnt).dev,
            ino: loc.ino,
        }
    }

    /// The topmost mount covering `loc`, if any (a later mount on the same
    /// directory covers an earlier one).
    fn mounted_on(&self, loc: Loc) -> Option<&Mount> {
        self.mounts.iter().rev().find(|m| m.on == Some(loc))
    }

    /// `loc`, or the root of the topmost stack of mounts on it.
    fn through_mounts(&self, mut loc: Loc) -> (Loc, bool) {
        let mut crossed = false;
        while let Some(m) = self.mounted_on(loc) {
            loc = self.mount_root(m.id);
            crossed = true;
        }
        (loc, crossed)
    }

    /// The path of an object, as the process's root sees it (`None` if no
    /// directory reaches it any more).
    pub fn path_of(&self, loc: Loc) -> Option<PathBuf> {
        let mount = self.mount(loc.mnt);
        let fs = self.fs(loc.mnt);
        if loc.ino == fs.root {
            return match mount.on {
                None => Some(PathBuf::from("/")),
                Some(on) => self.path_of(on),
            };
        }
        let (parent, name) = fs.inodes.iter().find_map(|(ino, inode)| {
            let entries = inode.entries()?;
            let (name, _) = entries.iter().find(|(_, child)| **child == loc.ino)?;
            Some((*ino, name))
        })?;
        Some(
            self.path_of(Loc {
                mnt: loc.mnt,
                ino: parent,
            })?
            .join(name),
        )
    }

    /// Mounts a fresh filesystem on the existing directory `spec.at` (an
    /// event): the new mount's parent is the mount that directory is on.
    pub fn mount_on(&mut self, spec: &crate::spec::MountEventSpec) -> Result<(), Fault> {
        if self.fss.contains_key(&spec.dev.0) || self.mounts.iter().any(|m| m.id == spec.id) {
            return Err(Fault::Model(format!(
                "mount: id {} or device {} already exists",
                spec.id, spec.dev.0
            )));
        }
        let on = self.lookup(&spec.at)?;
        if self.inode(on).entries().is_none() {
            return Err(Fault::Model(format!(
                "mount: {} is not a directory",
                spec.at.display()
            )));
        }
        let dev = spec.dev.0;
        let fs = Fs::new(dev, spec.fs.clone(), spec.size.0, 0, self.wall_now());
        self.fss.insert(dev, fs);
        self.mounts.push(Mount {
            id: spec.id,
            parent: Some(on.mnt),
            dev,
            on: Some(on),
            read_only: false,
            dead: false,
        });
        Ok(())
    }

    pub fn mountinfo(&self) -> Vec<MountEntry> {
        self.mounts
            .iter()
            .map(|m| {
                let fs = &self.fss[&m.dev];
                MountEntry {
                    id: m.id,
                    parent_id: m.parent.unwrap_or(0),
                    dev: m.dev,
                    fs_root: PathBuf::from("/"),
                    mount_point: m
                        .on
                        .map_or(Some(PathBuf::from("/")), |on| self.path_of(on))
                        .unwrap_or_else(|| PathBuf::from("/(detached)")),
                    fs_type: fs.fs_type.clone(),
                    source: fs.fs_type.clone(),
                    read_only: m.read_only,
                }
            })
            .collect()
    }

    // ---- Lookup ----

    /// Walks `path` from `start` (a directory) under `rules`. `.` stays;
    /// `..` is refused under `beneath` (`EXDEV`) and not modelled
    /// otherwise. Symlinks are never followed.
    pub fn walk(&self, start: Loc, path: &Path, rules: WalkRules) -> Result<Loc, Fault> {
        let names: Vec<&OsStr> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => Some(Ok(name)),
                Component::CurDir | Component::RootDir => None,
                Component::ParentDir => Some(Err(())),
                Component::Prefix(_) => unreachable!("no path prefixes on Linux"),
            })
            .collect::<Result<_, ()>>()
            .map_err(|()| {
                if rules.beneath {
                    Fault::Errno(Errno::EXDEV)
                } else {
                    Fault::Model(format!("'..' in {} is not modelled", path.display()))
                }
            })?;
        let refuse_dead = |at: Loc, step: &str| {
            if rules.refuse_dead && self.mount(at.mnt).dead {
                panic!(
                    "ModelHost call check failed: openat2 of {} {step} dead mount {}, whose \
                     server never answers: the lookup would hang; judge it from mountinfo \
                     without touching it",
                    path.display(),
                    at.mnt
                );
            }
        };
        let mut here = start;
        for (i, name) in names.iter().enumerate() {
            let last = i + 1 == names.len();
            refuse_dead(here, "looks up a name on");
            let entries = self.inode(here).entries().ok_or(Errno::ENOTDIR)?;
            let child = *entries.get(*name).ok_or(Errno::ENOENT)?;
            let (next, crossed) = self.through_mounts(Loc {
                mnt: here.mnt,
                ino: child,
            });
            refuse_dead(next, "steps into");
            if crossed && rules.no_xdev {
                return Err(Errno::EXDEV.into());
            }
            if matches!(self.inode(next).kind, Kind::Symlink(_)) {
                if last && rules.open_trailing_symlink {
                    return Ok(next);
                }
                if rules.no_symlinks {
                    return Err(Errno::ELOOP.into());
                }
                return Err(Fault::Model(format!(
                    "symlink at {} in {}: the model does not follow symlinks",
                    name.display(),
                    path.display()
                )));
            }
            here = next;
        }
        Ok(here)
    }

    /// Looks up an absolute path from the root, crossing mounts and refusing
    /// symlinks: how fixtures and events name objects.
    pub fn lookup(&self, path: &Path) -> Result<Loc, Fault> {
        let (root, _) = self.through_mounts(self.mount_root(self.root_mount().id));
        self.walk(
            root,
            path,
            WalkRules {
                no_symlinks: true,
                ..WalkRules::default()
            },
        )
    }

    /// [`State::lookup`], except that a trailing symlink is the object
    /// named (`lstat`): how fixtures check that a link itself survived.
    pub fn lookup_link(&self, path: &Path) -> Result<Loc, Fault> {
        let (root, _) = self.through_mounts(self.mount_root(self.root_mount().id));
        self.walk(
            root,
            path,
            WalkRules {
                no_symlinks: true,
                open_trailing_symlink: true,
                ..WalkRules::default()
            },
        )
    }

    /// The single entry `name` of directory `dir`, through mounts on it, not
    /// following a symlink.
    pub fn child(&self, dir: Loc, name: &OsStr) -> Result<Loc, Errno> {
        let entries = self.inode(dir).entries().ok_or(Errno::ENOTDIR)?;
        let ino = *entries.get(name).ok_or(Errno::ENOENT)?;
        Ok(self.through_mounts(Loc { mnt: dir.mnt, ino }).0)
    }

    // ---- Derived attributes ----

    /// Every live inode of a filesystem: reachable, or held by an open
    /// file, a process or a mount (and what lies below those).
    fn live(&self, dev: Dev) -> BTreeSet<u64> {
        let fs = &self.fss[&dev];
        let on_dev = |loc: &Loc| (self.mount(loc.mnt).dev == dev).then_some(loc.ino);
        let held = self
            .ofds
            .values()
            .map(|o| o.loc)
            .chain(self.procs.values().flat_map(|p| {
                p.exe
                    .iter()
                    .chain([&p.cwd, &p.root])
                    .chain(&p.maps)
                    .map(|r| r.loc)
            }))
            .chain(self.mounts.iter().filter_map(|m| m.on))
            .filter_map(|loc| on_dev(&loc));
        let mut live = fs.reachable();
        live.extend(fs.closure(held));
        live
    }

    pub fn space(&self, mnt: u64) -> Space {
        let fs = self.fs(mnt);
        let allocated: u64 = self
            .live(fs.dev)
            .iter()
            .map(|ino| fs.inodes[ino].allocated())
            .sum();
        Space {
            avail_bytes: fs
                .size
                .saturating_sub(fs.other_used)
                .saturating_sub(allocated),
            size_bytes: fs.size,
        }
    }

    /// Link count: directory entries naming it among reachable directories
    /// (for a directory: 2 plus its subdirectories, 0 once detached).
    fn nlink(&self, loc: Loc) -> u32 {
        let fs = self.fs(loc.mnt);
        let links = fs.links();
        let named = links.named.get(&loc.ino).copied().unwrap_or(0);
        let count = match self.inode(loc).entries() {
            Some(_) if loc.ino != fs.root && named == 0 => 0,
            Some(_) => 2 + links.subdirs.get(&loc.ino).copied().unwrap_or(0),
            None => named,
        };
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    pub fn is_linked(&self, loc: Loc) -> bool {
        let fs = self.fs(loc.mnt);
        loc.ino == fs.root || fs.links().reachable.contains(&loc.ino)
    }

    pub fn stat(&self, loc: Loc) -> Stat {
        let fs = self.fs(loc.mnt);
        let inode = self.inode(loc);
        fn when<T>(fs: &Fs, field: StatxField, value: T) -> Option<T> {
            (!fs.missing.contains(&field)).then_some(value)
        }
        Stat {
            dev: fs.dev,
            file_type: when(fs, StatxField::Type, inode.file_type()),
            perm: when(fs, StatxField::Mode, inode.perm),
            // Link counting walks the inode graph. A modelled filesystem
            // that cannot report nlink must not pay for a value it omits.
            nlink: (!fs.missing.contains(&StatxField::Nlink)).then(|| self.nlink(loc)),
            uid: when(fs, StatxField::Uid, inode.uid),
            gid: when(fs, StatxField::Gid, inode.gid),
            ino: when(fs, StatxField::Ino, loc.ino),
            size: when(fs, StatxField::Size, inode.size),
            blocks: when(fs, StatxField::Blocks, inode.allocated() / 512),
            mtime: when(fs, StatxField::Mtime, inode.mtime),
            ctime: when(fs, StatxField::Ctime, inode.ctime),
            btime: when(fs, StatxField::Btime, inode.btime),
            mnt_id: when(fs, StatxField::MntId, loc.mnt),
            mount_root: when(fs, StatxField::MountRoot, loc.ino == fs.root),
        }
    }

    // ---- Locks ----

    /// Whether `ofd` could take `mode` now: no other open file description
    /// of the same object holds a conflicting lock.
    pub fn lock_free(&self, ofd: u64, mode: LockMode) -> bool {
        let loc = self.ofds[&ofd].loc;
        let me = self.dev_ino(loc);
        !self.ofds.iter().any(|(id, other)| {
            *id != ofd
                && self.dev_ino(other.loc) == me
                && match (other.lock, mode) {
                    (None, _) => false,
                    (Some(LockMode::Shared), LockMode::Shared) => false,
                    (Some(_), _) => true,
                }
        })
    }

    // ---- Open file descriptions ----

    pub fn open_ofd(&mut self, loc: Loc, path: PathBuf, access: Access) -> u64 {
        let id = self.ofds.keys().next_back().map_or(1, |last| last + 1);
        self.ofds.insert(
            id,
            Ofd {
                loc,
                path,
                access,
                lock: None,
            },
        );
        id
    }

    /// Drops an open file description no fd refers to any more (with its
    /// lock).
    fn release_if_unused(&mut self, ofd: u64) {
        let used = self.own_ofds.contains(&ofd)
            || self
                .procs
                .values()
                .any(|p| p.fds.values().any(|o| *o == ofd));
        if !used {
            self.ofds.remove(&ofd);
        }
    }

    /// diskgc closed an fd.
    pub fn close_own(&mut self, ofd: u64) {
        self.own_ofds.remove(&ofd);
        self.release_if_unused(ofd);
    }

    /// A process opens `path` (lowest free fd from 3, unless given).
    pub fn proc_open(&mut self, pid: u32, path: &Path, fd: Option<u32>) -> Result<u32, Fault> {
        let loc = self.lookup(path)?;
        let ofd = self.open_ofd(loc, path.to_owned(), Access::Read);
        self.proc_attach(pid, ofd, fd)
    }

    /// Gives process `pid` an fd on `ofd`.
    pub fn proc_attach(&mut self, pid: u32, ofd: u64, fd: Option<u32>) -> Result<u32, Fault> {
        let proc = self
            .procs
            .get_mut(&pid)
            .ok_or_else(|| Fault::Model(format!("no process {pid}")))?;
        let fd = match fd {
            Some(fd) if proc.fds.contains_key(&fd) => {
                return Err(Fault::Model(format!("process {pid} has fd {fd} twice")));
            }
            Some(fd) => fd,
            None => (3..)
                .find(|n| !proc.fds.contains_key(n))
                .expect("fewer than 2^32 fds"),
        };
        proc.fds.insert(fd, ofd);
        Ok(fd)
    }

    /// A process closes every fd it opened by `path`.
    pub fn proc_close(&mut self, pid: u32, path: &Path) -> Result<(), Fault> {
        let proc = self
            .procs
            .get(&pid)
            .ok_or_else(|| Fault::Model(format!("close: no process {pid}")))?;
        let closing: Vec<(u32, u64)> = proc
            .fds
            .iter()
            .filter(|(_, ofd)| self.ofds[*ofd].path == path)
            .map(|(fd, ofd)| (*fd, *ofd))
            .collect();
        if closing.is_empty() {
            return Err(Fault::Model(format!(
                "close: process {pid} has no fd opened by {}",
                path.display()
            )));
        }
        for (fd, ofd) in closing {
            self.procs.get_mut(&pid).expect("checked").fds.remove(&fd);
            self.release_if_unused(ofd);
        }
        Ok(())
    }

    /// Process `pid` forks `child`: a copy whose fds refer to the same
    /// open file descriptions.
    pub fn proc_fork(&mut self, pid: u32, child: u32) -> Result<(), Fault> {
        if self.procs.contains_key(&child) || child == self.own_pid {
            return Err(Fault::Model(format!("fork: pid {child} exists")));
        }
        let mut copy = self
            .procs
            .get(&pid)
            .ok_or_else(|| Fault::Model(format!("fork: no process {pid}")))?
            .clone();
        // The child's pids in the parent's inner namespaces are not
        // modelled: refuse rather than diverge from Linux silently.
        if copy.nspid.as_deref() != Some(&[pid][..]) || copy.zombie {
            return Err(Fault::Model(format!(
                "fork: process {pid} has a modelled nspid or is a zombie; forking it is not modelled"
            )));
        }
        copy.generation = self.next_process_generation;
        self.next_process_generation += 1;
        copy.nspid = Some(vec![child]);
        self.procs.insert(child, copy);
        Ok(())
    }

    pub fn proc_exit(&mut self, pid: u32) -> Result<(), Fault> {
        let proc = self
            .procs
            .remove(&pid)
            .ok_or_else(|| Fault::Model(format!("exit: no process {pid}")))?;
        for ofd in proc.fds.into_values() {
            self.release_if_unused(ofd);
        }
        Ok(())
    }

    // ---- Changes to the tree ----

    /// Splits an absolute path into its parent directory (looked up) and
    /// last name.
    fn parent_of<'p>(&self, path: &'p Path) -> Result<(Loc, &'p OsStr), Fault> {
        let name = path
            .file_name()
            .ok_or_else(|| Fault::Model(format!("{} has no last name", path.display())))?;
        let parent = path.parent().expect("a path with a file name has a parent");
        Ok((self.lookup(parent)?, name))
    }

    /// Adds entry `name` -> `ino` to directory `dir`, stamping it as
    /// changed now.
    pub fn link(&mut self, dir: Loc, name: &OsStr, ino: u64) -> Result<(), Fault> {
        let now = self.wall_now();
        let parent = self.inode_mut(dir);
        let entries = parent.entries_mut().ok_or(Errno::ENOTDIR)?;
        if entries.contains_key(name) {
            return Err(Errno::EEXIST.into());
        }
        entries.insert(name.to_owned(), ino);
        parent.mtime = now;
        parent.ctime = now;
        Ok(())
    }

    /// Removes entry `name` from directory `dir` (stamping it), returning
    /// the inode it named.
    fn unlink_entry(&mut self, dir: Loc, name: &OsStr) -> Result<u64, Fault> {
        let now = self.wall_now();
        let parent = self.inode_mut(dir);
        let entries = parent.entries_mut().ok_or(Errno::ENOTDIR)?;
        let ino = entries.remove(name).ok_or(Errno::ENOENT)?;
        parent.mtime = now;
        parent.ctime = now;
        Ok(ino)
    }

    /// Creates a new inode as entry `name` of `dir`.
    pub fn create_in(&mut self, dir: Loc, name: &OsStr, kind: Kind) -> Result<Loc, Fault> {
        let now = self.wall_now();
        let ino = self.fs_mut(dir.mnt).add(Inode::new(kind, now));
        self.link(dir, name, ino)?;
        Ok(Loc { mnt: dir.mnt, ino })
    }

    /// Creates `path` (and missing parent directories) as `kind`. An
    /// existing directory is fine when a directory is wanted.
    pub fn create_path(&mut self, path: &Path, kind: Kind) -> Result<Loc, Fault> {
        let want_dir = matches!(kind, Kind::Dir(_));
        let mut here = self.lookup(Path::new("/"))?;
        let names: Vec<&OsStr> = normal_names(path)?;
        for (i, name) in names.iter().enumerate() {
            let last = i + 1 == names.len();
            match self.child(here, name) {
                Ok(_) if last && !want_dir => {
                    return Err(Fault::Model(format!("{} exists already", path.display())));
                }
                Ok(next) => {
                    if self.inode(next).entries().is_none() {
                        return Err(Errno::ENOTDIR.into());
                    }
                    here = next;
                }
                Err(Errno::ENOENT) if last => return self.create_in(here, name, kind),
                Err(Errno::ENOENT) => {
                    here = self.create_in(here, name, Kind::Dir(BTreeMap::new()))?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(here)
    }

    /// Whether a mount sits on `loc` or anywhere below it.
    fn holds_mount(&self, loc: Loc) -> bool {
        let below = self.fs(loc.mnt).closure([loc.ino]);
        self.mounts
            .iter()
            .filter_map(|m| m.on)
            .any(|on| on.mnt == loc.mnt && below.contains(&on.ino))
    }

    /// `rm -r`: unlinks every entry below a directory, then its name. An
    /// object something still holds open keeps only itself alive, empty.
    pub fn remove_path(&mut self, path: &Path) -> Result<(), Fault> {
        let (dir, name) = self.parent_of(path)?;
        let entries = self.inode(dir).entries().ok_or(Errno::ENOTDIR)?;
        let ino = *entries.get(name).ok_or(Errno::ENOENT)?;
        let top = Loc { mnt: dir.mnt, ino };
        if self.holds_mount(top) {
            return Err(Errno::EBUSY.into());
        }
        let now = self.wall_now();
        for below in self.fs(top.mnt).closure([top.ino]) {
            let inode = self.inode_mut(Loc {
                mnt: top.mnt,
                ino: below,
            });
            if let Some(entries) = inode.entries_mut()
                && !entries.is_empty()
            {
                entries.clear();
                inode.mtime = now;
                inode.ctime = now;
            }
        }
        self.unlink_entry(dir, name)?;
        Ok(())
    }

    /// `rename(from, to)` within one filesystem.
    pub fn rename_path(&mut self, from: &Path, to: &Path) -> Result<(), Fault> {
        if to.starts_with(from) {
            return Err(Errno::EINVAL.into());
        }
        let (from_dir, from_name) = self.parent_of(from)?;
        let (to_dir, to_name) = self.parent_of(to)?;
        if from_dir.mnt != to_dir.mnt {
            return Err(Errno::EXDEV.into());
        }
        let moving = *self
            .inode(from_dir)
            .entries()
            .and_then(|e| e.get(from_name))
            .ok_or(Errno::ENOENT)?;
        if self
            .mounted_on(Loc {
                mnt: from_dir.mnt,
                ino: moving,
            })
            .is_some()
        {
            return Err(Errno::EBUSY.into());
        }
        let moving_is_dir = self
            .inode(Loc {
                mnt: from_dir.mnt,
                ino: moving,
            })
            .entries()
            .is_some();
        let existing = self
            .inode(to_dir)
            .entries()
            .ok_or(Errno::ENOTDIR)?
            .get(to_name)
            .copied();
        if let Some(old) = existing {
            if self
                .mounted_on(Loc {
                    mnt: to_dir.mnt,
                    ino: old,
                })
                .is_some()
            {
                return Err(Errno::EBUSY.into());
            }
            let old_inode = self.inode(Loc {
                mnt: to_dir.mnt,
                ino: old,
            });
            match (moving_is_dir, old_inode.entries()) {
                (true, Some(entries)) if !entries.is_empty() => {
                    return Err(Errno::ENOTEMPTY.into());
                }
                (true, None) => return Err(Errno::ENOTDIR.into()),
                (false, Some(_)) => return Err(Errno::EISDIR.into()),
                _ => {}
            }
            self.unlink_entry(to_dir, to_name)?;
        }
        self.unlink_entry(from_dir, from_name)?;
        self.link(to_dir, to_name, moving)?;
        Ok(())
    }

    /// Changes the owner of the object at `path` (not following it), as
    /// `chown` does: the inode stays, its ctime moves.
    pub fn chown_path(&mut self, path: &Path, uid: u32) -> Result<(), Fault> {
        let loc = self.lookup(path)?;
        let now = self.wall_now();
        let inode = self.inode_mut(loc);
        inode.uid = uid;
        inode.ctime = now;
        Ok(())
    }

    /// Replaces the object at `path` by a new, empty one of the same type.
    pub fn replace_path(&mut self, path: &Path) -> Result<Loc, Fault> {
        let (dir, name) = self.parent_of(path)?;
        let old = self.child(dir, name)?;
        if old.mnt != dir.mnt {
            return Err(Errno::EBUSY.into());
        }
        let kind = match &self.inode(old).kind {
            Kind::File { .. } => Kind::File {
                cachedir_tag: false,
                content: Vec::new(),
            },
            Kind::Dir(_) => Kind::Dir(BTreeMap::new()),
            Kind::Symlink(target) => Kind::Symlink(target.clone()),
            Kind::Socket => Kind::Socket,
            Kind::Fifo => Kind::Fifo,
            Kind::CharDevice => Kind::CharDevice,
        };
        if self.holds_mount(old) {
            return Err(Errno::EBUSY.into());
        }
        self.unlink_entry(dir, name)?;
        self.create_in(dir, name, kind)
    }

    /// Simulates a file or directory being unlinked and a new object of the
    /// same type receiving the freed inode number. This is narrower than `replace_path`:
    /// reuse is possible only after every other name and reference is gone.
    pub fn replace_path_reusing_inode(&mut self, path: &Path) -> Result<(), Fault> {
        let (dir, name) = self.parent_of(path)?;
        let old = self.child(dir, name)?;
        let new_kind = match self.inode(old).kind {
            Kind::File { .. } => Kind::File {
                cachedir_tag: false,
                content: Vec::new(),
            },
            Kind::Dir(_) => Kind::Dir(BTreeMap::new()),
            _ => {
                return Err(Fault::Model(format!(
                    "replace_reusing_inode needs a file or directory: {}",
                    path.display()
                )));
            }
        };
        if self.holds_mount(old) {
            return Err(Errno::EBUSY.into());
        }
        let aliases = self
            .fs(old.mnt)
            .inodes
            .values()
            .filter_map(Inode::entries)
            .flat_map(|entries| entries.values())
            .filter(|&&ino| ino == old.ino)
            .count();
        let old_id = self.dev_ino(old);
        let held = self
            .ofds
            .values()
            .any(|ofd| self.dev_ino(ofd.loc) == old_id)
            || self.procs.values().any(|proc| {
                proc.exe
                    .as_ref()
                    .is_some_and(|r| self.dev_ino(r.loc) == old_id)
                    || self.dev_ino(proc.cwd.loc) == old_id
                    || self.dev_ino(proc.root.loc) == old_id
                    || proc.maps.iter().any(|r| self.dev_ino(r.loc) == old_id)
            });
        if aliases != 1 || held {
            return Err(Fault::Model(format!(
                "replace_reusing_inode needs one name and no held references: {}",
                path.display()
            )));
        }
        let now = self.wall_now();
        if now == self.inode(old).btime {
            return Err(Fault::Model(format!(
                "replace_reusing_inode needs a later model clock for a distinct birth time: {}",
                path.display()
            )));
        }
        self.unlink_entry(dir, name)?;
        self.fs_mut(old.mnt)
            .inodes
            .insert(old.ino, Inode::new(new_kind, now));
        self.link(dir, name, old.ino)?;
        Ok(())
    }

    /// `unlinkat(dir, name, what)` as the kernel does it.
    pub fn unlinkat(&mut self, dir: Loc, name: &OsStr, remove_dir: bool) -> Result<Loc, Errno> {
        let entries = self.inode(dir).entries().ok_or(Errno::ENOTDIR)?;
        let ino = *entries.get(name).ok_or(Errno::ENOENT)?;
        let target = Loc { mnt: dir.mnt, ino };
        if self.mounted_on(target).is_some() {
            return Err(Errno::EBUSY);
        }
        if self.mount(dir.mnt).read_only {
            return Err(Errno::EROFS);
        }
        match (self.inode(target).entries(), remove_dir) {
            (Some(_), false) => return Err(Errno::EISDIR),
            (None, true) => return Err(Errno::ENOTDIR),
            (Some(entries), true) if !entries.is_empty() => return Err(Errno::ENOTEMPTY),
            _ => {}
        }
        self.unlink_entry(dir, name).map_err(|fault| match fault {
            Fault::Errno(e) => e,
            Fault::Model(m) => panic!("model bug: {m}"),
        })?;
        Ok(target)
    }
}

/// The normal components of an absolute path; `.` and `..` are fixture
/// mistakes.
pub fn normal_names(path: &Path) -> Result<Vec<&OsStr>, Fault> {
    if !path.is_absolute() {
        return Err(Fault::Model(format!(
            "{} is not an absolute path",
            path.display()
        )));
    }
    path.components()
        .filter(|c| *c != Component::RootDir)
        .map(|c| match c {
            Component::Normal(name) => Ok(name),
            _ => Err(Fault::Model(format!(
                "{} has '.' or '..' components",
                path.display()
            ))),
        })
        .collect()
}

/// A wall-clock time as statx reports it.
pub fn timestamp(t: jiff::Timestamp) -> Timestamp {
    let secs = t.as_second();
    let nanos = t.subsec_nanosecond();
    // jiff gives a negative fraction for times before 1970.
    if nanos < 0 {
        Timestamp {
            secs: secs - 1,
            nanos: u32::try_from(nanos + 1_000_000_000).expect("in range"),
        }
    } else {
        Timestamp {
            secs,
            nanos: u32::try_from(nanos).expect("non-negative"),
        }
    }
}
