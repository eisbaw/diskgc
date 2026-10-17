//! `ModelHost`: the [`Host`] and [`Delete`] port answered from a host model
//! (docs/testing.md, Model tests; docs/architecture.md, The Host port).
//!
//! Every port call is recorded on the trace ([`Call`]), fires the events
//! addressed to it, and is **checked**: a call the safety rules forbid makes
//! the test fail at once (a panic naming the rule), whatever the code under
//! test would do with an error. The checks:
//!
//! - `openat2` from the root opens a mount point: `O_DIRECTORY |
//!   O_NOFOLLOW` and `RESOLVE_NO_SYMLINKS`, absolute path (docs/safety.md,
//!   Path traversal, step 2);
//! - `openat2` below a directory: `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
//!   RESOLVE_NO_XDEV`, no `O_NOFOLLOW`, relative path (step 3, and the walk);
//! - `statx` takes an empty path or a single name below a directory fd
//!   (a longer path would follow symlinks in its earlier components);
//! - `unlinkat` removes a single name, and only right after a successful
//!   `statx` of that name in that directory (the previous port call; through
//!   any fd of the directory): the identity check (docs/safety.md, Deletion).
//!
//! Locks live on open file descriptions, exactly as long as some fd refers
//! to them: a [`ModelFd`] (dropping it closes it) or a process's fd. The
//! port has no unlock, so no other release exists.
//!
//! Time is virtual: [`Host::now`] starts at [`UPTIME_AT_START`] and moves
//! only by [`Host::sleep`], [`Host::flock_wait`] waiting, and `advance`
//! events. Waiting jumps straight to the next timed event or the deadline;
//! it never polls.

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use diskgc_core::host::{
    Access, Base, Delete, DevIno, Environ, Errno, Host, HostError, LinkTarget, LockMode, Locked,
    MapEntry, MonoTime, MountEntry, OpenHow, Pid, ProcError, ProcFd, ProcLink, Space, Stat,
    TryLock, Unlink, WaitLock, is_single_name,
};

use crate::load::{Action, Event, LoadError, Trigger};
use crate::spec::{HostModel, Op, ProcAccess, When};
use crate::state::{Fault, Kind, Loc, Ref, State, WalkRules};

/// What [`Host::now`] reads when a run starts: a running machine's uptime,
/// so code that subtracts from `now()` is not hidden by a zero origin.
pub const UPTIME_AT_START: Duration = Duration::from_secs(3600);

/// One port call on the trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// Position in the run, from 1.
    pub index: u64,
    pub op: Op,
    /// What the call acted on: an fd's path joined with the call's relative
    /// path, or `/proc/<pid>/...`.
    pub path: Option<PathBuf>,
    /// The `openat2` arguments.
    pub how: Option<OpenHow>,
    /// The object opened, statted, locked or unlinked (on success).
    pub object: Option<DevIno>,
    /// The directory entry a `statx` or `unlinkat` of a name below a
    /// directory fd names: (the directory, the name).
    pub entry: Option<(DevIno, OsString)>,
    /// `ok`, an errno, `busy`, `deadline passed`, `gone`, `denied` or
    /// `failed`.
    pub result: String,
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{} {}", self.index, self.op)?;
        if let Some(path) = &self.path {
            write!(f, " {}", path.display())?;
        }
        write!(f, " -> {}", self.result)
    }
}

/// Who holds a lock (for test assertions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Holder {
    /// diskgc, through a [`ModelFd`].
    Own,
    /// A modelled process.
    Process(u32),
}

struct Inner {
    state: State,
    /// Events that have not fired, in declaration order.
    pending: Vec<Event>,
    trace: Vec<Call>,
}

/// A `Host` backed by a host model. Not `Send`: a run is one thread.
pub struct ModelHost {
    inner: Rc<RefCell<Inner>>,
}

/// An fd of [`ModelHost`]; dropping it closes it (releasing its lock).
pub struct ModelFd {
    ofd: u64,
    inner: Rc<RefCell<Inner>>,
}

impl fmt::Debug for ModelFd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ModelFd({})", self.ofd)
    }
}

impl Drop for ModelFd {
    fn drop(&mut self) {
        match self.inner.try_borrow_mut() {
            Ok(mut inner) => inner.state.close_own(self.ofd),
            // Unwinding from a failed check: the run is over anyway.
            Err(_) if std::thread::panicking() => {}
            Err(_) => panic!("ModelHost bug: an fd was dropped while the model was in use"),
        }
    }
}

/// Fails the test: the code under test called the port in a way the safety
/// rules forbid.
fn violation(message: impl fmt::Display) -> ! {
    panic!("ModelHost call check failed: {message}")
}

/// A fixture or test mistake (not a property of the code under test).
fn model_error(message: impl fmt::Display) -> ! {
    panic!("host model error: {message}")
}

fn errno_text(e: Errno) -> String {
    e.to_string()
}

/// A path with `.` components dropped (`..` is kept, so checks see it).
fn tidy(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| *c != Component::CurDir)
        .collect()
}

impl ModelHost {
    /// Loads a model.
    pub fn new(model: &HostModel) -> Result<Self, LoadError> {
        let (state, pending) = crate::load::load(model)?;
        Ok(Self {
            inner: Rc::new(RefCell::new(Inner {
                state,
                pending,
                trace: Vec::new(),
            })),
        })
    }

    /// Parses and loads a YAML model.
    pub fn from_yaml(text: &str) -> Result<Self, LoadError> {
        Self::new(&crate::load::parse_yaml(text)?)
    }

    // ---- Inspection for tests (not port calls: no trace, no events) ----

    /// Every call so far.
    pub fn trace(&self) -> Vec<Call> {
        self.inner.borrow().trace.clone()
    }

    /// Indices (in the model's `events`) of events that have not fired.
    pub fn unfired_events(&self) -> Vec<usize> {
        self.inner
            .borrow()
            .pending
            .iter()
            .map(|e| e.index)
            .collect()
    }

    /// The stat of an absolute path, if it exists (through mounts, not
    /// following symlinks).
    pub fn stat_path(&self, path: &Path) -> Option<Stat> {
        let inner = self.inner.borrow();
        let loc = inner.state.lookup(path).ok()?;
        Some(inner.state.stat(loc))
    }

    /// Space on the filesystem holding an absolute path.
    pub fn space_at(&self, path: &Path) -> Space {
        let inner = self.inner.borrow();
        let loc = inner
            .state
            .lookup(path)
            .unwrap_or_else(|f| model_error(format!("space_at {}: {f:?}", path.display())));
        inner.state.space(loc.mnt)
    }

    /// The locks held on the object at `path`, and by whom.
    pub fn locks_on(&self, path: &Path) -> Vec<(Holder, LockMode)> {
        let inner = self.inner.borrow();
        let state = &inner.state;
        let Ok(loc) = state.lookup(path) else {
            return Vec::new();
        };
        let object = state.dev_ino(loc);
        let mut held: Vec<(Holder, LockMode)> = state
            .ofds
            .iter()
            .filter(|(_, ofd)| state.dev_ino(ofd.loc) == object)
            .filter_map(|(id, ofd)| Some((*id, ofd.lock?)))
            .flat_map(|(id, mode)| {
                let own = state.own_ofds.contains(&id).then_some(Holder::Own);
                let procs = state
                    .procs
                    .iter()
                    .filter(move |(_, p)| p.fds.values().any(|o| *o == id))
                    .map(|(pid, _)| Holder::Process(*pid));
                own.into_iter().chain(procs).map(move |h| (h, mode))
            })
            .collect();
        held.sort_by_key(|(h, _)| *h);
        held
    }

    /// Virtual time since the start of the run.
    pub fn elapsed(&self) -> Duration {
        self.inner.borrow().state.elapsed
    }

    // ---- Calls ----

    fn fd_ofd(&self, fd: &ModelFd) -> u64 {
        if !Rc::ptr_eq(&fd.inner, &self.inner) {
            violation(format!("{fd:?} belongs to another ModelHost"));
        }
        fd.ofd
    }

    /// The object an fd of this host refers to.
    fn fd_object(&self, ofd: u64) -> DevIno {
        let inner = self.inner.borrow();
        inner.state.dev_ino(inner.loc_of(ofd))
    }

    fn base_path(&self, base: Base<'_, ModelFd>, path: &Path) -> PathBuf {
        match base {
            Base::Root => tidy(path),
            Base::Dir(fd) => {
                let ofd = self.fd_ofd(fd);
                let inner = self.inner.borrow();
                tidy(&inner.state.ofds[&ofd].path.join(path))
            }
        }
    }

    /// Runs one port call: due timed events, `before` events, the call,
    /// the trace entry, `after` events.
    fn call<R>(
        &self,
        op: Op,
        path: Option<PathBuf>,
        how: Option<OpenHow>,
        run: impl FnOnce(&mut Inner) -> (R, Option<DevIno>, String),
    ) -> R {
        self.call_on(op, path, how, None, run)
    }

    /// [`Self::call`] for a call that names a directory entry.
    fn call_on<R>(
        &self,
        op: Op,
        path: Option<PathBuf>,
        how: Option<OpenHow>,
        entry: Option<(DevIno, OsString)>,
        run: impl FnOnce(&mut Inner) -> (R, Option<DevIno>, String),
    ) -> R {
        let mut inner = self.inner.borrow_mut();
        inner.fire_due();
        let index = inner.trace.len() as u64 + 1;
        inner.fire_calls(index, op, path.as_deref(), When::Before);
        let (result, object, text) = run(&mut inner);
        inner.trace.push(Call {
            index,
            op,
            path: path.clone(),
            how,
            object,
            entry,
            result: text,
        });
        inner.fire_calls(index, op, path.as_deref(), When::After);
        result
    }

    fn wrap(&self, ofd: u64) -> ModelFd {
        ModelFd {
            ofd,
            inner: Rc::clone(&self.inner),
        }
    }

    /// A `/proc/<pid>/<what>` reader. An empty answer from a readable
    /// process is `Failed`, as `LinuxHost` reports it: there an empty read
    /// of a live task cannot be told apart from one bound to a dead leader,
    /// so empty is never the answer (diskgc-linux tasks.rs).
    fn proc_call<T>(
        &self,
        op: Op,
        pid: Pid,
        what: &str,
        read: impl FnOnce(&State, &crate::state::Proc) -> T,
        is_empty: impl FnOnce(&T) -> bool,
    ) -> Result<T, ProcError> {
        let path = PathBuf::from(format!("/proc/{pid}/{what}"));
        let shown = path.display().to_string();
        self.call(op, Some(path), None, |inner| {
            let Some(proc) = inner.state.procs.get(&pid.0) else {
                return (Err(ProcError::Gone), None, "gone".to_owned());
            };
            match proc.access {
                ProcAccess::Readable => {
                    let answer = read(&inner.state, proc);
                    if is_empty(&answer) {
                        let message = format!(
                            "the process's {what} is empty, which LinuxHost reports as Failed"
                        );
                        let error = HostError::invalid("read", &shown, message);
                        (
                            Err(ProcError::Failed(error)),
                            None,
                            "failed (empty)".to_owned(),
                        )
                    } else {
                        (Ok(answer), None, "ok".to_owned())
                    }
                }
                ProcAccess::Denied => (
                    Err(ProcError::Denied(HostError::errno(
                        "read",
                        &shown,
                        Errno::EACCES,
                    ))),
                    None,
                    "denied".to_owned(),
                ),
                ProcAccess::Failed => (
                    Err(ProcError::Failed(HostError::invalid(
                        "read",
                        &shown,
                        "the model says this process cannot be inspected",
                    ))),
                    None,
                    "failed".to_owned(),
                ),
            }
        })
    }
}

impl Inner {
    /// Fires every timed event that is due, earliest first.
    fn fire_due(&mut self) {
        while let Some(pos) = self.next_timed(self.state.elapsed) {
            self.fire(pos);
        }
    }

    /// The pending timed event due soonest at or before `by`.
    fn next_timed(&self, by: Duration) -> Option<usize> {
        self.pending
            .iter()
            .enumerate()
            .filter_map(|(pos, e)| match e.trigger {
                Trigger::Elapsed(t) if t <= by => Some((t, pos)),
                _ => None,
            })
            .min()
            .map(|(_, pos)| pos)
    }

    /// The time of the earliest pending timed event.
    fn next_timed_at(&self) -> Option<Duration> {
        self.pending
            .iter()
            .filter_map(|e| match e.trigger {
                Trigger::Elapsed(t) => Some(t),
                _ => None,
            })
            .min()
    }

    /// Lets time pass until `until`, firing timed events at their times.
    fn advance_to(&mut self, until: Duration) {
        while let Some(pos) = self.next_timed(until) {
            if let Trigger::Elapsed(t) = self.pending[pos].trigger {
                self.state.elapsed = self.state.elapsed.max(t);
            }
            self.fire(pos);
        }
        self.state.elapsed = self.state.elapsed.max(until);
    }

    /// Fires the events addressed to call `index` (`op` on `path`).
    fn fire_calls(&mut self, index: u64, op: Op, path: Option<&Path>, when: When) {
        // Calls before this one that match a trigger's (op, path).
        let earlier = |trace: &[Call], want: Option<&PathBuf>| {
            trace
                .iter()
                .filter(|c| c.index < index && c.op == op)
                .filter(|c| want.is_none_or(|p| c.path.as_ref() == Some(p)))
                .count()
        };
        let due: Vec<usize> = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, e)| match &e.trigger {
                Trigger::Index { index: i, when: w } => *i == index && *w == when,
                Trigger::Call {
                    op: o,
                    path: p,
                    nth,
                    when: w,
                } => {
                    *o == op
                        && *w == when
                        && p.as_deref().is_none_or(|p| Some(p) == path)
                        && earlier(&self.trace, p.as_ref()) + 1 == *nth as usize
                }
                Trigger::Elapsed(_) => false,
            })
            .map(|(pos, _)| pos)
            .collect();
        // Remove from the back so positions stay valid; fire in order.
        let mut events: Vec<Event> = due
            .into_iter()
            .rev()
            .map(|pos| self.pending.remove(pos))
            .collect();
        events.reverse();
        for event in events {
            self.apply(&event);
        }
    }

    fn fire(&mut self, pos: usize) {
        let event = self.pending.remove(pos);
        self.apply(&event);
    }

    fn apply(&mut self, event: &Event) {
        let state = &mut self.state;
        let outcome = match &event.action {
            Action::Create(path) => state
                .create_path(
                    path,
                    Kind::File {
                        cachedir_tag: false,
                    },
                )
                .map(drop),
            Action::Mkdir(path) => state
                .create_path(path, Kind::Dir(Default::default()))
                .map(drop),
            Action::Remove(path) => state.remove_path(path),
            Action::Rename { from, to } => state.rename_path(from, to),
            Action::Replace(path) => state.replace_path(path).map(drop),
            Action::Open { pid, path } => state.proc_open(*pid, path, None).map(drop),
            Action::Close { pid, path } => state.proc_close(*pid, path),
            Action::Exit(pid) => state.proc_exit(*pid),
            Action::Advance(d) => {
                state.elapsed += *d;
                Ok(())
            }
        };
        if let Err(fault) = outcome {
            model_error(format!(
                "events[{}] ({:?}) cannot be applied: {fault:?}",
                event.index, event.action
            ));
        }
        // An advance may make timed events due.
        if matches!(event.action, Action::Advance(_)) {
            self.fire_due();
        }
    }

    fn loc_of(&self, ofd: u64) -> Loc {
        self.state.ofds[&ofd].loc
    }

    /// Takes `mode` on `ofd` if nothing conflicts.
    fn try_lock(&mut self, ofd: u64, mode: LockMode) -> Result<bool, Errno> {
        let current = &self.state.ofds[&ofd];
        if current.access == Access::Path {
            // flock(2) on an O_PATH fd.
            return Err(Errno::EBADF);
        }
        assert!(
            current.lock.is_none(),
            "ModelHost bug: the port cannot lock a locked fd"
        );
        if !self.state.lock_free(ofd, mode) {
            return Ok(false);
        }
        self.state.ofds.get_mut(&ofd).expect("open").lock = Some(mode);
        Ok(true)
    }
}

/// What a `/proc` magic link to `r` shows: its path (` (deleted)` once no
/// directory names it) and its stat.
fn link_target(state: &State, r: &Ref) -> LinkTarget {
    let mut text = r.path.clone().into_os_string();
    if !state.is_linked(r.loc) {
        text.push(" (deleted)");
    }
    LinkTarget {
        text,
        stat: state.stat(r.loc),
    }
}

/// Checks `openat2`'s arguments against docs/safety.md, Path traversal.
fn check_openat2(from_root: bool, path: &Path, how: OpenHow) {
    let shown = path.display();
    let r = how.resolve;
    if from_root {
        if !path.is_absolute() {
            violation(format!("openat2 from the root with relative path {shown}"));
        }
        let missing: Vec<&str> = [
            (r.no_symlinks, "RESOLVE_NO_SYMLINKS"),
            (how.nofollow, "O_NOFOLLOW"),
            (how.directory, "O_DIRECTORY"),
        ]
        .into_iter()
        .filter(|(set, _)| !set)
        .map(|(_, name)| name)
        .collect();
        if !missing.is_empty() {
            violation(format!(
                "openat2 of mount point {shown} without {} (docs/safety.md, Path traversal, \
                 step 2)",
                missing.join(", ")
            ));
        }
    } else {
        if path.is_absolute() {
            violation(format!(
                "openat2 below a directory with absolute path {shown}"
            ));
        }
        let missing: Vec<&str> = [
            (r.beneath, "RESOLVE_BENEATH"),
            (r.no_symlinks, "RESOLVE_NO_SYMLINKS"),
            (r.no_xdev, "RESOLVE_NO_XDEV"),
        ]
        .into_iter()
        .filter(|(set, _)| !set)
        .map(|(_, name)| name)
        .collect();
        if !missing.is_empty() {
            violation(format!(
                "openat2 of {shown} below a directory without {} (docs/safety.md, Path \
                 traversal, step 3)",
                missing.join(", ")
            ));
        }
        if how.nofollow {
            violation(format!(
                "openat2 of {shown} below a directory with O_NOFOLLOW, which opens a trailing \
                 symlink instead of refusing it"
            ));
        }
    }
}

impl Host for ModelHost {
    type Fd = ModelFd;

    fn mountinfo(&self) -> Result<Vec<MountEntry>, HostError> {
        self.call(Op::Mountinfo, None, None, |inner| {
            (Ok(inner.state.mountinfo()), None, "ok".to_owned())
        })
    }

    fn openat2(
        &self,
        base: Base<'_, ModelFd>,
        path: &Path,
        how: OpenHow,
    ) -> Result<ModelFd, HostError> {
        let start_ofd = match base {
            Base::Root => None,
            Base::Dir(fd) => Some(self.fd_ofd(fd)),
        };
        check_openat2(start_ofd.is_none(), path, how);
        let full = self.base_path(base, path);
        let shown = full.display().to_string();
        let opened = self.call(Op::Openat2, Some(full.clone()), Some(how), |inner| {
            let state = &inner.state;
            let start = match start_ofd {
                Some(ofd) => inner.loc_of(ofd),
                None => state
                    .lookup(Path::new("/"))
                    .unwrap_or_else(|f| model_error(format!("no root: {f:?}"))),
            };
            let rules = WalkRules {
                beneath: how.resolve.beneath,
                no_symlinks: how.resolve.no_symlinks,
                no_xdev: how.resolve.no_xdev,
                open_trailing_symlink: how.nofollow && how.access == Access::Path,
            };
            let found = if state.inode(start).entries().is_none() {
                Err(Fault::Errno(Errno::ENOTDIR))
            } else if path.as_os_str().is_empty() {
                Err(Fault::Errno(Errno::ENOENT))
            } else {
                state.walk(start, path, rules)
            };
            let found = found.and_then(|loc| match (&state.inode(loc).kind, how.access) {
                (Kind::Dir(_), _) => Ok(loc),
                _ if how.directory => Err(Fault::Errno(Errno::ENOTDIR)),
                (Kind::Socket, Access::Read) => Err(Fault::Errno(Errno::ENXIO)),
                (Kind::Fifo, Access::Read) => Err(Fault::Model(
                    "opening a FIFO for reading blocks until a writer opens it; not modelled"
                        .to_owned(),
                )),
                _ => Ok(loc),
            });
            match found {
                Ok(loc) => {
                    let object = state.dev_ino(loc);
                    let ofd = inner.state.open_ofd(loc, full.clone(), how.access);
                    inner.state.own_ofds.insert(ofd);
                    (Ok(ofd), Some(object), "ok".to_owned())
                }
                Err(Fault::Errno(e)) => (
                    Err(HostError::errno("openat2", &shown, e)),
                    None,
                    errno_text(e),
                ),
                Err(Fault::Model(m)) => model_error(format!("openat2 {shown}: {m}")),
            }
        });
        opened.map(|ofd| self.wrap(ofd))
    }

    fn statx(&self, base: Base<'_, ModelFd>, path: &Path) -> Result<Stat, HostError> {
        let Base::Dir(fd) = base else {
            violation(format!(
                "statx of {} from the root follows symlinks on the way; reach it with \
                 resolve() and statx the fd",
                path.display()
            ));
        };
        let ofd = self.fd_ofd(fd);
        let name = path.as_os_str();
        if !name.is_empty() && !is_single_name(name) {
            violation(format!(
                "statx of {} below a directory: only an empty path or a single name is safe \
                 (earlier components would follow symlinks)",
                path.display()
            ));
        }
        let full = self.base_path(base, path);
        let shown = full.display().to_string();
        let entry = (!name.is_empty()).then(|| (self.fd_object(ofd), name.to_owned()));
        self.call_on(Op::Statx, Some(full), None, entry, |inner| {
            let dir = inner.loc_of(ofd);
            let found = if name.is_empty() {
                Ok(dir)
            } else {
                inner.state.child(dir, name)
            };
            match found {
                Ok(loc) => {
                    let stat = inner.state.stat(loc);
                    (Ok(stat), Some(inner.state.dev_ino(loc)), "ok".to_owned())
                }
                Err(e) => (
                    Err(HostError::errno("statx", &shown, e)),
                    None,
                    errno_text(e),
                ),
            }
        })
    }

    fn statvfs(&self, fd: &ModelFd) -> Result<Space, HostError> {
        let ofd = self.fd_ofd(fd);
        let path = self.base_path(Base::Dir(fd), Path::new(""));
        self.call(Op::Statvfs, Some(path), None, |inner| {
            let loc = inner.loc_of(ofd);
            (Ok(inner.state.space(loc.mnt)), None, "ok".to_owned())
        })
    }

    fn flock_try(&self, fd: ModelFd, mode: LockMode) -> Result<TryLock<ModelFd>, HostError> {
        let ofd = self.fd_ofd(&fd);
        let path = self.base_path(Base::Dir(&fd), Path::new(""));
        let shown = path.display().to_string();
        let got = self.call(Op::Flock, Some(path), None, |inner| {
            let object = Some(inner.state.dev_ino(inner.loc_of(ofd)));
            match inner.try_lock(ofd, mode) {
                Ok(true) => (Ok(true), object, "acquired".to_owned()),
                Ok(false) => (Ok(false), object, "busy".to_owned()),
                Err(e) => (Err(e), None, errno_text(e)),
            }
        });
        match got {
            Ok(true) => Ok(TryLock::Acquired(Locked::granted(fd, mode))),
            Ok(false) => Ok(TryLock::Busy(fd)),
            // `fd` is dropped (closed) here, as the port promises.
            Err(e) => Err(HostError::errno("flock", shown, e)),
        }
    }

    fn flock_wait(
        &self,
        fd: ModelFd,
        mode: LockMode,
        deadline: Option<MonoTime>,
    ) -> Result<WaitLock<ModelFd>, HostError> {
        let ofd = self.fd_ofd(&fd);
        let path = self.base_path(Base::Dir(&fd), Path::new(""));
        let shown = path.display().to_string();
        let until = deadline.map(|d| d.0.saturating_sub(UPTIME_AT_START));
        let got = self.call(Op::Flock, Some(path), None, |inner| {
            let object = Some(inner.state.dev_ino(inner.loc_of(ofd)));
            loop {
                match inner.try_lock(ofd, mode) {
                    Err(e) => return (Err(e), None, errno_text(e)),
                    Ok(true) => return (Ok(true), object, "acquired".to_owned()),
                    Ok(false) => {}
                }
                let next = inner.next_timed_at();
                match (until, next) {
                    (Some(until), Some(t)) if t <= until => inner.advance_to(t),
                    (Some(until), _) => {
                        inner.advance_to(until);
                        return (Ok(false), object, "deadline passed".to_owned());
                    }
                    (None, Some(t)) => inner.advance_to(t),
                    (None, None) => model_error(format!(
                        "flock_wait on {shown} without a deadline would block forever: no \
                         timed event is left to release the lock"
                    )),
                }
            }
        });
        match got {
            Ok(true) => Ok(WaitLock::Acquired(Locked::granted(fd, mode))),
            Ok(false) => Ok(WaitLock::DeadlinePassed(fd)),
            Err(e) => Err(HostError::errno("flock", shown, e)),
        }
    }

    fn now(&self) -> MonoTime {
        self.call(Op::Now, None, None, |inner| {
            let now = MonoTime(UPTIME_AT_START + inner.state.elapsed);
            (now, None, format!("{:?}", now.0))
        })
    }

    fn sleep(&self, duration: Duration) {
        self.call(Op::Sleep, None, None, |inner| {
            let until = inner.state.elapsed + duration;
            inner.advance_to(until);
            ((), None, format!("{duration:?}"))
        })
    }

    fn kernel_release(&self) -> Result<String, HostError> {
        self.call(Op::KernelRelease, None, None, |inner| {
            (Ok(inner.state.kernel.clone()), None, "ok".to_owned())
        })
    }

    fn pids(&self) -> Result<Vec<Pid>, HostError> {
        self.call(Op::Pids, None, None, |inner| {
            let pids = inner.state.procs.keys().map(|p| Pid(*p)).collect();
            (Ok(pids), None, "ok".to_owned())
        })
    }

    fn proc_fds(&self, pid: Pid) -> Result<Vec<ProcFd>, ProcError> {
        self.proc_call(
            Op::ProcFds,
            pid,
            "fd",
            |state, proc| {
                proc.fds
                    .iter()
                    .map(|(fd, ofd)| {
                        let ofd = &state.ofds[ofd];
                        let r = Ref {
                            loc: ofd.loc,
                            path: ofd.path.clone(),
                        };
                        ProcFd {
                            fd: *fd,
                            target: link_target(state, &r),
                        }
                    })
                    .collect()
            },
            Vec::is_empty,
        )
    }

    fn proc_links(&self, pid: Pid, which: ProcLink) -> Result<Vec<LinkTarget>, ProcError> {
        self.proc_call(
            Op::ProcLinks,
            pid,
            which.name(),
            |state, proc| {
                let r = match which {
                    ProcLink::Cwd => Some(&proc.cwd),
                    ProcLink::Root => Some(&proc.root),
                    ProcLink::Exe => proc.exe.as_ref(),
                };
                r.iter().map(|r| link_target(state, r)).collect()
            },
            Vec::is_empty,
        )
    }

    fn proc_maps(&self, pid: Pid) -> Result<Vec<MapEntry>, ProcError> {
        self.proc_call(
            Op::ProcMaps,
            pid,
            "maps",
            |state, proc| {
                proc.maps
                    .iter()
                    .map(|r| MapEntry {
                        dev: state.mount(r.loc.mnt).dev,
                        ino: r.loc.ino,
                        path: Some(link_target(state, r).text),
                    })
                    .collect()
            },
            Vec::is_empty,
        )
    }

    fn proc_environ(&self, pid: Pid) -> Result<Environ, ProcError> {
        self.proc_call(
            Op::ProcEnviron,
            pid,
            "environ",
            |_, proc| proc.environ.clone(),
            |env| env.vars.is_empty() && env.without_equals.is_empty(),
        )
    }
}

impl Delete for ModelHost {
    fn unlinkat(&self, dir: &ModelFd, name: &OsStr, what: Unlink) -> Result<(), HostError> {
        let ofd = self.fd_ofd(dir);
        if !is_single_name(name) {
            violation(format!(
                "unlinkat of {name:?}: only a single name can be unlinked safely"
            ));
        }
        let full = self.base_path(Base::Dir(dir), Path::new(name));
        let shown = full.display().to_string();
        let entry = (self.fd_object(ofd), name.to_owned());
        self.call_on(
            Op::Unlinkat,
            Some(full),
            None,
            Some(entry.clone()),
            |inner| {
                // The identity check is the call right before: a successful
                // statx of this name in this directory (through any fd of it).
                let checked = inner.trace.last().is_some_and(|last| {
                    last.op == Op::Statx
                        && last.object.is_some()
                        && last.entry.as_ref() == Some(&entry)
                });
                if !checked {
                    violation(format!(
                        "unlinkat of {shown} without an identity check right before it (a statx of \
                     that name in that directory as the previous call; docs/safety.md, Deletion)"
                    ));
                }
                let dir_loc = inner.loc_of(ofd);
                match inner.state.unlinkat(dir_loc, name, what == Unlink::Dir) {
                    Ok(loc) => (Ok(()), Some(inner.state.dev_ino(loc)), "ok".to_owned()),
                    Err(e) => (
                        Err(HostError::errno("unlinkat", &shown, e)),
                        None,
                        errno_text(e),
                    ),
                }
            },
        )
    }
}
