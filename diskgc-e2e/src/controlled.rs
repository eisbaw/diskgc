//! The test-only `Host` wrapper with controlled pause and kill points
//! (docs/testing.md, Controlled pause and kill points).
//!
//! [`Controlled`] wraps any [`Host`] (and [`Delete`]) and counts every port
//! call, across all methods, with one counter starting at 1. At a call the
//! harness chose ([`Point`]: the k-th call, or the n-th call of one
//! operation), it acts *before* performing the call:
//!
//! - **pause:** prints a [`ControlLine::Pause`] on stdout and blocks reading
//!   one line from stdin; the harness checks the process is blocked there,
//!   changes the fixture, and writes a line to resume it. EOF on stdin while
//!   paused ends the process with [`exit::FAIL`]: the harness went away, and
//!   carrying on would act on a fixture nobody is watching.
//! - **kill:** prints a [`ControlLine::Kill`] and sends itself `SIGKILL`, so
//!   call k and everything after it never happen.
//!
//! Why a stdin handshake and not `SIGSTOP` (strace makes a self-stopped
//! process indistinguishable from a passing ptrace stop) is in
//! docs/testing.md, Controlled pause and kill points.
//!
//! The wrapper only forwards: it adds no operation, mints no sandbox-gate
//! permit and makes no syscall of its own on the fixture, so deletions still
//! go through the wrapped host's `Delete` methods (for `LinuxHost`, through
//! the pre-release sandbox gate). It lives in diskgc-e2e, which the release
//! package never links (checked by `diskgc-e2e-release-check`).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use diskgc_core::host::{
    Base, BoundSocket, Delete, DirListing, Environ, Host, HostError, LinkTarget, LockMode,
    MapEntry, MonoTime, MountEntry, OpenHow, Pid, ProcError, ProcFd, ProcLink, ProcStatus,
    ProcView, Space, Stat, TryLock, Unlink, WaitLock,
};

use crate::exit;

/// Starts every line the wrapper (and the driver's final count) prints, so
/// the harness can tell control lines from results. Also a needle of the
/// release check: the shipped binary must not contain it.
pub const CONTROL_PREFIX: &str = "diskgc-e2e-control:";

/// The port operations, by method name: what [`Point::Nth`] may name.
pub const OPS: &[&str] = &[
    "mountinfo",
    "openat2",
    "statx",
    "read_dir",
    "statvfs",
    "flock_try",
    "flock_wait",
    "now",
    "sleep",
    "kernel_release",
    "pids",
    "proc_fds",
    "proc_links",
    "proc_maps",
    "proc_environ",
    "proc_views",
    "own_pid",
    "proc_status",
    "bound_unix_sockets",
    "unlinkat",
    "fchmod",
];

/// A position on the port-call trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// The k-th port call of the run, from 1.
    Call(u64),
    /// The n-th call (from 1) of the operation `op` (one of [`OPS`]).
    Nth { op: &'static str, nth: u64 },
}

impl Point {
    /// Parses `K` or `OP:N`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let number = |n: &str| -> Result<u64, String> {
            n.parse()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("{text:?}: positions count from 1, got {n:?}"))
        };
        match text.split_once(':') {
            None => Ok(Self::Call(number(text)?)),
            Some((op, nth)) => {
                let op = OPS.iter().find(|known| **known == op).ok_or_else(|| {
                    format!("{text:?}: {op:?} is not a port operation (one of {OPS:?})")
                })?;
                Ok(Self::Nth {
                    op,
                    nth: number(nth)?,
                })
            }
        }
    }

    fn matches(self, call: u64, op: &str, nth: u64) -> bool {
        match self {
            Self::Call(k) => k == call,
            Self::Nth { op: o, nth: n } => o == op && n == nth,
        }
    }
}

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Call(k) => write!(f, "{k}"),
            Self::Nth { op, nth } => write!(f, "{op}:{nth}"),
        }
    }
}

/// What happens at a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Pause,
    Kill,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Kill => "kill",
        }
    }
}

/// The points of one run, in the order given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub points: Vec<(Point, Action)>,
}

impl Plan {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn at(mut self, point: Point, action: Action) -> Self {
        self.points.push((point, action));
        self
    }
}

/// A planned point the run reached, handed to the wrapper's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// The call's position on the trace, from 1.
    pub call: u64,
    pub op: &'static str,
    /// Its position among the calls of `op`, from 1.
    pub nth: u64,
    pub action: Action,
}

/// A line the wrapper or the driver prints after [`CONTROL_PREFIX`]. The
/// one place that writes and parses the format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlLine {
    /// Blocked before call `call`, waiting for a line on stdin.
    Pause {
        call: u64,
        op: String,
        nth: u64,
        pid: u32,
    },
    /// Resumed; call `call` runs next.
    Resume { call: u64 },
    /// About to `SIGKILL` itself before call `call`.
    Kill {
        call: u64,
        op: String,
        nth: u64,
        pid: u32,
    },
    /// The operation finished after `calls` port calls.
    Done { calls: u64 },
}

impl fmt::Display for ControlLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{CONTROL_PREFIX} ")?;
        match self {
            Self::Pause { call, op, nth, pid } => {
                write!(f, "pause call={call} op={op} nth={nth} pid={pid}")
            }
            Self::Resume { call } => write!(f, "resume call={call}"),
            Self::Kill { call, op, nth, pid } => {
                write!(f, "kill call={call} op={op} nth={nth} pid={pid}")
            }
            Self::Done { calls } => write!(f, "done calls={calls}"),
        }
    }
}

impl ControlLine {
    /// Parses a stdout line; `None` if it is not a control line.
    pub fn parse(line: &str) -> Result<Option<Self>, String> {
        let Some(rest) = line.strip_prefix(CONTROL_PREFIX) else {
            return Ok(None);
        };
        let mut words = rest.split_whitespace();
        let kind = words.next().unwrap_or("");
        let fields: BTreeMap<&str, &str> = words.filter_map(|w| w.split_once('=')).collect();
        let bad = || format!("malformed control line {line:?}");
        let number = |key: &str| -> Result<u64, String> {
            fields.get(key).and_then(|v| v.parse().ok()).ok_or_else(bad)
        };
        let pid = || -> Result<u32, String> {
            fields
                .get("pid")
                .and_then(|v| v.parse().ok())
                .ok_or_else(bad)
        };
        let op = || -> Result<String, String> {
            fields.get("op").map(|v| (*v).to_owned()).ok_or_else(bad)
        };
        Ok(Some(match kind {
            "pause" => Self::Pause {
                call: number("call")?,
                op: op()?,
                nth: number("nth")?,
                pid: pid()?,
            },
            "resume" => Self::Resume {
                call: number("call")?,
            },
            "kill" => Self::Kill {
                call: number("call")?,
                op: op()?,
                nth: number("nth")?,
                pid: pid()?,
            },
            "done" => Self::Done {
                calls: number("calls")?,
            },
            _ => return Err(bad()),
        }))
    }
}

/// What a reached point does; [`stdio_control`] in the driver, a recorder in
/// unit tests.
pub type OnHit = Box<dyn Fn(&Hit)>;

/// A [`Host`] that counts every call to `H` and acts at the planned points.
pub struct Controlled<H> {
    host: H,
    plan: Vec<(Point, Action, Cell<bool>)>,
    calls: Cell<u64>,
    per_op: RefCell<BTreeMap<&'static str, u64>>,
    on_hit: OnHit,
}

impl<H> Controlled<H> {
    pub fn new(host: H, plan: Plan, on_hit: OnHit) -> Self {
        Self {
            host,
            plan: plan
                .points
                .into_iter()
                .map(|(p, a)| (p, a, Cell::new(false)))
                .collect(),
            calls: Cell::new(0),
            per_op: RefCell::new(BTreeMap::new()),
            on_hit,
        }
    }

    /// Whether a planned point names a `/proc` reader, or a call by number
    /// (which could fall on one): then the readers must be called one by
    /// one through this wrapper.
    fn targets_readers(&self) -> bool {
        const READERS: [&str; 4] = ["proc_fds", "proc_links", "proc_maps", "proc_environ"];
        self.plan.iter().any(|(point, _, _)| match point {
            Point::Call(_) => true,
            Point::Nth { op, .. } => READERS.contains(op),
        })
    }

    /// Port calls made so far.
    pub fn calls(&self) -> u64 {
        self.calls.get()
    }

    /// Calls made so far per operation (operations not called are absent).
    pub fn calls_per_op(&self) -> BTreeMap<&'static str, u64> {
        self.per_op.borrow().clone()
    }

    /// The planned points the run never reached, as `action at point`.
    pub fn unreached(&self) -> Vec<String> {
        self.plan
            .iter()
            .filter(|(_, _, hit)| !hit.get())
            .map(|(point, action, _)| format!("{} at {point}", action.name()))
            .collect()
    }

    /// Counts the call of `op` about to be made and runs the actions planned
    /// for it, before the call.
    fn before(&self, op: &'static str) {
        debug_assert!(OPS.contains(&op), "{op} missing from OPS");
        let call = self.calls.get() + 1;
        self.calls.set(call);
        let nth = {
            let mut per_op = self.per_op.borrow_mut();
            let n = per_op.entry(op).or_insert(0);
            *n += 1;
            *n
        };
        for (point, action, hit) in &self.plan {
            if point.matches(call, op, nth) {
                hit.set(true);
                (self.on_hit)(&Hit {
                    call,
                    op,
                    nth,
                    action: *action,
                });
            }
        }
    }
}

/// The driver's action at a point: the stdin handshake for a pause,
/// `SIGKILL` for a kill (see the module docs).
pub fn stdio_control(hit: &Hit) {
    let pid = std::process::id();
    let mut stdout = std::io::stdout().lock();
    let say = |out: &mut std::io::StdoutLock<'_>, line: ControlLine| {
        // Flushed at once: the harness waits for it, and a kill would lose
        // anything still buffered.
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            eprintln!("diskgc-e2e-driver: cannot write {line} to stdout; stopping");
            std::process::exit(exit::FAIL);
        }
    };
    match hit.action {
        Action::Pause => {
            say(
                &mut stdout,
                ControlLine::Pause {
                    call: hit.call,
                    op: hit.op.to_owned(),
                    nth: hit.nth,
                    pid,
                },
            );
            let mut line = String::new();
            match std::io::stdin().lock().read_line(&mut line) {
                Ok(n) if n > 0 => say(&mut stdout, ControlLine::Resume { call: hit.call }),
                Ok(_) => {
                    eprintln!(
                        "diskgc-e2e-driver: stdin closed while paused before call {} ({}); \
                         stopping instead of carrying on unobserved",
                        hit.call, hit.op
                    );
                    std::process::exit(exit::FAIL);
                }
                Err(e) => {
                    eprintln!(
                        "diskgc-e2e-driver: reading stdin while paused before call {}: {e}",
                        hit.call
                    );
                    std::process::exit(exit::FAIL);
                }
            }
        }
        Action::Kill => {
            say(
                &mut stdout,
                ControlLine::Kill {
                    call: hit.call,
                    op: hit.op.to_owned(),
                    nth: hit.nth,
                    pid,
                },
            );
            drop(stdout);
            let sent = rustix::process::kill_process(
                rustix::process::getpid(),
                rustix::process::Signal::KILL,
            );
            // SIGKILL to oneself is delivered before kill() returns to user
            // space; reaching this line means it was not sent.
            eprintln!(
                "diskgc-e2e-driver: SIGKILL before call {} was not delivered ({sent:?})",
                hit.call
            );
            std::process::abort();
        }
    }
}

impl<H: Host> Host for Controlled<H> {
    type Fd = H::Fd;

    fn mountinfo(&self) -> Result<Vec<MountEntry>, HostError> {
        self.before("mountinfo");
        self.host.mountinfo()
    }

    fn openat2(
        &self,
        base: Base<'_, Self::Fd>,
        path: &Path,
        how: OpenHow,
    ) -> Result<Self::Fd, HostError> {
        self.before("openat2");
        self.host.openat2(base, path, how)
    }

    fn statx(&self, base: Base<'_, Self::Fd>, path: &Path) -> Result<Stat, HostError> {
        self.before("statx");
        self.host.statx(base, path)
    }

    fn read_dir(&self, dir: &Self::Fd, limit: usize) -> Result<DirListing, HostError> {
        self.before("read_dir");
        self.host.read_dir(dir, limit)
    }

    fn statvfs(&self, fd: &Self::Fd) -> Result<Space, HostError> {
        self.before("statvfs");
        self.host.statvfs(fd)
    }

    fn flock_try(&self, fd: Self::Fd, mode: LockMode) -> Result<TryLock<Self::Fd>, HostError> {
        self.before("flock_try");
        self.host.flock_try(fd, mode)
    }

    fn flock_wait(
        &self,
        fd: Self::Fd,
        mode: LockMode,
        deadline: Option<MonoTime>,
    ) -> Result<WaitLock<Self::Fd>, HostError> {
        self.before("flock_wait");
        self.host.flock_wait(fd, mode, deadline)
    }

    fn now(&self) -> MonoTime {
        self.before("now");
        self.host.now()
    }

    fn sleep(&self, duration: Duration) {
        self.before("sleep");
        self.host.sleep(duration);
    }

    fn kernel_release(&self) -> Result<String, HostError> {
        self.before("kernel_release");
        self.host.kernel_release()
    }

    fn pids(&self) -> Result<Vec<Pid>, HostError> {
        self.before("pids");
        self.host.pids()
    }

    fn proc_fds(&self, pid: Pid) -> Result<Vec<ProcFd>, ProcError> {
        self.before("proc_fds");
        self.host.proc_fds(pid)
    }

    fn proc_links(&self, pid: Pid, which: ProcLink) -> Result<Vec<LinkTarget>, ProcError> {
        self.before("proc_links");
        self.host.proc_links(pid, which)
    }

    fn proc_maps(&self, pid: Pid) -> Result<Vec<MapEntry>, ProcError> {
        self.before("proc_maps");
        self.host.proc_maps(pid)
    }

    fn proc_environ(&self, pid: Pid) -> Result<Environ, ProcError> {
        self.before("proc_environ");
        self.host.proc_environ(pid)
    }

    /// The inner host's own (`LinuxHost`: several processes at once), as
    /// one call, unless a planned point names a `/proc` reader or a call by
    /// number: then the provided method, one reader call after another,
    /// each counted and pausable.
    fn proc_views(&self, pids: &[Pid]) -> Vec<Result<ProcView, ProcError>> {
        if self.targets_readers() {
            return pids
                .iter()
                .map(|pid| diskgc_core::host::read_proc_view(self, *pid))
                .collect();
        }
        self.before("proc_views");
        self.host.proc_views(pids)
    }

    fn own_pid(&self) -> Pid {
        self.before("own_pid");
        self.host.own_pid()
    }

    fn proc_status(&self, pid: Pid) -> Result<ProcStatus, ProcError> {
        self.before("proc_status");
        self.host.proc_status(pid)
    }

    fn bound_unix_sockets(&self) -> Result<Vec<BoundSocket>, HostError> {
        self.before("bound_unix_sockets");
        self.host.bound_unix_sockets()
    }
}

impl<H: Delete> Delete for Controlled<H> {
    fn unlinkat(&self, dir: &Self::Fd, name: &OsStr, what: Unlink) -> Result<(), HostError> {
        self.before("unlinkat");
        self.host.unlinkat(dir, name, what)
    }

    fn fchmod(&self, dir: &Self::Fd, perm: u32) -> Result<(), HostError> {
        self.before("fchmod");
        self.host.fchmod(dir, perm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskgc_core::resolve::{Want, resolve};
    use diskgc_model::ModelHost;
    use std::rc::Rc;

    const MODEL: &str = r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}
  - {id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}
tree:
  /tmp/a:
    f: {}
    sub: {.: {}}
processes:
  - {pid: 42, exe: /tmp/a/f, cwd: /tmp/a, environ: [A=1], maps: [/tmp/a/f], fds: [{fd: 0, path: /tmp/a/f}]}
"#;

    fn model() -> ModelHost {
        ModelHost::from_yaml(MODEL).unwrap_or_else(|e| panic!("{e}"))
    }

    /// A wrapper whose hits are recorded instead of acted on.
    fn recorded(plan: Plan) -> (Controlled<ModelHost>, Rc<RefCell<Vec<Hit>>>) {
        let hits = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&hits);
        let host = Controlled::new(
            model(),
            plan,
            Box::new(move |hit: &Hit| sink.borrow_mut().push(*hit)),
        );
        (host, hits)
    }

    /// Calls every port method once (and `statx` twice).
    fn call_everything<H: Delete>(host: &H) {
        let a = resolve(host, Path::new("/tmp/a"), Want::Directory).expect("resolve");
        host.read_dir(&a.fd, 10).expect("read_dir");
        host.statvfs(&a.fd).expect("statvfs");
        let deadline = host.now().checked_add(Duration::from_secs(1));
        let lockable = || {
            host.openat2(
                Base::Dir(&a.fd),
                Path::new("f"),
                diskgc_core::resolve::beneath_how(
                    diskgc_core::host::Access::Read,
                    diskgc_core::resolve::Want::Any,
                ),
            )
            .expect("open f")
        };
        drop(
            host.flock_try(lockable(), LockMode::Shared)
                .expect("flock_try"),
        );
        drop(
            host.flock_wait(lockable(), LockMode::Shared, deadline)
                .expect("flock_wait"),
        );
        host.sleep(Duration::from_millis(1));
        host.kernel_release().expect("kernel_release");
        host.pids().expect("pids");
        host.proc_fds(Pid(42)).expect("proc_fds");
        host.proc_links(Pid(42), ProcLink::Cwd).expect("proc_links");
        host.proc_maps(Pid(42)).expect("proc_maps");
        host.proc_environ(Pid(42)).expect("proc_environ");
        // Delegated as one call: the model's provided method then reads
        // the readers itself (not counted here); one pid, five readers
        // plus the environment.
        assert_eq!(host.proc_views(&[Pid(42)]).len(), 1);
        host.own_pid();
        host.proc_status(Pid(42)).expect("proc_status");
        host.bound_unix_sockets().expect("bound_unix_sockets");
        host.statx(Base::Dir(&a.fd), Path::new("sub"))
            .expect("statx");
        host.unlinkat(&a.fd, OsStr::new("sub"), Unlink::Dir)
            .expect("unlinkat");
        host.fchmod(&a.fd, 0o755).expect("fchmod");
    }

    #[test]
    fn every_port_call_is_counted_once_like_the_models_trace() {
        let (host, hits) = recorded(Plan::new());
        call_everything(&host);
        let ops = host.calls_per_op();
        assert_eq!(
            ops.keys().copied().collect::<Vec<_>>().len(),
            OPS.len(),
            "every operation counted: {ops:?}"
        );
        assert!(OPS.iter().all(|op| ops.contains_key(op)), "{ops:?}");
        // The model records each port call on its trace: one count each,
        // except the delegated proc_views, one count for its 6 reader calls.
        assert_eq!(host.calls() + 5, host.host.trace().len() as u64);
        assert_eq!(ops.values().sum::<u64>(), host.calls());
        assert!(hits.borrow().is_empty());
        assert!(host.unreached().is_empty());
    }

    /// A kill stands in for death by panicking; the model's trace then
    /// shows whether the killed call was performed.
    /// `proc_views` goes to the inner host as one call unless a point
    /// names a `/proc` reader or a call number; then each reader call is
    /// made (and counted, and pausable) here.
    #[test]
    fn proc_views_is_delegated_unless_a_point_targets_a_reader() {
        let (host, _) = recorded(Plan::new().at(
            Point::Nth {
                op: "statx",
                nth: 99,
            },
            Action::Pause,
        ));
        host.proc_views(&[Pid(42)]);
        assert_eq!(host.calls_per_op().get("proc_views"), Some(&1));
        assert_eq!(host.calls_per_op().get("proc_fds"), None);
        for point in [
            Point::Nth {
                op: "proc_maps",
                nth: 99,
            },
            Point::Call(999),
        ] {
            let (host, _) = recorded(Plan::new().at(point, Action::Pause));
            host.proc_views(&[Pid(42)]);
            assert_eq!(host.calls_per_op().get("proc_views"), None, "{point}");
            assert_eq!(host.calls_per_op().get("proc_fds"), Some(&1), "{point}");
        }
    }

    #[test]
    fn a_point_acts_before_its_call_and_a_kill_stops_the_run_there() {
        // Call 5 is resolve's second statx (of the opened /tmp/a); the
        // unlinkat is killed, so the pause planned on a second one is never
        // reached.
        let plan = Plan::new()
            .at(Point::Call(5), Action::Pause)
            .at(
                Point::Nth {
                    op: "unlinkat",
                    nth: 1,
                },
                Action::Kill,
            )
            .at(
                Point::Nth {
                    op: "unlinkat",
                    nth: 2,
                },
                Action::Pause,
            );
        let hits = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&hits);
        let host = Controlled::new(
            model(),
            plan,
            Box::new(move |hit: &Hit| {
                sink.borrow_mut().push(*hit);
                if hit.action == Action::Kill {
                    panic!("killed before call {}", hit.call);
                }
            }),
        );
        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            call_everything(&host);
        }));
        assert!(died.is_err(), "the run went past the kill");
        let kill_call = host.calls();
        assert_eq!(
            *hits.borrow(),
            [
                Hit {
                    call: 5,
                    op: "statx",
                    nth: 2,
                    action: Action::Pause
                },
                Hit {
                    call: kill_call,
                    op: "unlinkat",
                    nth: 1,
                    action: Action::Kill
                },
            ]
        );
        let trace = host.host.trace();
        assert_eq!(
            trace.len() as u64,
            kill_call - 1,
            "the killed call must not have run: {:?}",
            trace.last()
        );
        assert!(
            host.host.stat_path(Path::new("/tmp/a/sub")).is_some(),
            "nothing was unlinked"
        );
        assert_eq!(host.unreached(), ["pause at unlinkat:2"]);
    }

    #[test]
    fn points_parse_from_the_command_line() {
        assert_eq!(Point::parse("17"), Ok(Point::Call(17)));
        assert_eq!(
            Point::parse("unlinkat:2"),
            Ok(Point::Nth {
                op: "unlinkat",
                nth: 2
            })
        );
        for bad in ["0", "unlinkat:0", "x", "rmdir:1", "unlinkat:", ":1", "-3"] {
            assert!(Point::parse(bad).is_err(), "{bad:?} accepted");
        }
        assert_eq!(
            Point::parse("read_dir:3").unwrap().to_string(),
            "read_dir:3"
        );
    }

    #[test]
    fn control_lines_round_trip() {
        let lines = [
            ControlLine::Pause {
                call: 7,
                op: "unlinkat".into(),
                nth: 2,
                pid: 99,
            },
            ControlLine::Resume { call: 7 },
            ControlLine::Kill {
                call: 9,
                op: "statx".into(),
                nth: 4,
                pid: 99,
            },
            ControlLine::Done { calls: 12 },
        ];
        for line in lines {
            let text = line.to_string();
            assert!(text.starts_with(CONTROL_PREFIX), "{text}");
            assert_eq!(ControlLine::parse(&text), Ok(Some(line)));
        }
        assert_eq!(ControlLine::parse("0 file \"a\""), Ok(None));
        assert!(ControlLine::parse(&format!("{CONTROL_PREFIX} pause call=x")).is_err());
        assert!(ControlLine::parse(&format!("{CONTROL_PREFIX} jump")).is_err());
    }
}
