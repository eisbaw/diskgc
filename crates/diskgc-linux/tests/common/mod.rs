//! Test support for the real-kernel tests of this crate.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::host::{
    Base, BoundSocket, DaemonProbe, Dev, DevIno, DirListing, DockerEnvironment, DockerQuery,
    Environ, FsKey, HeldDir, Host, HostError, Ids, LinkTarget, LockMode, Locked, LoopIdentity,
    MakeUserLock, MapEntry, MonoTime, MountEntry, NixClient, NixEnvironment, NixQuery, OpenHow,
    Pid, PidNamespace, PodmanQuery, Pressure, ProcError, ProcFd, ProcLink, ProcStatus, ProcView,
    Space, Stat, StateFile, StateLock, StateWritten, Timestamp, TryLock, UserLock, UserLockError,
    WaitLock,
};
use diskgc_core::report::Signal;
use diskgc_linux::{LinuxFd, LinuxHost};

type Identity = <LinuxHost as Host>::ProcIdentity;

/// A [`LinuxHost`] whose process list is diskgc's own process alone: the
/// real kernel for every filesystem call, a fixed, settled process table
/// for the in-use scan. diskgc never counts its own process (docs/safety.md,
/// In use), so this scan finds nothing in use, whatever the rest of the
/// machine is doing, and settles at once.
///
/// For tests whose verdicts must not depend on other processes: on a busy
/// host a real scan can stay unsettled (`in_use_unknown`, cause `busy`)
/// for one run and not the next (TASK-337). Every other call, including
/// the reads of diskgc's own process, goes to the kernel: a scan that
/// stopped passing over that process would count its descriptors (the
/// read-ahead threads' open directories). Another process's use is not
/// seen at all.
///
/// Every [`Host`] method is forwarded by hand, defaulted ones included
/// (`LinuxHost` overrides several, `read_ahead` among them): a method
/// added to the trait with a default must be added here too.
pub struct OwnProcessOnly(pub LinuxHost);

impl Host for OwnProcessOnly {
    type Fd = LinuxFd;
    type ProcIdentity = Identity;

    /// Only diskgc's own process.
    fn pids(&self) -> Result<Vec<Pid>, HostError> {
        Ok(vec![self.0.own_pid()])
    }

    // Everything else is the kernel's.
    fn mountinfo(&self) -> Result<Vec<MountEntry>, HostError> {
        self.0.mountinfo()
    }
    fn openat2(
        &self,
        base: Base<'_, LinuxFd>,
        path: &Path,
        how: OpenHow,
    ) -> Result<LinuxFd, HostError> {
        self.0.openat2(base, path, how)
    }
    fn statx(&self, base: Base<'_, LinuxFd>, path: &Path) -> Result<Stat, HostError> {
        self.0.statx(base, path)
    }
    fn executable_file(&self, path: &Path) -> Result<bool, HostError> {
        self.0.executable_file(path)
    }
    fn command_path(&self) -> OsString {
        self.0.command_path()
    }
    fn docker_environment(&self) -> DockerEnvironment {
        self.0.docker_environment()
    }
    fn docker_query(&self, query: DockerQuery) -> Result<String, HostError> {
        self.0.docker_query(query)
    }
    fn podman_query(&self, query: PodmanQuery) -> Result<String, HostError> {
        self.0.podman_query(query)
    }
    fn nix_environment(&self) -> NixEnvironment {
        self.0.nix_environment()
    }
    fn nix_query(
        &self,
        client: &NixClient<LinuxFd>,
        query: NixQuery,
        socket: Option<&LinuxFd>,
        environment: Option<&[(OsString, OsString)]>,
    ) -> Result<String, HostError> {
        self.0.nix_query(client, query, socket, environment)
    }
    fn nix_daemon_probe(
        &self,
        socket: &LinuxFd,
        timeout: Duration,
    ) -> Result<DaemonProbe, HostError> {
        self.0.nix_daemon_probe(socket, timeout)
    }
    fn read_backend_config(&self, path: &Path, limit: usize) -> Result<Option<Vec<u8>>, HostError> {
        self.0.read_backend_config(path, limit)
    }
    fn command_origin(&self) -> Result<PathBuf, HostError> {
        self.0.command_origin()
    }
    fn read_link(&self, dir: &LinuxFd, name: &OsStr) -> Result<PathBuf, HostError> {
        self.0.read_link(dir, name)
    }
    fn read_dir(&self, dir: &LinuxFd, limit: usize) -> Result<DirListing, HostError> {
        self.0.read_dir(dir, limit)
    }
    fn listable(&self, dir: &LinuxFd) -> Result<(), HostError> {
        self.0.listable(dir)
    }
    fn read_ahead(&self, path: &Path) {
        self.0.read_ahead(path)
    }
    fn read_head(&self, file: &LinuxFd, limit: usize) -> Result<Vec<u8>, HostError> {
        self.0.read_head(file, limit)
    }
    fn reopen_regular(&self, file: &LinuxFd) -> Result<LinuxFd, HostError> {
        self.0.reopen_regular(file)
    }
    fn statvfs(&self, fd: &LinuxFd) -> Result<Space, HostError> {
        self.0.statvfs(fd)
    }
    fn flock_try(&self, fd: LinuxFd, mode: LockMode) -> Result<TryLock<LinuxFd>, HostError> {
        self.0.flock_try(fd, mode)
    }
    fn flock_wait(
        &self,
        fd: LinuxFd,
        mode: LockMode,
        deadline: Option<MonoTime>,
    ) -> Result<WaitLock<LinuxFd>, HostError> {
        self.0.flock_wait(fd, mode, deadline)
    }
    fn interrupted(&self) -> Option<Signal> {
        self.0.interrupted()
    }
    fn second_signal(&self) -> bool {
        self.0.second_signal()
    }
    fn now(&self) -> MonoTime {
        self.0.now()
    }
    fn wall_clock(&self) -> Timestamp {
        self.0.wall_clock()
    }
    fn sleep(&self, duration: Duration) {
        self.0.sleep(duration)
    }
    fn pressure(&self) -> Result<Pressure, HostError> {
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
    fn proc_identity(&self, pid: Pid) -> Result<Identity, ProcError> {
        self.0.proc_identity(pid)
    }
    fn proc_identity_released(&self, identity: &Identity) -> Result<bool, ProcError> {
        self.0.proc_identity_released(identity)
    }
    fn proc_fds(&self, pid: Pid) -> Result<Vec<ProcFd>, ProcError> {
        self.0.proc_fds(pid)
    }
    fn proc_links(&self, pid: Pid, which: ProcLink) -> Result<Vec<LinkTarget>, ProcError> {
        self.0.proc_links(pid, which)
    }
    fn proc_links_all(&self, pid: Pid) -> Result<[Vec<LinkTarget>; 3], ProcError> {
        self.0.proc_links_all(pid)
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
    fn take_proc_read_notes(&self) -> Vec<String> {
        self.0.take_proc_read_notes()
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
    fn inherit_lock(&self, lock: &Locked<LinuxFd>) -> Result<(), HostError> {
        self.0.inherit_lock(lock)
    }
    fn proc_status(&self, pid: Pid) -> Result<ProcStatus, ProcError> {
        self.0.proc_status(pid)
    }
    fn bound_unix_sockets(&self) -> Result<Vec<BoundSocket>, HostError> {
        self.0.bound_unix_sockets()
    }
    fn pid_namespace(&self) -> Result<PidNamespace, HostError> {
        self.0.pid_namespace()
    }
    fn test_container_pid_namespace(&self) -> bool {
        self.0.test_container_pid_namespace()
    }
    fn proc_nspid(&self, pid: Pid) -> Result<Vec<u32>, ProcError> {
        self.0.proc_nspid(pid)
    }
    fn pid_alive(&self, pid: Pid) -> Result<bool, HostError> {
        self.0.pid_alive(pid)
    }
    fn fs_magic(&self, fd: &LinuxFd) -> Result<u64, HostError> {
        self.0.fs_magic(fd)
    }
    fn fs_id(&self, fd: &LinuxFd) -> Result<u64, HostError> {
        self.0.fs_id(fd)
    }
    fn loop_devices(
        &self,
        mount: &MountEntry,
        fd: Option<&LinuxFd>,
    ) -> Result<Vec<Dev>, HostError> {
        self.0.loop_devices(mount, fd)
    }
    fn proc_uids(&self, pid: Pid) -> Result<Ids, ProcError> {
        self.0.proc_uids(pid)
    }
    fn proc_cmdline(&self, pid: Pid) -> Result<Vec<OsString>, ProcError> {
        self.0.proc_cmdline(pid)
    }
    fn proc_root_lookup(
        &self,
        pid: Pid,
        path: &Path,
        climb: bool,
    ) -> Result<Vec<DevIno>, ProcError> {
        self.0.proc_root_lookup(pid, path, climb)
    }
    fn proc_climb(&self, pid: Pid, from: HeldDir) -> Result<Vec<DevIno>, ProcError> {
        self.0.proc_climb(pid, from)
    }
    fn proc_mountinfo(&self, pid: Pid) -> Result<Vec<MountEntry>, ProcError> {
        self.0.proc_mountinfo(pid)
    }
    fn fs_uuid(&self, fd: &LinuxFd, mount: &MountEntry) -> Result<Option<String>, HostError> {
        self.0.fs_uuid(fd, mount)
    }
    fn loop_backing_file(&self, dev: Dev) -> Result<Option<PathBuf>, HostError> {
        self.0.loop_backing_file(dev)
    }
    fn loop_backing_files(&self) -> Result<Vec<(Dev, PathBuf)>, HostError> {
        self.0.loop_backing_files()
    }
    fn loop_backing_identity(&self, dev: Dev) -> Result<LoopIdentity, HostError> {
        self.0.loop_backing_identity(dev)
    }
    fn boot_id(&self) -> Result<String, HostError> {
        self.0.boot_id()
    }
    fn state_lock(&self, key: &FsKey) -> Result<StateLock<LinuxFd>, HostError> {
        self.0.state_lock(key)
    }
    fn state_read(&self, key: &FsKey, limit: u64) -> Result<Option<StateFile>, HostError> {
        self.0.state_read(key, limit)
    }
    fn state_write(
        &self,
        lock: &Locked<LinuxFd>,
        dir: &LinuxFd,
        key: &FsKey,
        bytes: &[u8],
        cancel: &dyn Fn() -> bool,
    ) -> Result<StateWritten, HostError> {
        self.0.state_write(lock, dir, key, bytes, cancel)
    }
    fn user_lock_path(&self) -> Result<Option<PathBuf>, HostError> {
        self.0.user_lock_path()
    }
    fn user_lock(&self, make: MakeUserLock) -> Result<Option<UserLock<LinuxFd>>, UserLockError> {
        self.0.user_lock(make)
    }
}
