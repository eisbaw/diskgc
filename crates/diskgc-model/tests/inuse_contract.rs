//! The in-use scan against a host that breaks the port's contract: a read
//! of several processes answered with fewer results than pids asked for
//! must fail the scan (an error of the run), never leave a pid unread
//! (task-6 review round 2, mped should-fix 4).

mod common;

use std::path::Path;
use std::time::Duration;

use common::host;
use diskgc_core::host::{
    Base, BoundSocket, DirListing, Environ, Host, HostError, LinkTarget, LockMode, Locked,
    MapEntry, MonoTime, MountEntry, OpenHow, Pid, ProcError, ProcFd, ProcLink, ProcStatus,
    ProcView, Space, Stat, Timestamp, TryLock, WaitLock,
};
use diskgc_core::inuse::{self, Options};
use diskgc_model::{ModelFd, ModelHost};

/// ModelHost, except that `proc_views` drops its last result.
struct Short(ModelHost);

impl Host for Short {
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
    fn sleep(&self, duration: Duration) {
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
        let mut views = self.0.proc_views(pids, facts);
        views.pop();
        views
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
    fn fs_uuid(&self, dev: diskgc_core::host::Dev) -> Result<Option<String>, HostError> {
        self.0.fs_uuid(dev)
    }

    fn loop_backing_file(
        &self,
        dev: diskgc_core::host::Dev,
    ) -> Result<Option<std::path::PathBuf>, HostError> {
        self.0.loop_backing_file(dev)
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
    fn user_lock_path(&self) -> Option<std::path::PathBuf> {
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
        key: &diskgc_core::host::FsKey,
        bytes: &[u8],
        cancel: &dyn Fn() -> bool,
    ) -> Result<diskgc_core::host::StateWritten, HostError> {
        self.0.state_write(lock, key, bytes, cancel)
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
        pid_lookup: false,
        pytest_facts: false,
    };
    let err = inuse::scan(&Short(model), &options, None).expect_err("a short answer is an error");
    assert!(err.to_string().contains("answered with 1 results"), "{err}");
}
