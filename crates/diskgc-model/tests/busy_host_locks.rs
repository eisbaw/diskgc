//! TASK-364 (mped review S3): the reclaim walk waits a busy host out only
//! after the candidate's protocol has returned, so no candidate lock is
//! held during any pause: not an `fs` candidate's `flock`, not a Cargo
//! target's locks (a Cargo build would block on them). A host that notes,
//! at every sleep, whether diskgc holds a lock on a candidate. The short
//! pauses inside a protocol stay under its locks, as designed: the 20 ms
//! retry of a busy scan, a process's re-reads (40 ms in all) and a stale
//! scan's retry pauses (250 ms, 1 s; docs/safety.md, In use: Freshness).
//! So a pause, contiguous sleeps merged, longer than 1 s under a lock is
//! the failure.

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::host::{
    Base, BoundSocket, Delete, DirListing, Environ, Host, HostError, LinkTarget, LockMode, Locked,
    MapEntry, MonoTime, MountEntry, OpenHow, Pid, ProcError, ProcFd, ProcLink, ProcStatus,
    ProcView, Space, Stat, Timestamp, TryLock, Unlink, WaitLock,
};
use diskgc_core::profile::Library;
use diskgc_core::units::parse_size;
use diskgc_model::{Holder, ModelFd, ModelHost};

/// The model, the candidate paths watched, and every sleep taken holding
/// a lock on one: when it started, how long.
struct LockWatch(ModelHost, Vec<PathBuf>, RefCell<Vec<(MonoTime, Duration)>>);

impl LockWatch {
    /// The pauses taken holding a lock, contiguous sleeps merged.
    fn locked_pauses(&self) -> Vec<(MonoTime, Duration)> {
        let mut pauses: Vec<(MonoTime, Duration)> = Vec::new();
        for (at, slept) in self.2.borrow().iter().copied() {
            match pauses.last_mut() {
                Some((start, total)) if start.checked_add(*total) == Some(at) => *total += slept,
                _ => pauses.push((at, slept)),
            }
        }
        pauses
    }
}

impl Host for LockWatch {
    type Fd = ModelFd;
    type ProcIdentity = <ModelHost as Host>::ProcIdentity;

    fn mountinfo(&self) -> Result<Vec<MountEntry>, HostError> {
        self.0.mountinfo()
    }
    fn openat2(
        &self,
        base: Base<'_, ModelFd>,
        path: &Path,
        how: OpenHow,
    ) -> Result<ModelFd, HostError> {
        self.0.openat2(base, path, how)
    }
    fn statx(&self, base: Base<'_, ModelFd>, path: &Path) -> Result<Stat, HostError> {
        self.0.statx(base, path)
    }
    fn read_link(
        &self,
        dir: &ModelFd,
        name: &std::ffi::OsStr,
    ) -> Result<std::path::PathBuf, HostError> {
        self.0.read_link(dir, name)
    }
    fn read_dir(&self, dir: &ModelFd, limit: usize) -> Result<DirListing, HostError> {
        self.0.read_dir(dir, limit)
    }
    fn listable(&self, dir: &ModelFd) -> Result<(), HostError> {
        self.0.listable(dir)
    }
    fn read_head(&self, file: &ModelFd, limit: usize) -> Result<Vec<u8>, HostError> {
        self.0.read_head(file, limit)
    }
    fn reopen_regular(&self, file: &ModelFd) -> Result<ModelFd, HostError> {
        self.0.reopen_regular(file)
    }
    fn statvfs(&self, fd: &ModelFd) -> Result<Space, HostError> {
        self.0.statvfs(fd)
    }
    fn flock_try(&self, fd: ModelFd, mode: LockMode) -> Result<TryLock<ModelFd>, HostError> {
        self.0.flock_try(fd, mode)
    }
    fn flock_wait(
        &self,
        fd: ModelFd,
        mode: LockMode,
        deadline: Option<MonoTime>,
    ) -> Result<WaitLock<ModelFd>, HostError> {
        self.0.flock_wait(fd, mode, deadline)
    }
    fn now(&self) -> MonoTime {
        self.0.now()
    }
    fn interrupted(&self) -> Option<diskgc_core::report::Signal> {
        self.0.interrupted()
    }
    fn second_signal(&self) -> bool {
        self.0.second_signal()
    }
    fn wall_clock(&self) -> Timestamp {
        self.0.wall_clock()
    }
    /// Notes a sleep taken holding a lock on a watched candidate.
    fn sleep(&self, duration: Duration) {
        let held = self.1.iter().any(|path| {
            self.0
                .locks_on(path)
                .iter()
                .any(|(holder, _)| *holder == Holder::Own)
        });
        if held {
            self.2.borrow_mut().push((self.0.now(), duration));
        }
        self.0.sleep(duration);
    }
    fn pressure(&self) -> Result<diskgc_core::host::Pressure, HostError> {
        self.0.pressure()
    }
    fn lower_priority(&self) -> Result<(), HostError> {
        self.0.lower_priority()
    }
    fn kernel_release(&self) -> Result<String, HostError> {
        self.0.kernel_release()
    }
    fn hostname(&self) -> Result<String, HostError> {
        self.0.hostname()
    }
    fn pids(&self) -> Result<Vec<Pid>, HostError> {
        self.0.pids()
    }
    fn proc_identity(&self, pid: Pid) -> Result<Self::ProcIdentity, ProcError> {
        self.0.proc_identity(pid)
    }
    fn proc_identity_released(&self, identity: &Self::ProcIdentity) -> Result<bool, ProcError> {
        self.0.proc_identity_released(identity)
    }
    fn proc_fds(&self, pid: Pid) -> Result<Vec<ProcFd>, ProcError> {
        self.0.proc_fds(pid)
    }
    fn proc_links(&self, pid: Pid, which: ProcLink) -> Result<Vec<LinkTarget>, ProcError> {
        self.0.proc_links(pid, which)
    }
    fn proc_maps(&self, pid: Pid) -> Result<Vec<MapEntry>, ProcError> {
        self.0.proc_maps(pid)
    }
    fn proc_environ(&self, pid: Pid) -> Result<Environ, ProcError> {
        self.0.proc_environ(pid)
    }
    fn proc_views(&self, pids: &[Pid], facts: bool) -> Vec<Result<ProcView, ProcError>> {
        self.0.proc_views(pids, facts)
    }
    fn own_pid(&self) -> Pid {
        self.0.own_pid()
    }
    fn effective_uid(&self) -> u32 {
        self.0.effective_uid()
    }
    fn account_uid(&self, name: &str) -> Result<Option<u32>, HostError> {
        self.0.account_uid(name)
    }
    fn inherit_lock(&self, lock: &Locked<ModelFd>) -> Result<(), HostError> {
        self.0.inherit_lock(lock)
    }
    fn proc_status(&self, pid: Pid) -> Result<ProcStatus, ProcError> {
        self.0.proc_status(pid)
    }
    fn bound_unix_sockets(&self) -> Result<Vec<BoundSocket>, HostError> {
        self.0.bound_unix_sockets()
    }

    fn pid_namespace(&self) -> Result<diskgc_core::host::PidNamespace, HostError> {
        self.0.pid_namespace()
    }

    fn proc_nspid(&self, pid: Pid) -> Result<Vec<u32>, diskgc_core::host::ProcError> {
        self.0.proc_nspid(pid)
    }

    fn pid_alive(&self, pid: Pid) -> Result<bool, HostError> {
        self.0.pid_alive(pid)
    }

    fn fs_id(&self, fd: &Self::Fd) -> Result<u64, HostError> {
        self.0.fs_id(fd)
    }

    fn loop_devices(
        &self,
        mount: &diskgc_core::host::MountEntry,
        fd: Option<&Self::Fd>,
    ) -> Result<Vec<diskgc_core::host::Dev>, HostError> {
        self.0.loop_devices(mount, fd)
    }

    fn fs_magic(&self, fd: &Self::Fd) -> Result<u64, HostError> {
        self.0.fs_magic(fd)
    }

    fn proc_uids(&self, pid: Pid) -> Result<diskgc_core::host::Ids, diskgc_core::host::ProcError> {
        self.0.proc_uids(pid)
    }

    fn proc_cmdline(
        &self,
        pid: Pid,
    ) -> Result<Vec<std::ffi::OsString>, diskgc_core::host::ProcError> {
        self.0.proc_cmdline(pid)
    }

    fn proc_root_lookup(
        &self,
        pid: Pid,
        path: &std::path::Path,
        climb: bool,
    ) -> Result<Vec<diskgc_core::host::DevIno>, diskgc_core::host::ProcError> {
        self.0.proc_root_lookup(pid, path, climb)
    }
    fn proc_climb(
        &self,
        pid: Pid,
        from: diskgc_core::host::HeldDir,
    ) -> Result<Vec<diskgc_core::host::DevIno>, diskgc_core::host::ProcError> {
        self.0.proc_climb(pid, from)
    }
    fn proc_mountinfo(
        &self,
        pid: Pid,
    ) -> Result<Vec<diskgc_core::host::MountEntry>, diskgc_core::host::ProcError> {
        self.0.proc_mountinfo(pid)
    }
    fn fs_uuid(
        &self,
        fd: &Self::Fd,
        mount: &diskgc_core::host::MountEntry,
    ) -> Result<Option<String>, HostError> {
        self.0.fs_uuid(fd, mount)
    }

    fn loop_backing_file(
        &self,
        dev: diskgc_core::host::Dev,
    ) -> Result<Option<std::path::PathBuf>, HostError> {
        self.0.loop_backing_file(dev)
    }

    fn loop_backing_files(
        &self,
    ) -> Result<Vec<(diskgc_core::host::Dev, std::path::PathBuf)>, HostError> {
        self.0.loop_backing_files()
    }

    fn loop_backing_identity(
        &self,
        dev: diskgc_core::host::Dev,
    ) -> Result<diskgc_core::host::LoopIdentity, HostError> {
        self.0.loop_backing_identity(dev)
    }

    fn test_container_pid_namespace(&self) -> bool {
        self.0.test_container_pid_namespace()
    }

    fn boot_id(&self) -> Result<String, HostError> {
        self.0.boot_id()
    }

    fn state_lock(
        &self,
        key: &diskgc_core::host::FsKey,
    ) -> Result<diskgc_core::host::StateLock<Self::Fd>, HostError> {
        self.0.state_lock(key)
    }
    fn user_lock_path(&self) -> Result<Option<std::path::PathBuf>, HostError> {
        self.0.user_lock_path()
    }

    fn user_lock(
        &self,
        make: diskgc_core::host::MakeUserLock,
    ) -> Result<Option<diskgc_core::host::UserLock<Self::Fd>>, diskgc_core::host::UserLockError>
    {
        self.0.user_lock(make)
    }

    fn state_read(
        &self,
        key: &diskgc_core::host::FsKey,
        limit: u64,
    ) -> Result<Option<diskgc_core::host::StateFile>, HostError> {
        self.0.state_read(key, limit)
    }

    fn state_write(
        &self,
        lock: &diskgc_core::host::Locked<Self::Fd>,
        dir: &Self::Fd,
        key: &diskgc_core::host::FsKey,
        bytes: &[u8],
        cancel: &dyn Fn() -> bool,
    ) -> Result<diskgc_core::host::StateWritten, HostError> {
        self.0.state_write(lock, dir, key, bytes, cancel)
    }
}

impl Delete for LockWatch {
    fn may_delete(&self) -> Result<(), HostError> {
        self.0.may_delete()
    }
    fn unlinkat(
        &self,
        dir: &ModelFd,
        name: &std::ffi::OsStr,
        what: Unlink,
    ) -> Result<(), HostError> {
        self.0.unlinkat(dir, name, what)
    }
    fn fchmod(&self, dir: &ModelFd, deadline: Option<MonoTime>) -> Result<bool, HostError> {
        self.0.fchmod(dir, deadline)
    }
    fn run_command(
        &self,
        cwd: &ModelFd,
        run: &diskgc_core::host::CommandRun,
    ) -> diskgc_core::host::CommandInvocation {
        self.0.run_command(cwd, run)
    }
    fn nix_roots(
        &self,
        client: &diskgc_core::host::NixClient<ModelFd>,
        socket: &ModelFd,
        targets: &[PathBuf],
        environment: &[(std::ffi::OsString, std::ffi::OsString)],
        deadline: MonoTime,
    ) -> Result<String, HostError> {
        self.0
            .nix_roots(client, socket, targets, environment, deadline)
    }
    fn nix_gc(
        &self,
        client: &diskgc_core::host::NixClient<ModelFd>,
        socket: &ModelFd,
        run: &diskgc_core::host::NixGc,
    ) -> diskgc_core::host::CommandInvocation {
        self.0.nix_gc(client, socket, run)
    }
}

/// A fork storm over the first `listings` process listings (pid 10 forks
/// after each): a scan and its retry do not settle over 32.
fn storm(listings: u32) -> String {
    (1..=listings)
        .map(|k| {
            format!(
                "  - {{at: {{op: pids, nth: {k}, when: after}}, fork: {{pid: 10, child: {}}}}}\n",
                1000 + k
            )
        })
        .collect()
}

/// A Cargo target and an `fs` candidate (a Node project's `node_modules`,
/// a directory the protocol `flock`s) waited out through a storm of
/// `listings`: no pause holds a lock on either, the waits did happen, and
/// both are reclaimed once the storm is over.
#[test]
fn no_candidate_lock_is_held_while_waiting_for_a_busy_host() {
    let at = "2026-10-01T00:00:00Z";
    let profile = format!(
        "{{.: {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}, .fingerprint: {{.: {{mtime: {at}}}, \
         p-1: {{.: {{mtime: {at}}}, bin-p: {{mtime: {at}}}}}}}, deps: {{.: {{mtime: {at}}}, p-1: \
         {{size: 1M, mtime: {at}}}}}}}"
    );
    let model = |storm_listings: u32| {
        host(&format!(
            r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 100G, other_used: 99G, parent: 1}}
tree:
  /run/diskgc/lock: {{uid: 0, gid: 0}}
  /usr/bin/tool: {{}}
  /home/u/src/p01/Cargo.toml: {{}}
  /home/u/src/p01/target: {{CACHEDIR.TAG: {{cachedir_tag: true}}, debug: {profile}}}
  /home/u/n:
    .: {{mtime: 2026-08-16T03:00:00Z}}
    package.json: {{content: "{{\"name\": \"n\", \"version\": \"1.0.0\"}}", mtime: 2026-08-16T03:00:00Z}}
    node_modules:
      .: {{mtime: 2026-08-16T03:00:00Z}}
      x: {{.: {{mtime: 2026-08-16T03:00:00Z}}, i.js: {{size: 4K, mtime: 2026-08-16T03:00:00Z}}}}
processes:
  - {{pid: 10, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], environ: [PATH=/usr/bin], fds: [{{path: /usr/bin/tool}}]}}
events:
{}"#,
            storm(storm_listings)
        ))
    };
    let library = Library::builtin().unwrap_or_else(|e| panic!("{e}"));
    for (select, roots, watched) in [
        (
            "profile:cargo-target",
            "/home/u/src",
            vec![
                PathBuf::from("/home/u/src/p01/target"),
                PathBuf::from("/home/u/src/p01/target/debug"),
                PathBuf::from("/home/u/src/p01/target/debug/.cargo-lock"),
            ],
        ),
        (
            "profile:node-modules",
            "/home/u/n",
            vec![PathBuf::from("/home/u/n/node_modules")],
        ),
    ] {
        let watch = LockWatch(model(64), watched.clone(), RefCell::new(Vec::new()));
        let request = Request {
            path: PathBuf::from(roots),
            min_free: parse_size("100%").expect("a size"),
            target_free: None,
            timeout: Some(Duration::from_secs(600)),
            tally_budget: None,
            watch: None,
            tier: Tier::select(select, &library, Some(&[roots.to_owned()]), 1000)
                .unwrap_or_else(|e| panic!("{e}")),
        };
        let mut lines = Vec::new();
        let report = executor::ensure(&watch, &request, &[1000], &mut |l| lines.push(l.to_owned()));
        eprintln!("log:\n  {}", lines.join("\n  "));
        let long: Vec<_> = watch
            .locked_pauses()
            .into_iter()
            .filter(|(_, total)| *total > Duration::from_secs(1))
            .collect();
        assert!(long.is_empty(), "{select}: paused holding a lock: {long:?}");
        assert!(
            lines.iter().any(|l| l.contains("the host is too busy")),
            "{select}: no wait happened: {lines:#?}"
        );
        assert!(
            watch.0.unfired_events().is_empty(),
            "{select}: the storm outlasted the run"
        );
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.candidate.starts_with(Path::new(roots))),
            "{select}: {report:#?}"
        );
    }
}
