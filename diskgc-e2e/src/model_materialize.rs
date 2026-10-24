//! Materialize the kernel-representable part of a `HostModel` on the
//! container's scratch tmpfs. Validation runs before any fixture write, so
//! model-only fields cannot silently change the meaning of a replay.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rustix::fs::{AtFlags, CWD, StatxFlags};

use diskgc_model::host::CACHEDIR_TAG_CONTENT;
use diskgc_model::profile_vector::ProfileVector;
use diskgc_model::spec::{
    Bytes, FileSpec, GateSpec, HostModel, LockModeSpec, Mode, NodeSpec, ProcAccess, ProcessSpec,
    Special, WallTime,
};

use crate::holder::Holder;
use crate::scratch::{self, Scratch};
use crate::snapshot;
use crate::suite::Ctx;
use crate::{Owner, TESTER};

const MAX_FILE_SIZE: usize = 64 * 1024 * 1024;
pub(crate) const SCRATCH_PROBE_EXE: &str = "/home/tester/differential-process-probe";
const IMAGE_BUILD_PROBE_EXE: &str = "/usr/bin/buildah";

/// A checked outcome replay projects away the model's nominal clock and
/// mount geometry. Only a complete, independently checked stage plan can
/// issue one of these values. Callers still compare Linux and model outcomes.
pub struct OutcomeProjection<'a> {
    model: &'a HostModel,
}

/// All staged fixture writes are declared before the first write. Exactly
/// one stage owns each modelled tree node; the last owns processes and locks.
pub struct OutcomeStagePlan<'a> {
    _checked_vector: &'a ProfileVector,
    stages: Vec<&'a HostModel>,
    next_index: usize,
}

impl<'a> OutcomeStagePlan<'a> {
    pub fn checked(vector: &'a ProfileVector, stages: Vec<&'a HostModel>) -> Result<Self, String> {
        let library = diskgc_core::profile::Library::builtin()
            .map_err(|error| format!("outcome projection library: {error}"))?;
        vector.check(&library)?;
        validate_stage_partition(&vector.host, &stages)?;
        Ok(Self {
            _checked_vector: vector,
            stages,
            next_index: 0,
        })
    }

    pub fn next_stage(&mut self) -> Result<OutcomeProjection<'a>, String> {
        let index = self.next_index;
        let model = self
            .stages
            .get(index)
            .copied()
            .ok_or_else(|| format!("outcome projection stage {index} is absent"))?;
        self.next_index += 1;
        Ok(OutcomeProjection { model })
    }
}

fn validate_stage_partition(expected: &HostModel, stages: &[&HostModel]) -> Result<(), String> {
    if stages.is_empty() {
        return Err("outcome projection has no materialization stages".into());
    }
    let mut seen = BTreeSet::new();
    for (index, stage) in stages.iter().enumerate() {
        let mut nonstage_fields = (**stage).clone();
        nonstage_fields.tree = expected.tree.clone();
        nonstage_fields.processes = expected.processes.clone();
        nonstage_fields.locks = expected.locks.clone();
        if nonstage_fields != *expected {
            return Err(format!(
                "outcome projection stage {index} changes fields beyond the checked tree/process cohort"
            ));
        }
        for (path, node) in &stage.tree {
            if expected.tree.get(path) != Some(node) {
                return Err(format!(
                    "outcome projection stage {index} contains node {} absent from the checked vector",
                    path.display()
                ));
            }
            if !seen.insert(path.clone()) {
                return Err(format!(
                    "outcome projection duplicates node {} across stages",
                    path.display()
                ));
            }
        }
        let final_stage = index + 1 == stages.len();
        if final_stage {
            if stage.processes != expected.processes {
                return Err(
                    "outcome projection final stage omits or changes checked processes".into(),
                );
            }
            if stage.locks != expected.locks {
                return Err("outcome projection final stage omits or changes checked locks".into());
            }
        } else if !stage.processes.is_empty() || !stage.locks.is_empty() {
            return Err(format!(
                "outcome projection stage {index} starts checked processes or locks before the final stage"
            ));
        }
    }
    if let Some(missing) = expected.tree.keys().find(|path| !seen.contains(*path)) {
        return Err(format!(
            "outcome projection omits checked node {} from all stages",
            missing.display()
        ));
    }
    Ok(())
}

fn mapped_path(line: &str) -> Option<&Path> {
    line.split_whitespace().nth(5).map(Path::new)
}

/// Fixture helpers stay alive until both LinuxHost runs and the deletion
/// oracle finish. Dropping this guard closes their stdin and waits for them.
pub struct Materialized {
    holders: Vec<HeldProcess>,
    mounted: Vec<PathBuf>,
}

struct HeldProcess {
    holder: Holder,
    path: PathBuf,
    lock_mode: Option<LockModeSpec>,
    exe: Option<PathBuf>,
}

/// A second open file description must see the kernel conflict appropriate
/// to the declared mode. A shared holder admits another shared lock; an
/// exclusive holder admits neither kind.
fn lock_conflicts(path: &Path, op: rustix::fs::FlockOperation) -> Result<bool, String> {
    let file = fs::File::open(path).map_err(|error| {
        format!(
            "open held lock {} for verification: {error}",
            path.display()
        )
    })?;
    match rustix::fs::flock(&file, op) {
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Ok(()) => Ok(false),
        Err(error) => Err(format!(
            "verify lock {} with {op:?}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn verify_lock_mode(path: &Path, mode: LockModeSpec) -> Result<(), String> {
    use rustix::fs::FlockOperation::{NonBlockingLockExclusive, NonBlockingLockShared};
    if !lock_conflicts(path, NonBlockingLockExclusive)? {
        return Err(format!("{} has no conflicting kernel lock", path.display()));
    }
    let shared_conflicts = lock_conflicts(path, NonBlockingLockShared)?;
    if shared_conflicts != (mode == LockModeSpec::Exclusive) {
        return Err(format!(
            "{} kernel lock mode disagrees with model {:?}: shared probe {}",
            path.display(),
            mode,
            if shared_conflicts {
                "conflicted"
            } else {
                "succeeded"
            }
        ));
    }
    Ok(())
}

impl Materialized {
    /// Verify the kernel still exposes each promised open file. A helper
    /// that exits between planning and reclaim would make an in-use test
    /// vacuous; this check makes that failure explicit.
    pub fn assert_holding(&self) -> Result<(), String> {
        for held in &self.holders {
            let fd_dir = format!("/proc/{}/fd", held.holder.pid());
            let entries =
                fs::read_dir(&fd_dir).map_err(|error| format!("list helper {fd_dir}: {error}"))?;
            let mut found = false;
            for entry in entries {
                let entry = entry.map_err(|error| format!("read helper {fd_dir}: {error}"))?;
                let target = fs::read_link(entry.path()).map_err(|error| {
                    format!("read helper fd {}: {error}", entry.path().display())
                })?;
                if target == held.path {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(format!(
                    "helper pid {} no longer holds {} open",
                    held.holder.pid(),
                    held.path.display()
                ));
            }
            if let Some(mode) = held.lock_mode {
                verify_lock_mode(&held.path, mode)
                    .map_err(|error| format!("helper pid {}: {error}", held.holder.pid()))?;
            }
            if let Some(exe) = &held.exe {
                let link =
                    fs::read_link(format!("/proc/{}/exe", held.holder.pid())).map_err(|error| {
                        format!("read helper pid {} exe: {error}", held.holder.pid())
                    })?;
                if &link != exe {
                    return Err(format!(
                        "helper pid {} exe {} differs from model {}",
                        held.holder.pid(),
                        link.display(),
                        exe.display()
                    ));
                }
                let maps = fs::read_to_string(format!("/proc/{}/maps", held.holder.pid()))
                    .map_err(|error| {
                        format!("read helper pid {} maps: {error}", held.holder.pid())
                    })?;
                let file_maps: BTreeSet<_> = maps
                    .lines()
                    .filter_map(mapped_path)
                    .filter(|path| path.is_absolute())
                    .collect();
                if file_maps != BTreeSet::from([exe.as_path()]) {
                    return Err(format!(
                        "helper pid {} file-backed maps {:?} differ from model executable {}",
                        held.holder.pid(),
                        file_maps,
                        exe.display()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Negative evidence for an active directory: every visible process
    /// reference must stay outside it before and after each diskgc run.
    pub fn assert_no_reference_to(&self, candidate: &Path) -> Result<(), String> {
        for held in &self.holders {
            assert_process_no_reference_to(held.holder.pid(), candidate)?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<(), String> {
        for held in self.holders.drain(..) {
            held.holder.finish()?;
        }
        while let Some(path) = self.mounted.last() {
            rustix::mount::unmount(path, rustix::mount::UnmountFlags::empty())
                .map_err(|error| format!("unmount fixture {}: {error}", path.display()))?;
            self.mounted.pop();
        }
        Ok(())
    }
}

/// Inspect one real helper so a negative-process fixture cannot pass by
/// silently losing its intended absence of candidate references.
pub(crate) fn assert_process_no_reference_to(pid: u32, candidate: &Path) -> Result<(), String> {
    let fd_dir = format!("/proc/{pid}/fd");
    for entry in fs::read_dir(&fd_dir).map_err(|error| format!("list {fd_dir}: {error}"))? {
        let entry = entry.map_err(|error| format!("read {fd_dir}: {error}"))?;
        let path = fs::read_link(entry.path())
            .map_err(|error| format!("read {}: {error}", entry.path().display()))?;
        if path.starts_with(candidate) {
            return Err(format!(
                "helper pid {pid} fd {} references {} inside {}",
                entry.file_name().to_string_lossy(),
                path.display(),
                candidate.display()
            ));
        }
    }
    for name in ["cwd", "root", "exe"] {
        let path = fs::read_link(format!("/proc/{pid}/{name}"))
            .map_err(|error| format!("read helper pid {pid} {name}: {error}"))?;
        if path.starts_with(candidate) {
            return Err(format!(
                "helper pid {pid} {name} {} references {}",
                path.display(),
                candidate.display()
            ));
        }
    }
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))
        .map_err(|error| format!("read helper pid {pid} maps: {error}"))?;
    for line in maps.lines() {
        if mapped_path(line).is_some_and(|path| path.starts_with(candidate)) {
            return Err(format!(
                "helper pid {pid} maps entry names {}: {line}",
                candidate.display()
            ));
        }
    }
    let environ = fs::read(format!("/proc/{pid}/environ"))
        .map_err(|error| format!("read helper pid {pid} environ: {error}"))?;
    let candidate_text = candidate
        .to_str()
        .ok_or("non-UTF-8 candidate in process reference check")?;
    for entry in environ.split(|byte| *byte == 0) {
        if entry
            .windows(candidate_text.len())
            .any(|window| window == candidate_text.as_bytes())
        {
            return Err(format!(
                "helper pid {pid} initial environment names {}",
                candidate.display()
            ));
        }
    }
    Ok(())
}

impl Drop for Materialized {
    fn drop(&mut self) {
        // Helpers may hold files on an inner mount. Close them first, even
        // when an earlier check returned an error.
        self.holders.clear();
        while let Some(path) = self.mounted.pop() {
            if let Err(error) = rustix::mount::unmount(&path, rustix::mount::UnmountFlags::empty())
            {
                eprintln!(
                    "error: unmount fixture {} during cleanup: {error}",
                    path.display()
                );
            }
        }
    }
}

/// A model field that cannot be represented by this container replay.
/// Returns the field path, rather than silently skipping the scenario.
pub fn validate(model: &HostModel) -> Result<(), String> {
    let unsupported = |field: &str, reason: &str| {
        Err(format!(
            "materialize(model): {field} is model-only: {reason}"
        ))
    };
    if model.kernel.is_some() {
        return unsupported("kernel", "the running kernel version is fixed");
    }
    if model.hostname.is_some() {
        return unsupported("hostname", "the container hostname is fixed");
    }
    if model.own_pid.is_some()
        || model.own_uid_map.is_some()
        || model.own_uid.is_some_and(|uid| uid != TESTER.uid)
    {
        return unsupported(
            "own_pid/own_uid_map/own_uid",
            "the traced tester process supplies these values",
        );
    }
    if model.pid_namespace != diskgc_model::spec::PidNamespaceSpec::Initial || model.proc_hidepid {
        return unsupported(
            "pid_namespace/proc_hidepid",
            "the container's own PID namespace and /proc mount are what they are",
        );
    }
    if model.sandbox_gate != GateSpec::Open {
        return unsupported("sandbox_gate", "the container interlock supplies the gate");
    }
    if model.command_exit != 0
        || model.command_error_after_start
        || model.docker != Default::default()
    {
        return unsupported(
            "command/docker",
            "scripted command responses have no kernel equivalent",
        );
    }
    if !model.events.is_empty() {
        return unsupported(
            "events",
            "virtual call-time races cannot be replayed as a fixture",
        );
    }
    if model.locks.len() > 1 {
        return unsupported("locks", "only one process-owned lock is supported");
    }
    if let Some(lock) = model.locks.first()
        && !model
            .processes
            .iter()
            .any(|process| process.pid == lock.holder)
    {
        return unsupported("locks[0].holder", "the holder must be a modelled process");
    }
    for (index, process) in model.processes.iter().enumerate() {
        let lock = model.locks.iter().find(|lock| lock.holder == process.pid);
        validate_process(index, process, lock.is_some())?;
        if process.exe.as_deref() == Some(Path::new(SCRATCH_PROBE_EXE)) {
            if model.processes.len() != 1 {
                return unsupported("processes", "the copied probe shape requires one process");
            }
            let Some(NodeSpec::File(file)) = model.tree.get(Path::new(SCRATCH_PROBE_EXE)) else {
                return unsupported(
                    "processes[0].exe",
                    "the copied probe executable must be a modelled scratch file",
                );
            };
            if file.symlink.is_some() || file.link.is_some() || file.special.is_some() {
                return unsupported(
                    "processes[0].exe",
                    "the copied probe executable must be a regular file",
                );
            }
            if file.content.is_some() || file.mode != Some(Mode(0o755)) {
                return unsupported(
                    "processes[0].exe",
                    "the copied probe needs an executable file model with mode 0755 and no synthetic content",
                );
            }
        }
    }
    if let Some(lock) = model.locks.first() {
        destination(&lock.path).map_err(|_| {
            "materialize(model): locks[0].path is model-only: only scratch files can be locked"
                .to_owned()
        })?;
    }
    if model.mounts.is_empty() {
        return unsupported("mounts", "a root mount is required");
    }
    let mut inner_at: Option<&Path> = None;
    let mut previous_inner_id = None;
    for (index, mount) in model.mounts.iter().enumerate() {
        let field = format!("mounts[{index}]");
        let source = mount.bind_of.and_then(|source| {
            model.mounts[..index]
                .iter()
                .find(|prior| prior.id == source)
        });
        if let Some(source) = source {
            if !matches!(
                source.at.to_str(),
                Some("/tmp" | "/var/tmp" | "/home/tester")
            ) || source.dev != mount.dev
                || source.fs != mount.fs
                || source.size != mount.size
                || source.other_used != mount.other_used
                || source.statx_missing != mount.statx_missing
            {
                return unsupported(
                    &field,
                    "only matching scratch filesystem-root binds are supported",
                );
            }
        } else if mount.bind_of.is_some()
            || model.mounts[..index]
                .iter()
                .any(|prior| prior.dev == mount.dev)
        {
            return unsupported(
                &field,
                "repeated devices require bind_of an earlier scratch root",
            );
        }
        if !mount.statx_missing.is_empty() {
            return unsupported(
                &format!("{field}.statx_missing"),
                "the kernel chooses its statx mask",
            );
        }
        if mount.other_used.0 != 0 {
            return unsupported(
                &format!("{field}.other_used"),
                "exact statvfs accounting cannot be recreated",
            );
        }
        if mount.read_only {
            return unsupported(
                &format!("{field}.read_only"),
                "the harness's filesystem is writable",
            );
        }
        if mount.at == Path::new("/") {
            if !matches!(mount.fs.as_str(), "ext4" | "overlay") {
                return unsupported(
                    &format!("{field}.fs"),
                    "the container root is an overlay; only a nominal ext4/overlay root is supported",
                );
            }
            continue; // The container's root filesystem is provisioned by the image.
        }
        if mount.fs != "tmpfs" {
            return unsupported(
                &format!("{field}.fs"),
                "only scratch tmpfs mounts are supported",
            );
        }
        if matches!(
            mount.at.to_str(),
            Some("/tmp" | "/var/tmp" | "/home/tester" | "/run/diskgc")
        ) {
            if mount.bind_of.is_some() {
                return unsupported(&field, "binding over a harness scratch root is unsupported");
            }
            if model.mounts[..index]
                .iter()
                .any(|prior| prior.at == mount.at)
            {
                return unsupported(&field, "stacking over a harness root is unsupported");
            }
            continue; // The harness provisions its base scratch mounts.
        }
        let (root, _) = destination(&mount.at)?;
        if let Some(source) = source
            && source.at != Path::new(root)
        {
            return unsupported(&field, "bind source and target must share one scratch root");
        }
        let base = model.mounts[..index]
            .iter()
            .find(|candidate| candidate.at == Path::new(root));
        let Some(base) = base else {
            return unsupported(
                &field,
                "the containing scratch root is absent from the model",
            );
        };
        match inner_at {
            None => {
                if mount.parent != Some(base.id) {
                    return unsupported(&field, "inner tmpfs must descend from its scratch root");
                }
                inner_at = Some(&mount.at);
            }
            Some(at) => {
                if mount.at != at || mount.parent != previous_inner_id {
                    return unsupported(
                        &field,
                        "only one inner mount point with an optional stack is supported",
                    );
                }
            }
        }
        previous_inner_id = Some(mount.id);
    }
    for (path, node) in &model.tree {
        if path == Path::new("/run/diskgc/lock") {
            let expected = NodeSpec::File(FileSpec {
                uid: Some(0),
                gid: Some(0),
                ..FileSpec::default()
            });
            if node != &expected {
                return Err("materialize(model): /run/diskgc/lock must match the harness-provisioned root-owned regular lock".into());
            }
            continue; // The container image provisions this root-owned lock.
        }
        if path == Path::new(IMAGE_BUILD_PROBE_EXE) {
            if node != &NodeSpec::File(FileSpec::default())
                || !model
                    .processes
                    .iter()
                    .any(|process| process.exe.as_deref() == Some(path))
            {
                return unsupported(
                    "tree./usr/bin/buildah",
                    "only the VM image's named process probe is supported",
                );
            }
            continue;
        }
        let _ = destination(path)?;
        validate_node(path, node)?;
        if let Some(at) = inner_at {
            validate_mount_tree_boundary(path, node, at)?;
        }
    }
    if let Some(lock) = model.locks.first() {
        let (state, _) = diskgc_model::load::load(model)
            .map_err(|error| format!("materialize(model): locks[0] does not load: {error}"))?;
        let loc = state.lookup(&lock.path).map_err(|error| {
            format!("materialize(model): locks[0].path is not reachable: {error:?}")
        })?;
        let inode = state.inode(loc);
        if !matches!(&inode.kind, diskgc_model::state::Kind::File { .. })
            || inode.uid != TESTER.uid
            || inode.perm & 0o600 != 0o600
        {
            return unsupported(
                "locks[0].path",
                "the helper needs an existing tester-owned readable and writable regular file",
            );
        }
    }
    Ok(())
}

fn validate_mount_tree_boundary(
    path: &Path,
    node: &NodeSpec,
    mount_at: &Path,
) -> Result<(), String> {
    if path == mount_at {
        return Err(format!(
            "materialize(model): {} is model-only: explicit mount-root attributes are unsupported",
            path.display()
        ));
    }
    if let NodeSpec::Dir(dir) = node {
        for (name, child) in &dir.entries {
            let child_path = path.join(name);
            if path.starts_with(mount_at) != child_path.starts_with(mount_at) {
                return Err(format!(
                    "materialize(model): {} crosses an inner mount inside a directory node; use a separate tree key",
                    child_path.display()
                ));
            }
            validate_mount_tree_boundary(&child_path, child, mount_at)?;
        }
    } else if let NodeSpec::File(file) = node
        && file.symlink.is_some()
        && mount_at.starts_with(path)
    {
        return Err(format!(
            "materialize(model): {} is model-only: a symlink cannot be an inner mount ancestor",
            path.display()
        ));
    }
    Ok(())
}

fn validate_process(index: usize, process: &ProcessSpec, locked: bool) -> Result<(), String> {
    let field = format!("processes[{index}]");
    let unsupported = |name: &str, reason: &str| {
        Err(format!(
            "materialize(model): {field}.{name} is model-only: {reason}"
        ))
    };
    if process.proc != ProcAccess::Readable {
        return unsupported("proc", "the ready helper must be inspectable");
    }
    let copied_probe = process.exe.as_deref() == Some(Path::new(SCRATCH_PROBE_EXE))
        && !locked
        && process.maps == [PathBuf::from(SCRATCH_PROBE_EXE)]
        && process.environ.len() == 1
        && process.environ[0] == "PATH=/bin"
        && process.cwd.is_none();
    let image_build_probe = process.exe.as_deref() == Some(Path::new(IMAGE_BUILD_PROBE_EXE))
        && !locked
        && process.maps == [PathBuf::from(IMAGE_BUILD_PROBE_EXE)]
        && process.environ == ["PATH=/usr/bin"]
        && process.cwd.as_deref() == Some(Path::new("/"));
    if !copied_probe
        && !image_build_probe
        && (process.exe.is_some() || !process.maps.is_empty() || !process.environ.is_empty())
    {
        return unsupported(
            "exe/maps/environ",
            "only the copied scratch probe with one executable mapping and PATH=/bin is supported",
        );
    }
    if process.root.is_some()
        || process.name.is_some()
        || process.uid.is_some_and(|uid| uid != TESTER.uid)
        || process.euid.is_some_and(|uid| uid != TESTER.uid)
        || process.gid.is_some_and(|gid| gid != TESTER.gid)
        || process.non_dumpable
        || process.uid_map.is_some()
        || process.privileged
    {
        return unsupported(
            "root/name/ids/dumpability/uid_map/privileged",
            "the tester helper has fixed process credentials and namespace",
        );
    }
    if process.nspid.is_some() || process.zombie {
        return unsupported(
            "nspid/zombie",
            "the helper is one live process in the container's PID namespace",
        );
    }
    if process.cmdline.is_some() || !process.view.is_empty() {
        return unsupported(
            "cmdline/view",
            "the helper's command line and mount view are fixed by the harness",
        );
    }
    if (!locked && process.fds.len() != 1)
        || (locked && !process.fds.is_empty())
        || process.fds.iter().any(|fd| fd.deleted || fd.fd.is_some())
    {
        return unsupported(
            "fds",
            "the helper holds exactly one named file, either from fds or one lock, at a kernel-selected fd",
        );
    }
    if let Some(fd) = process.fds.first() {
        destination(&fd.path)?;
    }
    if let Some(cwd) = &process.cwd {
        if locked {
            return unsupported(
                "cwd",
                "the flock helper runs at / and cannot reproduce a custom cwd",
            );
        }
        if !(image_build_probe && cwd == Path::new("/")) {
            destination(cwd)?;
        }
    }
    Ok(())
}

fn validate_node(path: &Path, node: &NodeSpec) -> Result<(), String> {
    match node {
        NodeSpec::Dir(dir) => {
            if let Some(mtime) = dir.attrs.mtime {
                checked_mtime(mtime, path)?;
            }
            for (name, child) in &dir.entries {
                if name == "." || name == ".." || name.contains('/') {
                    return Err(format!(
                        "materialize(model): invalid entry {name:?} below {}",
                        path.display()
                    ));
                }
                validate_node(&path.join(name), child)?;
            }
        }
        NodeSpec::File(file) => {
            if let Some(mtime) = file.mtime {
                if file.symlink.is_some() {
                    return Err(format!(
                        "materialize(model): {}.mtime on a symlink is model-only",
                        path.display()
                    ));
                }
                checked_mtime(mtime, path)?;
            }
            if file.link.is_some() {
                return Err(format!(
                    "materialize(model): {}.link is model-only: hard-link replay is not implemented",
                    path.display()
                ));
            }
            if file.bound {
                return Err(format!(
                    "materialize(model): {}.bound is model-only: a listener lifetime is required",
                    path.display()
                ));
            }
            if file.special == Some(Special::CharDevice) {
                return Err(format!(
                    "materialize(model): {}.type char_device is model-only: scratch tmpfs is nodev",
                    path.display()
                ));
            }
            if file.symlink.is_some()
                && (file.size.is_some() || file.content.is_some() || file.special.is_some())
            {
                return Err(format!(
                    "materialize(model): {} has conflicting symlink attributes",
                    path.display()
                ));
            }
            if file.symlink.is_some() && file.mode.is_some() {
                return Err(format!(
                    "materialize(model): {}.mode on a symlink is model-only",
                    path.display()
                ));
            }
            if file.special == Some(Special::Socket)
                && file.mode.is_some_and(|mode| mode.0 != 0o755)
            {
                return Err(format!(
                    "materialize(model): {}.mode on a socket is model-only: the harness binds sockets with mode 0755",
                    path.display()
                ));
            }
            if file.special.is_some() && (file.size.is_some() || file.content.is_some()) {
                return Err(format!(
                    "materialize(model): {} has conflicting special-file attributes",
                    path.display()
                ));
            }
            if file.cachedir_tag
                && (file.content.is_some() || file.special.is_some() || file.symlink.is_some())
            {
                return Err(format!(
                    "materialize(model): {} has conflicting CACHEDIR.TAG attributes",
                    path.display()
                ));
            }
            let content_len = if file.cachedir_tag {
                CACHEDIR_TAG_CONTENT.len()
            } else {
                file.content.as_deref().unwrap_or("").len()
            };
            let size = file.size.map_or(content_len, |size| size.0 as usize);
            if size > MAX_FILE_SIZE || size < content_len {
                return Err(format!(
                    "materialize(model): {} size {size} is outside supported range",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn destination(path: &Path) -> Result<(&'static str, &Path), String> {
    for root in ["/tmp", "/var/tmp", "/home/tester"] {
        if let Ok(relative) = path.strip_prefix(root)
            && relative.components().next().is_some()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return Ok((root, relative));
        }
    }
    Err(format!(
        "materialize(model): {} lies outside the container scratch mounts",
        path.display()
    ))
}

fn scratch_for<'a>(ctx: &'a Ctx, root: &str) -> &'a Scratch {
    match root {
        "/tmp" => &ctx.scratch,
        "/var/tmp" => &ctx.var_tmp,
        "/home/tester" => &ctx.home,
        _ => unreachable!("destination only returns known scratch roots"),
    }
}

/// A literal host model includes an exact clock instant, which this harness
/// cannot hold constant while building the fixture. Refuse it before writes.
pub fn materialize(ctx: &Ctx, model: &HostModel) -> Result<Materialized, String> {
    validate(model)?;
    validate_literal_nominal(model)?;
    materialize_validated(ctx, model)
}

/// Materialize only the filesystem/process shape of a checked outcome vector.
/// The caller must independently check its expectation and compare the model
/// and Linux outcomes; model clock and mount geometry remain projected away.
pub fn materialize_outcome_projection(
    ctx: &Ctx,
    projection: OutcomeProjection<'_>,
) -> Result<Materialized, String> {
    validate(projection.model)?;
    materialize_validated(ctx, projection.model)
}

fn validate_literal_nominal(model: &HostModel) -> Result<(), String> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("read mountinfo before materialize: {error}"))?;
    let real_mounts = snapshot::parse_mountinfo(&mountinfo)?;
    for (index, mount) in model.mounts.iter().enumerate() {
        let field = format!("mounts[{index}]");
        if !matches!(
            mount.at.to_str(),
            Some("/" | "/tmp" | "/var/tmp" | "/home/tester" | "/run/diskgc")
        ) {
            return Err(format!(
                "materialize(model): {field}.id is model-only: the kernel assigns an inner mount ID only when that mount is created"
            ));
        }
        let stat = rustix::fs::statx(
            CWD,
            &mount.at,
            AtFlags::SYMLINK_NOFOLLOW,
            StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
        )
        .map_err(|error| format!("statx {} before materialize: {error}", mount.at.display()))?;
        if stat.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
            return Err(format!(
                "materialize(model): {field}.id is model-only: kernel did not report a mount ID"
            ));
        }
        if mount.id != stat.stx_mnt_id {
            return Err(format!(
                "materialize(model): {field}.id is model-only: model {}, kernel {}",
                mount.id, stat.stx_mnt_id
            ));
        }
        let real_mount = real_mounts
            .iter()
            .find(|real| real.mount_id == stat.stx_mnt_id)
            .ok_or_else(|| {
                format!(
                    "materialize(model): {field}.id is model-only: kernel mount {} is absent from mountinfo",
                    stat.stx_mnt_id
                )
            })?;
        if real_mount.mount_point != mount.at {
            return Err(format!(
                "materialize(model): {field}.at is model-only: kernel mount {} is at {}, not {}",
                mount.id,
                real_mount.mount_point.display(),
                mount.at.display()
            ));
        }
        if mount.fs != real_mount.fs_type {
            return Err(format!(
                "materialize(model): {field}.fs is model-only: model {}, kernel {}",
                mount.fs, real_mount.fs_type
            ));
        }
        if mount.at == Path::new("/") && mount.parent.is_some() {
            return Err(format!(
                "materialize(model): {field}.parent is model-only: the container root has no visible parent"
            ));
        }
        if mount.at != Path::new("/") && mount.parent != Some(real_mount.parent_id) {
            return Err(format!(
                "materialize(model): {field}.parent is model-only: model {:?}, kernel {}",
                mount.parent, real_mount.parent_id
            ));
        }
        if mount.dev.0.major != stat.stx_dev_major || mount.dev.0.minor != stat.stx_dev_minor {
            return Err(format!(
                "materialize(model): {field}.dev is model-only: model {}, kernel {}:{}",
                mount.dev, stat.stx_dev_major, stat.stx_dev_minor
            ));
        }
        let vfs = rustix::fs::statvfs(&mount.at).map_err(|error| {
            format!("statvfs {} before materialize: {error}", mount.at.display())
        })?;
        let real_size = vfs.f_blocks.checked_mul(vfs.f_frsize).ok_or_else(|| {
            format!("materialize(model): {field}.size is model-only: statvfs size overflow")
        })?;
        if mount.size.0 != real_size {
            return Err(format!(
                "materialize(model): {field}.size is model-only: model {}, kernel {real_size}",
                mount.size.0
            ));
        }
    }
    Err(format!(
        "materialize(model): clock is model-only: the exact model instant {} cannot be held constant while writing a fixture",
        model.clock
    ))
}

fn materialize_validated(ctx: &Ctx, model: &HostModel) -> Result<Materialized, String> {
    let probe_bytes = if model
        .processes
        .iter()
        .any(|process| process.exe.as_deref() == Some(Path::new(SCRATCH_PROBE_EXE)))
    {
        let bytes = fs::read(&ctx.self_exe).map_err(|error| {
            format!(
                "read copied fixture probe {}: {error}",
                ctx.self_exe.display()
            )
        })?;
        let Some(NodeSpec::File(file)) = model.tree.get(Path::new(SCRATCH_PROBE_EXE)) else {
            return Err(
                "materialize(model): copied probe file disappeared after validation".into(),
            );
        };
        if file.size != Some(Bytes(bytes.len() as u64)) {
            return Err(format!(
                "materialize(model): copied probe model size {:?} differs from binary size {}",
                file.size,
                bytes.len()
            ));
        }
        Some(bytes)
    } else {
        None
    };
    if model
        .processes
        .iter()
        .any(|process| process.exe.as_deref() == Some(Path::new(IMAGE_BUILD_PROBE_EXE)))
    {
        let probe = fs::metadata(IMAGE_BUILD_PROBE_EXE)
            .map_err(|error| format!("VM image process probe {IMAGE_BUILD_PROBE_EXE}: {error}"))?;
        if !probe.is_file() || probe.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "VM image process probe {IMAGE_BUILD_PROBE_EXE} is not an executable regular file"
            ));
        }
    }
    let inner_at = model
        .mounts
        .iter()
        .find(|mount| {
            mount.at != Path::new("/")
                && !matches!(
                    mount.at.to_str(),
                    Some("/tmp" | "/var/tmp" | "/home/tester" | "/run/diskgc")
                )
        })
        .map(|mount| mount.at.as_path());
    let mut materialized = Materialized {
        holders: Vec::new(),
        mounted: Vec::new(),
    };
    // Populate the parent scratch mounts before covering the mount point.
    for (path, node) in &model.tree {
        if path == Path::new("/run/diskgc/lock")
            || path == Path::new(IMAGE_BUILD_PROBE_EXE)
            || (probe_bytes.is_some() && path == Path::new(SCRATCH_PROBE_EXE))
            || inner_at.is_some_and(|at| path.starts_with(at))
        {
            continue;
        }
        let (root, relative) = destination(path)?;
        let scratch = scratch_for(ctx, root);
        create_node(scratch, relative, node)?;
    }
    if let Some(bytes) = probe_bytes {
        ctx.home
            .write_file("differential-process-probe", &bytes, 0o755, TESTER)?;
    }
    let mut inner_scratch = None;
    if let Some(at) = inner_at {
        let (root, relative) = destination(at)?;
        let parent_scratch = scratch_for(ctx, root);
        parent_scratch.mkdirs(
            relative
                .to_str()
                .ok_or("materialize(model): non-UTF-8 mount point")?,
            0o755,
            TESTER,
        )?;
        for mount in model.mounts.iter().filter(|mount| mount.at == at) {
            let scratch = if let Some(source) = mount.bind_of {
                let source_mount = model
                    .mounts
                    .iter()
                    .find(|prior| prior.id == source)
                    .ok_or_else(|| format!("materialize(model): bind_of {source} missing"))?;
                let source_scratch = scratch_for(
                    ctx,
                    source_mount.at.to_str().ok_or("non-UTF-8 scratch root")?,
                );
                if source_scratch.root() != Path::new(root) {
                    return Err(format!(
                        "materialize(model): bind source {} and target {} must share a scratch root",
                        source_mount.at.display(),
                        at.display()
                    ));
                }
                source_scratch.bind_mount_root(
                    relative
                        .to_str()
                        .ok_or("materialize(model): non-UTF-8 bind target")?,
                )?;
                materialized.mounted.push(at.to_owned());
                Scratch::attach(
                    at.to_str()
                        .ok_or("materialize(model): non-UTF-8 bind target")?,
                )?
            } else {
                Scratch::mount(
                    at.to_str()
                        .ok_or("materialize(model): non-UTF-8 mount point")?,
                    "0755",
                    TESTER,
                )
                .map_err(|error| format!("materialize(model): mount id {}: {error}", mount.id))?
            };
            if mount.bind_of.is_none() {
                materialized.mounted.push(at.to_owned());
            }
            inner_scratch = Some(scratch);
        }
        let scratch = inner_scratch
            .as_ref()
            .ok_or("materialize(model): inner mount was not created")?;
        for (path, node) in &model.tree {
            if path.starts_with(at) {
                let relative = path
                    .strip_prefix(at)
                    .map_err(|error| format!("inner tree path {}: {error}", path.display()))?;
                create_node(scratch, relative, node)?;
            }
        }
    }
    // Creating children changes parent mtimes. Apply explicit times only
    // after the complete tree exists, with descendants before their parents.
    for (path, node) in &model.tree {
        if path == Path::new("/run/diskgc/lock") {
            continue;
        }
        if path == Path::new(IMAGE_BUILD_PROBE_EXE) {
            continue;
        }
        let (scratch, relative) = if let Some(at) = inner_at.filter(|at| path.starts_with(at)) {
            (
                inner_scratch
                    .as_ref()
                    .ok_or("materialize(model): inner mount missing for mtime")?,
                path.strip_prefix(at).map_err(|error| error.to_string())?,
            )
        } else {
            let (root, relative) = destination(path)?;
            (scratch_for(ctx, root), relative)
        };
        apply_mtimes(scratch, relative, node)?;
    }
    for process in &model.processes {
        let lock = model.locks.iter().find(|lock| lock.holder == process.pid);
        let path = if let Some(lock) = lock {
            &lock.path
        } else {
            &process.fds[0].path
        };
        let path_text = path
            .to_str()
            .ok_or("materialize(model): non-UTF-8 process fd path")?;
        let cwd_text = process
            .cwd
            .as_deref()
            .unwrap_or(Path::new("/"))
            .to_str()
            .ok_or("materialize(model): non-UTF-8 process cwd")?;
        let args: Vec<&str> = match lock.map(|lock| lock.mode) {
            Some(LockModeSpec::Shared) => vec!["flock-hold-existing", "--shared", path_text],
            Some(LockModeSpec::Exclusive) => vec!["flock-hold-existing", path_text],
            None => vec!["hold", path_text, cwd_text],
        };
        let holder = if let Some(exe) = &process.exe {
            if exe == Path::new(IMAGE_BUILD_PROBE_EXE) {
                Holder::tester_program_env(ctx, exe, &args, &[("PATH", "/usr/bin")])
            } else {
                Holder::tester_program(ctx, exe, &args)
            }
        } else {
            Holder::tester(ctx, &args, &[])
        }
        .map_err(|error| {
            format!(
                "materialize(model): helper for pid {}: {error}",
                process.pid
            )
        })?;
        materialized.holders.push(HeldProcess {
            holder,
            path: path.clone(),
            lock_mode: lock.map(|lock| lock.mode),
            exe: process.exe.clone(),
        });
    }
    materialized.assert_holding()?;
    Ok(materialized)
}

fn checked_mtime(mtime: WallTime, path: &Path) -> Result<std::time::SystemTime, String> {
    let seconds = u64::try_from(mtime.0.as_second()).map_err(|_| {
        format!(
            "materialize(model): {}.mtime before the Unix epoch is unsupported",
            path.display()
        )
    })?;
    let nanos = u32::try_from(mtime.0.subsec_nanosecond()).map_err(|_| {
        format!(
            "materialize(model): {}.mtime has negative subsecond part",
            path.display()
        )
    })?;
    Ok(UNIX_EPOCH + Duration::new(seconds, nanos))
}

fn apply_mtimes(scratch: &Scratch, relative: &Path, node: &NodeSpec) -> Result<(), String> {
    let mtime = match node {
        NodeSpec::Dir(dir) => {
            for (name, child) in &dir.entries {
                apply_mtimes(scratch, &relative.join(name), child)?;
            }
            dir.attrs.mtime
        }
        NodeSpec::File(file) => file.mtime,
    };
    let Some(mtime) = mtime else { return Ok(()) };
    let rel = relative
        .to_str()
        .ok_or("materialize(model): non-UTF-8 mtime path")?;
    let path = scratch.path(rel);
    if scratch::mount_id(&path, false)? != scratch.mnt_id() {
        return Err(format!(
            "materialize(model): refusing to set mtime outside scratch mount at {}",
            path.display()
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| format!("open {} to set mtime: {error}", path.display()))?;
    file.set_modified(checked_mtime(mtime, &path)?)
        .map_err(|error| format!("set mtime on {}: {error}", path.display()))
}

fn create_node(scratch: &Scratch, relative: &Path, node: &NodeSpec) -> Result<(), String> {
    let rel = relative
        .to_str()
        .ok_or("materialize(model): non-UTF-8 path")?;
    match node {
        NodeSpec::Dir(dir) => {
            let owner = Owner {
                uid: dir.attrs.uid.unwrap_or(TESTER.uid),
                gid: dir.attrs.gid.unwrap_or(TESTER.gid),
            };
            let mode = dir.attrs.mode.map_or(0o755, |mode| mode.0);
            scratch.mkdirs(rel, 0o755, TESTER)?;
            scratch.chown(rel, owner)?;
            scratch.chmod(rel, mode)?;
            for (name, child) in &dir.entries {
                create_node(scratch, &relative.join(name), child)?;
            }
        }
        NodeSpec::File(file) => create_file(scratch, rel, file)?,
    }
    Ok(())
}

fn create_file(scratch: &Scratch, rel: &str, file: &FileSpec) -> Result<(), String> {
    let parent = Path::new(rel)
        .parent()
        .ok_or("materialize(model): file has no parent")?;
    if !parent.as_os_str().is_empty() {
        scratch.mkdirs(
            parent
                .to_str()
                .ok_or("materialize(model): non-UTF-8 parent")?,
            0o755,
            TESTER,
        )?;
    }
    let owner = Owner {
        uid: file.uid.unwrap_or(TESTER.uid),
        gid: file.gid.unwrap_or(TESTER.gid),
    };
    let mode = file.mode.map_or(0o644, |mode| mode.0);
    if let Some(target) = &file.symlink {
        scratch.symlink(rel, target)?;
        scratch.chown(rel, owner)?;
    } else if let Some(kind) = file.special {
        match kind {
            Special::Fifo => {
                scratch.fifo(rel, mode, owner)?;
            }
            Special::Socket => {
                scratch.socket(rel, owner)?;
            }
            Special::CharDevice => unreachable!("rejected by validate"),
        }
    } else {
        let content = if file.cachedir_tag {
            CACHEDIR_TAG_CONTENT
        } else {
            file.content.as_deref().unwrap_or("").as_bytes()
        };
        let size = file.size.map_or(content.len(), |size| size.0 as usize);
        let mut bytes = vec![0; size];
        bytes[..content.len()].copy_from_slice(content);
        scratch.write_file(rel, &bytes, mode, owner)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use diskgc_model::profile_vector::ProfileVector;
    use diskgc_model::spec::{LockModeSpec, LockSpec, ProcAccess, ProcessSpec, StatxField};

    use super::*;

    const VECTOR: &str = include_str!(
        "../../crates/diskgc-model/tests/models/profiles/python-pycache-checkout.yaml"
    );
    const IN_USE_VECTOR: &str =
        include_str!("../../crates/diskgc-model/tests/models/profiles/python-pycache-in-use.yaml");

    #[test]
    fn outcome_projection_requires_independent_expectation_and_complete_stage_plan() {
        let mut vector = ProfileVector::parse(VECTOR).expect("checked-in vector");
        let model = vector.host.clone();
        let mut plan =
            OutcomeStagePlan::checked(&vector, vec![&model]).expect("independent expectation");
        plan.next_stage().expect("first stage");
        assert!(
            plan.next_stage().is_err(),
            "one stage cannot be issued twice"
        );

        vector.expect.reclaim_exit += 1;
        let error = OutcomeStagePlan::checked(&vector, vec![&model])
            .err()
            .expect("wrong independent expectation must fail");
        assert!(error.contains("exit"), "{error}");
        vector.expect.reclaim_exit -= 1;

        let mut changed = model.clone();
        changed.mounts[0].fs = "changed".into();
        let error = validate_stage_partition(&vector.host, &[&changed])
            .expect_err("changed mount geometry must fail");
        assert!(error.contains("beyond the checked"), "{error}");

        let mut added = model;
        added.tree.insert(
            "/home/tester/unexpected".into(),
            NodeSpec::File(FileSpec::default()),
        );
        let error = validate_stage_partition(&vector.host, &[&added])
            .expect_err("unlisted stage node must fail");
        assert!(error.contains("absent from the checked vector"), "{error}");

        let mut paths = vector.host.tree.keys();
        let first = paths.next().expect("first model tree node").clone();
        let missing = paths.next().expect("second model tree node").clone();
        let mut old_stage = vector.host.clone();
        old_stage.tree.retain(|path, _| path == &first);
        old_stage.processes.clear();
        old_stage.locks.clear();
        let mut fresh_stage = vector.host.clone();
        fresh_stage.tree.remove(&first);
        fresh_stage.tree.remove(&missing);
        let error = validate_stage_partition(&vector.host, &[&old_stage, &fresh_stage])
            .expect_err("node omitted from both historical stages must fail");
        assert!(error.contains("omits checked node"), "{error}");
        assert!(error.contains(&missing.display().to_string()), "{error}");

        let mut old = vector.host.clone();
        old.processes.clear();
        old.locks.clear();
        let error = validate_stage_partition(&vector.host, &[&old, &vector.host])
            .expect_err("duplicate stage tree must fail");
        assert!(error.contains("duplicates node"), "{error}");

        let process_vector = ProfileVector::parse(IN_USE_VECTOR).expect("in-use vector");
        assert!(
            !process_vector.host.processes.is_empty(),
            "process bite must be live"
        );
        let mut no_process = process_vector.host.clone();
        no_process.processes.clear();
        let error = validate_stage_partition(&process_vector.host, &[&no_process])
            .expect_err("missing final process cohort must fail");
        assert!(
            error.contains("final stage omits or changes checked processes"),
            "{error}"
        );

        let mut with_lock = process_vector.host.clone();
        let holder = with_lock.processes[0].pid;
        with_lock.locks.push(LockSpec {
            path: "/home/tester/held".into(),
            holder,
            mode: LockModeSpec::Exclusive,
        });
        let no_lock = process_vector.host.clone();
        let error = validate_stage_partition(&with_lock, &[&no_lock])
            .expect_err("missing final lock cohort must fail");
        assert!(
            error.contains("final stage omits or changes checked locks"),
            "{error}"
        );
    }

    #[test]
    fn lock_preflight_rejects_unsupported_shapes() {
        let mut vector = ProfileVector::parse(crate::differential_suite::CARGO_TARGET_VECTOR)
            .expect("Cargo target vector");
        vector.host.mounts[1].at = "/home/tester".into();
        vector.host.mounts[1].fs = "tmpfs".into();
        let process: ProcessSpec =
            serde_json::from_value(serde_json::json!({"pid": 42})).expect("minimal process");
        vector.host.processes.push(process);
        let lock = LockSpec {
            path: crate::differential_suite::CARGO_LOCK_PATH.into(),
            holder: 42,
            mode: LockModeSpec::Exclusive,
        };
        vector.host.locks.push(lock.clone());
        validate(&vector.host).expect("one existing tester-owned lock file");

        vector.host.locks.push(lock.clone());
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("only one process-owned lock")
        );
        vector.host.locks.pop();
        vector.host.processes[0].cwd = Some("/home/tester/project".into());
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("cwd is model-only")
        );
        vector.host.processes[0].cwd = None;
        vector.host.processes[0].fds.push(
            serde_json::from_value(
                serde_json::json!({"path": "/home/tester/project/good/Cargo.toml"}),
            )
            .expect("fd"),
        );
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("fds is model-only")
        );
        vector.host.processes[0].fds.clear();
        vector.host.locks[0].path = "/run/diskgc/lock".into();
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("only scratch files can be locked")
        );
        vector.host.locks[0].path = "/home/tester/project/good/target/debug".into();
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("readable and writable regular file")
        );
        vector.host.locks[0].path = lock.path;
        vector.host.locks[0].holder = 99;
        let error = validate(&vector.host).unwrap_err();
        assert!(error.contains("holder is model-only"), "{error}");
    }

    #[test]
    fn rejects_model_only_fields_before_materialization() {
        let mut vector = ProfileVector::parse(VECTOR).expect("checked-in vector");
        vector.host.kernel = Some("2.6.0".into());
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("kernel is model-only")
        );
        vector.host.kernel = None;
        vector.host.mounts[0].statx_missing.push(StatxField::MntId);
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("statx_missing is model-only")
        );
        vector.host.mounts[0].statx_missing.clear();
        vector.host.events.push(Default::default());
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("events is model-only")
        );
        vector.host.events.clear();
        vector.host.mounts[1].at = "/home/tester".into();
        vector.host.mounts[1].fs = "tmpfs".into();
        vector.host.tree.insert(
            "/run/diskgc/lock".into(),
            NodeSpec::File(FileSpec::default()),
        );
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("must match the harness-provisioned")
        );
    }

    #[test]
    fn process_preflight_accepts_only_a_reproducible_open_file_holder() {
        let mut vector = ProfileVector::parse(IN_USE_VECTOR).expect("checked-in vector");
        vector.host.tree.remove(Path::new("/usr/bin/python"));
        vector.host.mounts[1].at = "/home/tester".into();
        vector.host.mounts[1].fs = "tmpfs".into();
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("exe/maps/environ is model-only")
        );
        let process = &mut vector.host.processes[0];
        process.exe = None;
        process.maps.clear();
        process.environ.clear();
        validate(&vector.host).expect("one tester helper holding one named file");
        vector.host.processes[0].fds[0].deleted = true;
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("fds is model-only")
        );
        vector.host.processes[0].fds[0].deleted = false;
        vector.host.processes[0].proc = ProcAccess::Denied;
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("proc is model-only")
        );
        vector.host.processes[0].proc = ProcAccess::Readable;
        vector.host.processes[0].root = Some("/home/tester/project".into());
        assert!(
            validate(&vector.host)
                .unwrap_err()
                .contains("root/name/ids/dumpability/uid_map/privileged is model-only"),
            "custom process root must fail before fixture writes"
        );
    }
}
