//! Materialize the kernel-representable part of a `HostModel` on the
//! container's scratch tmpfs. Validation runs before any fixture write, so
//! model-only fields cannot silently change the meaning of a replay.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use diskgc_model::host::CACHEDIR_TAG_CONTENT;
use diskgc_model::spec::{
    FileSpec, GateSpec, HostModel, NodeSpec, ProcAccess, ProcessSpec, Special, WallTime,
};

use crate::holder::Holder;
use crate::scratch::{self, Scratch};
use crate::suite::Ctx;
use crate::{Owner, TESTER};

const MAX_FILE_SIZE: usize = 64 * 1024 * 1024;

/// Fixture helpers stay alive until both LinuxHost runs and the deletion
/// oracle finish. Dropping this guard closes their stdin and waits for them.
pub struct Materialized {
    holders: Vec<(Holder, PathBuf)>,
}

impl Materialized {
    /// Verify the kernel still exposes each promised open file. A helper
    /// that exits between planning and reclaim would make an in-use test
    /// vacuous; this check makes that failure explicit.
    pub fn assert_holding(&self) -> Result<(), String> {
        for (holder, path) in &self.holders {
            let fd_dir = format!("/proc/{}/fd", holder.pid());
            let entries =
                fs::read_dir(&fd_dir).map_err(|error| format!("list helper {fd_dir}: {error}"))?;
            let mut found = false;
            for entry in entries {
                let entry = entry.map_err(|error| format!("read helper {fd_dir}: {error}"))?;
                let target = fs::read_link(entry.path()).map_err(|error| {
                    format!("read helper fd {}: {error}", entry.path().display())
                })?;
                if target == *path {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(format!(
                    "helper pid {} no longer holds {} open",
                    holder.pid(),
                    path.display()
                ));
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(), String> {
        for (holder, _) in self.holders {
            holder.finish()?;
        }
        Ok(())
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
    if !model.locks.is_empty() {
        return unsupported("locks", "flock replay is not implemented");
    }
    for (index, process) in model.processes.iter().enumerate() {
        validate_process(index, process)?;
    }
    if model.mounts.is_empty() {
        return unsupported("mounts", "a root mount is required");
    }
    for (index, mount) in model.mounts.iter().enumerate() {
        let field = format!("mounts[{index}]");
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
            if matches!(mount.fs.as_str(), "nfs" | "btrfs") {
                return unsupported(
                    &format!("{field}.fs"),
                    "the container root is not this filesystem type",
                );
            }
            continue; // The container's root filesystem is provisioned by the image.
        }
        if mount.fs != "tmpfs"
            || !matches!(
                mount.at.to_str(),
                Some("/tmp" | "/home/tester" | "/run/diskgc")
            )
        {
            return unsupported(&field, "only the harness's tmpfs mounts are supported");
        }
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
        let _ = destination(path)?;
        validate_node(path, node)?;
    }
    Ok(())
}

fn validate_process(index: usize, process: &ProcessSpec) -> Result<(), String> {
    let field = format!("processes[{index}]");
    let unsupported = |name: &str, reason: &str| {
        Err(format!(
            "materialize(model): {field}.{name} is model-only: {reason}"
        ))
    };
    if process.proc != ProcAccess::Readable {
        return unsupported("proc", "the ready helper must be inspectable");
    }
    if process.exe.is_some() || !process.maps.is_empty() || !process.environ.is_empty() {
        return unsupported(
            "exe/maps/environ",
            "the harness probe has its own executable, mappings and environment",
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
    if process.fds.len() != 1 || process.fds[0].deleted || process.fds[0].fd.is_some() {
        return unsupported(
            "fds",
            "the helper holds exactly one named, existing file at a kernel-selected fd",
        );
    }
    destination(&process.fds[0].path)?;
    if let Some(cwd) = &process.cwd {
        destination(cwd)?;
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
    for root in ["/tmp", "/home/tester"] {
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

/// Preflight-validate, then create the model tree on fresh scratch mounts.
/// Call only inside the interlocked e2e container. The required model clock
/// and nominal mount size/device/id are deliberately not materialized;
/// differential callers must compare outcomes that do not depend on them.
pub fn materialize(ctx: &Ctx, model: &HostModel) -> Result<Materialized, String> {
    validate(model)?;
    for (path, node) in &model.tree {
        if path == Path::new("/run/diskgc/lock") {
            continue;
        }
        let (root, relative) = destination(path)?;
        let scratch = if root == "/tmp" {
            &ctx.scratch
        } else {
            &ctx.home
        };
        create_node(scratch, relative, node)?;
    }
    // Creating children changes parent mtimes. Apply explicit times only
    // after the complete tree exists, with descendants before their parents.
    for (path, node) in &model.tree {
        if path == Path::new("/run/diskgc/lock") {
            continue;
        }
        let (root, relative) = destination(path)?;
        let scratch = if root == "/tmp" {
            &ctx.scratch
        } else {
            &ctx.home
        };
        apply_mtimes(scratch, relative, node)?;
    }
    let mut holders = Vec::new();
    for process in &model.processes {
        let path = &process.fds[0].path;
        let path_text = path
            .to_str()
            .ok_or("materialize(model): non-UTF-8 process fd path")?;
        let cwd_text = process
            .cwd
            .as_deref()
            .unwrap_or(Path::new("/"))
            .to_str()
            .ok_or("materialize(model): non-UTF-8 process cwd")?;
        let holder = Holder::tester(ctx, &["hold", path_text, cwd_text], &[]).map_err(|error| {
            format!(
                "materialize(model): helper for pid {}: {error}",
                process.pid
            )
        })?;
        holders.push((holder, path.clone()));
    }
    let materialized = Materialized { holders };
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
    use diskgc_model::spec::{ProcAccess, StatxField};

    use super::*;

    const VECTOR: &str = include_str!(
        "../../crates/diskgc-model/tests/models/profiles/python-pycache-checkout.yaml"
    );
    const IN_USE_VECTOR: &str =
        include_str!("../../crates/diskgc-model/tests/models/profiles/python-pycache-in-use.yaml");

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
    }
}
