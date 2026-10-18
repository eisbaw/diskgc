//! The in-use scan against a host that breaks the port's contract: a read
//! of several processes answered with fewer results than pids asked for
//! must fail the scan (an error of the run), never leave a pid unread
//! (task-6 review round 2, mped should-fix 4).

mod common;

use std::path::Path;
use std::time::Duration;

use common::host;
use diskgc_core::host::{
    Base, BoundSocket, DirListing, Environ, Host, HostError, LinkTarget, LockMode, MapEntry,
    MonoTime, MountEntry, OpenHow, Pid, ProcError, ProcFd, ProcLink, ProcStatus, ProcView, Space,
    Stat, Timestamp, TryLock, WaitLock,
};
use diskgc_core::inuse::{self, Options};
use diskgc_model::{ModelFd, ModelHost};

/// ModelHost, except that `proc_views` drops its last result.
struct Short(ModelHost);

impl Host for Short {
    type Fd = ModelFd;

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
    fn read_dir(&self, dir: &ModelFd, limit: usize) -> Result<DirListing, HostError> {
        self.0.read_dir(dir, limit)
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
    fn wall_clock(&self) -> Timestamp {
        self.0.wall_clock()
    }
    fn sleep(&self, duration: Duration) {
        self.0.sleep(duration);
    }
    fn kernel_release(&self) -> Result<String, HostError> {
        self.0.kernel_release()
    }
    fn pids(&self) -> Result<Vec<Pid>, HostError> {
        self.0.pids()
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
    fn proc_views(&self, pids: &[Pid]) -> Vec<Result<ProcView, ProcError>> {
        let mut views = self.0.proc_views(pids);
        views.pop();
        views
    }
    fn own_pid(&self) -> Pid {
        self.0.own_pid()
    }
    fn proc_status(&self, pid: Pid) -> Result<ProcStatus, ProcError> {
        self.0.proc_status(pid)
    }
    fn bound_unix_sockets(&self) -> Result<Vec<BoundSocket>, HostError> {
        self.0.bound_unix_sockets()
    }
}

#[test]
fn a_short_answer_to_a_read_of_several_processes_fails_the_scan() {
    let model = host(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}
tree:
  /usr/bin/tool: {}
  /keep: {}
  /c/a: {}
processes:
  - {pid: 7, exe: /usr/bin/tool, maps: [/usr/bin/tool], environ: [A=1], fds: [{path: /keep}]}
  - {pid: 9, exe: /usr/bin/tool, maps: [/usr/bin/tool], environ: [A=1], fds: [{path: /c/a}]}
"#,
    );
    let options = Options {
        users: &[1000],
        sockets: false,
    };
    let err = inuse::scan(&Short(model), &options).expect_err("a short answer is an error");
    assert!(err.detail.contains("answered with 1 results"), "{err}");
}
