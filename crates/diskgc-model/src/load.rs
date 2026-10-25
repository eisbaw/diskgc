//! Strict loading (docs/testing.md, Model tests): a [`HostModel`] becomes a
//! [`State`] plus its events, with every reference resolved. A dangling pid,
//! path or mount, a duplicate, or anything the model cannot express is a
//! [`LoadError`], never a default.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::host::{Access, Environ, Errno, Ids, LockMode};
use diskgc_core::report::Signal;

use crate::spec::{
    At, DirAttrs, EventSpec, FileSpec, HostModel, LockModeSpec, NodeSpec, Op, Special, When,
};
use crate::state::{
    DEFAULT_ID, Fault, Fs, Kind, Loc, Mount, Proc, Ref, State, normal_names, timestamp,
};

/// The capability a `privileged: true` process holds (as `systemd --user`
/// does).
const CAP_WAKE_ALARM: u32 = 35;

/// The kernel release a model reports unless it says otherwise.
pub const DEFAULT_KERNEL: &str = "6.12.0-model";

/// Why a host model does not load: where in the model, and what is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    /// Where, e.g. `processes[pid 4242].cwd` (empty for YAML syntax errors,
    /// whose message carries the location).
    pub at: String,
    pub message: String,
}

impl LoadError {
    fn new(at: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            at: at.into(),
            message: message.into(),
        }
    }

    fn fault(at: impl Into<String>, fault: Fault) -> Self {
        let message = match fault {
            Fault::Errno(e) => format!("not reachable ({e})"),
            Fault::Model(m) => m,
        };
        Self::new(at, message)
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.at.is_empty() {
            write!(f, "host model: {}", self.message)
        } else {
            write!(f, "host model: {}: {}", self.at, self.message)
        }
    }
}

impl std::error::Error for LoadError {}

/// What starts an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// The `nth` call of `op` (on `path`, if given).
    Call {
        op: Op,
        path: Option<PathBuf>,
        nth: u32,
        when: When,
    },
    /// The `index`-th call of the run, from 1.
    Index { index: u64, when: When },
    /// The clock has run this long.
    Elapsed(Duration),
}

/// What an event does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    DeliverSignal(Signal),
    Create(PathBuf),
    Mkdir(PathBuf),
    Remove(PathBuf),
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    Replace(PathBuf),
    ReplaceReusingInode(PathBuf),
    Chown {
        path: PathBuf,
        uid: u32,
    },
    Open {
        pid: u32,
        path: PathBuf,
    },
    Close {
        pid: u32,
        path: PathBuf,
    },
    Exit(u32),
    Fork {
        pid: u32,
        child: u32,
    },
    Dumpable {
        pid: u32,
        value: bool,
    },
    Access {
        pid: u32,
        proc: crate::spec::ProcAccess,
    },
    Advance(Duration),
    /// The call the event fires before fails with this errno.
    Fail(Errno),
    /// A new filesystem mounted on a directory.
    Mount(crate::spec::MountEventSpec),
}

/// A validated event; `index` is its position in the model's `events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub index: usize,
    pub trigger: Trigger,
    pub action: Action,
}

/// Parses a host model from YAML.
pub fn parse_yaml(text: &str) -> Result<HostModel, LoadError> {
    serde_saphyr::from_str(text).map_err(|e| LoadError::new("", e.to_string()))
}

/// Builds the state and events of a model.
pub fn load(model: &HostModel) -> Result<(State, Vec<Event>), LoadError> {
    if model.host_model != crate::spec::FORMAT_VERSION {
        return Err(LoadError::new(
            "host_model",
            format!(
                "format version {} is not supported (only {})",
                model.host_model,
                crate::spec::FORMAT_VERSION
            ),
        ));
    }
    let kernel = model
        .kernel
        .clone()
        .unwrap_or_else(|| DEFAULT_KERNEL.to_owned());
    let mut state = State::new(model.clock.0, kernel);
    if let Some(hostname) = &model.hostname {
        if hostname.is_empty() || hostname.contains(['/', '\0']) {
            return Err(LoadError::new(
                "hostname",
                "must be a nonempty host name without `/` or NUL",
            ));
        }
        state.hostname = hostname.clone();
    }
    if let Some(pid) = model.own_pid {
        state.own_pid = pid;
    }
    if let Some(uid) = model.own_uid {
        state.own_uid = uid;
    }
    state.users = model.users.clone();
    state.pid_namespace_initial = model.pid_namespace == crate::spec::PidNamespaceSpec::Initial;
    state.proc_hidepid = model.proc_hidepid;
    state.sandbox_gate = model.sandbox_gate;
    state.command_exit = model.command_exit;
    state.command_error_after_start = model.command_error_after_start;
    state.docker = model.docker.clone();
    match model.own_uid_map.as_deref() {
        None => {}
        Some("none") => state.own_uid_map = None,
        Some(map) => state.own_uid_map = Some(map.to_owned()),
    }
    load_mounts(&mut state, model)?;
    let deferred = load_tree(&mut state, model)?;
    load_processes(&mut state, model)?;
    load_locks(&mut state, model)?;
    remove_deleted(&mut state, model)?;
    // Last: every change above stamped its directory with `clock`.
    for (loc, attrs) in &deferred.dir_attrs {
        set_dir_attrs(&mut state, *loc, attrs, model);
    }
    let events = load_events(&state, model)?;
    Ok((state, events))
}

fn require_normal(at: &str, path: &Path) -> Result<(), LoadError> {
    normal_names(path)
        .map(|_| ())
        .map_err(|fault| LoadError::fault(at, fault))
}

fn load_mounts(state: &mut State, model: &HostModel) -> Result<(), LoadError> {
    let now = timestamp(model.clock.0);
    let mut paths: BTreeMap<u64, PathBuf> = BTreeMap::new();
    for spec in &model.mounts {
        let at = format!("mounts[id {}]", spec.id);
        require_normal(&at, &spec.at)?;
        if spec.id == 0 || paths.contains_key(&spec.id) {
            return Err(LoadError::new(at, "mount ids are unique and not 0"));
        }
        let dev = spec.dev.0;
        match (state.fss.get(&dev), spec.bind_of) {
            (None, None) => {}
            (None, Some(source)) => {
                return Err(LoadError::new(
                    &at,
                    format!("bind_of {source} has no filesystem on device {dev}"),
                ));
            }
            (Some(_), None) => {
                return Err(LoadError::new(
                    &at,
                    format!("device {dev} is mounted twice without bind_of"),
                ));
            }
            (Some(fs), Some(source)) => {
                let source_mount = state
                    .mounts
                    .iter()
                    .find(|mount| mount.id == source)
                    .ok_or_else(|| {
                        LoadError::new(&at, format!("bind_of {source} is not an earlier mount"))
                    })?;
                if source_mount.dev != dev {
                    return Err(LoadError::new(
                        &at,
                        format!("bind_of {source} uses a different device"),
                    ));
                }
                if fs.fs_type != spec.fs
                    || fs.size != spec.size.0
                    || fs.other_used != spec.other_used.0
                    || fs.missing != spec.statx_missing.iter().copied().collect()
                {
                    return Err(LoadError::new(
                        &at,
                        format!("bind_of {source} has incompatible filesystem metadata"),
                    ));
                }
            }
        }
        let on = match spec.parent {
            None if spec.at == Path::new("/") && state.mounts.is_empty() => None,
            None => {
                return Err(LoadError::new(
                    at,
                    "only the first mount, at /, has no parent",
                ));
            }
            Some(parent) => {
                let parent_at = paths.get(&parent).ok_or_else(|| {
                    LoadError::new(
                        &at,
                        format!("parent {parent} is not a mount declared before it"),
                    )
                })?;
                let rel = spec.at.strip_prefix(parent_at).map_err(|_| {
                    LoadError::new(
                        &at,
                        format!(
                            "{} is not below its parent's mount point {}",
                            spec.at.display(),
                            parent_at.display()
                        ),
                    )
                })?;
                Some(mount_point(state, parent, rel).map_err(|m| LoadError::new(&at, m))?)
            }
        };
        if spec.bind_of.is_none() {
            let mut fs = Fs::new(dev, spec.fs.clone(), spec.size.0, spec.other_used.0, now);
            fs.missing = spec.statx_missing.iter().copied().collect();
            state.fss.insert(dev, fs);
        }
        state.mounts.push(Mount {
            id: spec.id,
            parent: spec.parent,
            dev,
            on,
            read_only: spec.read_only,
            dead: spec.dead,
        });
        paths.insert(spec.id, spec.at.clone());
    }
    if state.mounts.is_empty() {
        return Err(LoadError::new("mounts", "there must be a mount at /"));
    }
    Ok(())
}

/// The directory at `rel` below the root of mount `parent`, created if
/// missing, without crossing into another mount.
fn mount_point(state: &mut State, parent: u64, rel: &Path) -> Result<Loc, String> {
    let mut here = state.mount_root(parent);
    let names: Vec<_> = rel.components().collect();
    for (i, name) in names.iter().enumerate() {
        let last = i + 1 == names.len();
        let name = name.as_os_str();
        let entry = state
            .inode(here)
            .entries()
            .and_then(|e| e.get(name))
            .copied();
        here = match entry {
            None => state
                .create_in(here, name, Kind::Dir(BTreeMap::new()))
                .map_err(|f| format!("{f:?}"))?,
            Some(ino) => {
                let next = Loc { mnt: parent, ino };
                // Two mounts on one directory of the same parent are
                // siblings (ambiguous, as mount propagation can make them);
                // a mount covering a directory on the way is the parent.
                if !last && state.mounts.iter().any(|m| m.on == Some(next)) {
                    return Err(format!(
                        "{} is covered by another mount, which should be its parent",
                        state.path_of(next).unwrap_or_default().display()
                    ));
                }
                if state.inode(next).entries().is_none() {
                    return Err("the mount point is not a directory".to_owned());
                }
                next
            }
        };
    }
    Ok(here)
}

/// What waits until every node exists: hard links (their targets), and
/// directory attributes (linking entries stamps a directory's times).
#[derive(Default)]
struct Deferred {
    links: Vec<PendingLink>,
    dir_attrs: Vec<(Loc, DirAttrs)>,
}

struct PendingLink {
    at: String,
    dir: Loc,
    name: String,
    target: PathBuf,
}

fn load_tree(state: &mut State, model: &HostModel) -> Result<Deferred, LoadError> {
    let mut deferred = Deferred::default();
    for (path, node) in &model.tree {
        let at = format!("tree[{}]", path.display());
        require_normal(&at, path)?;
        match path.file_name() {
            // The root directory itself.
            None => add_to(
                state,
                state.lookup(path).expect("/ exists"),
                node,
                &at,
                model,
                &mut deferred,
            )?,
            Some(name) => {
                let name = name
                    .to_str()
                    .ok_or_else(|| LoadError::new(&at, "names must be UTF-8"))?;
                let parent = path.parent().expect("a path with a name has a parent");
                let dir = state
                    .create_path(parent, Kind::Dir(BTreeMap::new()))
                    .map_err(|f| LoadError::fault(&at, f))?;
                add_node(state, dir, name, node, &at, model, &mut deferred)?;
            }
        }
    }
    for link in std::mem::take(&mut deferred.links) {
        let target = state
            .lookup(&link.target)
            .map_err(|f| LoadError::fault(format!("{}.link", link.at), f))?;
        if target.mnt != link.dir.mnt {
            return Err(LoadError::new(
                link.at,
                "a hard link must be on the same mount as its target",
            ));
        }
        if state.inode(target).entries().is_some() {
            return Err(LoadError::new(link.at, "cannot hard-link a directory"));
        }
        state
            .link(link.dir, link.name.as_ref(), target.ino)
            .map_err(|f| LoadError::fault(link.at, f))?;
    }
    Ok(deferred)
}

/// Adds `node` as entry `name` of `dir`. A directory node naming an
/// existing directory (a mount point, or one another key made) adds to it.
fn add_node(
    state: &mut State,
    dir: Loc,
    name: &str,
    node: &NodeSpec,
    at: &str,
    model: &HostModel,
    deferred: &mut Deferred,
) -> Result<(), LoadError> {
    match state.child(dir, name.as_ref()) {
        Ok(existing) => match node {
            NodeSpec::Dir(_) if state.inode(existing).entries().is_some() => {
                add_to(state, existing, node, at, model, deferred)
            }
            _ => Err(LoadError::new(at, "declared twice")),
        },
        Err(diskgc_core::host::Errno::ENOENT) => {
            create_node(state, dir, name, node, at, model, deferred)
        }
        Err(e) => Err(LoadError::fault(at, Fault::Errno(e))),
    }
}

/// Adds a directory node's attributes and entries to the directory `loc`.
fn add_to(
    state: &mut State,
    loc: Loc,
    node: &NodeSpec,
    at: &str,
    model: &HostModel,
    deferred: &mut Deferred,
) -> Result<(), LoadError> {
    let NodeSpec::Dir(spec) = node else {
        return Err(LoadError::new(at, "declared twice"));
    };
    if spec.attrs != DirAttrs::default() {
        deferred.dir_attrs.push((loc, spec.attrs.clone()));
    }
    for (name, child) in &spec.entries {
        add_node(
            state,
            loc,
            name,
            child,
            &format!("{at}/{name}"),
            model,
            deferred,
        )?;
    }
    Ok(())
}

fn create_node(
    state: &mut State,
    dir: Loc,
    name: &str,
    node: &NodeSpec,
    at: &str,
    model: &HostModel,
    deferred: &mut Deferred,
) -> Result<(), LoadError> {
    match node {
        NodeSpec::Dir(_) => {
            let loc = state
                .create_in(dir, name.as_ref(), Kind::Dir(BTreeMap::new()))
                .map_err(|f| LoadError::fault(at, f))?;
            add_to(state, loc, node, at, model, deferred)
        }
        NodeSpec::File(spec) if spec.link.is_some() => {
            if *spec
                != (FileSpec {
                    link: spec.link.clone(),
                    ..FileSpec::default()
                })
            {
                return Err(LoadError::new(
                    at,
                    "a hard link takes no other attributes (they are its target's)",
                ));
            }
            deferred.links.push(PendingLink {
                at: at.to_owned(),
                dir,
                name: name.to_owned(),
                target: spec.link.clone().expect("checked"),
            });
            Ok(())
        }
        NodeSpec::File(spec) => {
            let kind = match (&spec.symlink, spec.special) {
                (Some(_), Some(_)) => {
                    return Err(LoadError::new(at, "a symlink has no 'type'"));
                }
                (Some(target), None) => Kind::Symlink(target.clone()),
                (None, Some(Special::Socket)) => Kind::Socket,
                (None, Some(Special::Fifo)) => Kind::Fifo,
                (None, Some(Special::CharDevice)) => Kind::CharDevice,
                (None, None) => Kind::File {
                    cachedir_tag: spec.cachedir_tag,
                    content: spec
                        .content
                        .as_ref()
                        .map_or_else(Vec::new, |s| s.as_bytes().to_vec()),
                },
            };
            if !matches!(kind, Kind::File { .. })
                && (spec.size.is_some() || spec.cachedir_tag || spec.content.is_some())
            {
                return Err(LoadError::new(
                    at,
                    "only regular files have a size or a CACHEDIR.TAG signature",
                ));
            }
            if spec.cachedir_tag && spec.content.is_some() {
                return Err(LoadError::new(
                    at,
                    "a regular file cannot have both content and a CACHEDIR.TAG signature",
                ));
            }
            if spec.bound && !matches!(kind, Kind::Socket) {
                return Err(LoadError::new(at, "only a socket can be 'bound'"));
            }
            let loc = state
                .create_in(dir, name.as_ref(), kind)
                .map_err(|f| LoadError::fault(at, f))?;
            if spec.bound {
                let id = state.dev_ino(loc);
                state.bound_sockets.insert(id);
            }
            let mtime = timestamp(spec.mtime.map_or(model.clock.0, |t| t.0));
            let inode = state.inode_mut(loc);
            inode.size = spec
                .size
                .map_or(spec.content.as_ref().map_or(0, |s| s.len() as u64), |s| s.0);
            inode.mtime = mtime;
            inode.ctime = mtime;
            inode.btime = mtime;
            if let Some(uid) = spec.uid {
                inode.uid = uid;
            }
            if let Some(gid) = spec.gid {
                inode.gid = gid;
            }
            if let Some(mode) = spec.mode {
                inode.perm = mode.0;
            }
            Ok(())
        }
    }
}

fn set_dir_attrs(state: &mut State, loc: Loc, attrs: &DirAttrs, model: &HostModel) {
    let mtime = timestamp(attrs.mtime.map_or(model.clock.0, |t| t.0));
    let inode = state.inode_mut(loc);
    inode.mtime = mtime;
    inode.ctime = mtime;
    inode.btime = mtime;
    if let Some(uid) = attrs.uid {
        inode.uid = uid;
    }
    if let Some(gid) = attrs.gid {
        inode.gid = gid;
    }
    if let Some(mode) = attrs.mode {
        inode.perm = mode.0;
    }
}

fn reference(state: &State, at: &str, path: &Path) -> Result<Ref, LoadError> {
    require_normal(at, path)?;
    let loc = state.lookup(path).map_err(|fault| {
        LoadError::new(
            at,
            match fault {
                Fault::Errno(e) => format!("{} is not in the tree ({e})", path.display()),
                Fault::Model(m) => m,
            },
        )
    })?;
    Ok(Ref {
        loc,
        path: path.to_owned(),
    })
}

fn load_processes(state: &mut State, model: &HostModel) -> Result<(), LoadError> {
    let root = PathBuf::from("/");
    for spec in &model.processes {
        let at = format!("processes[pid {}]", spec.pid);
        if state.procs.contains_key(&spec.pid) {
            return Err(LoadError::new(at, "pid declared twice"));
        }
        let exe = spec
            .exe
            .as_deref()
            .map(|p| reference(state, &format!("{at}.exe"), p))
            .transpose()?;
        let cwd = reference(
            state,
            &format!("{at}.cwd"),
            spec.cwd.as_ref().unwrap_or(&root),
        )?;
        let proc_root = reference(
            state,
            &format!("{at}.root"),
            spec.root.as_ref().unwrap_or(&root),
        )?;
        for (name, r) in [("cwd", &cwd), ("root", &proc_root)] {
            if state.inode(r.loc).entries().is_none() {
                return Err(LoadError::new(format!("{at}.{name}"), "is not a directory"));
            }
        }
        let maps = spec
            .maps
            .iter()
            .map(|p| reference(state, &format!("{at}.maps"), p))
            .collect::<Result<_, _>>()?;
        if spec.pid == state.own_pid {
            return Err(LoadError::new(at, "is diskgc's own pid (own_pid)"));
        }
        let uid = spec.uid.unwrap_or(DEFAULT_ID);
        let euid = spec.euid.unwrap_or(uid);
        let name = spec.name.clone().map_or_else(
            || {
                spec.exe
                    .as_ref()
                    .and_then(|e| e.file_name())
                    .map_or_else(|| "?".into(), ToOwned::to_owned)
            },
            Into::into,
        );
        let nspid = match &spec.nspid {
            None => Some(vec![spec.pid]),
            Some(crate::spec::NspidSpec::Unreadable(_)) => None,
            Some(crate::spec::NspidSpec::Levels(levels)) => {
                if levels.first() != Some(&spec.pid) || levels.contains(&0) {
                    return Err(LoadError::new(
                        format!("{at}.nspid"),
                        "must start with the process's pid and hold no 0",
                    ));
                }
                Some(levels.clone())
            }
        };
        if spec.zombie && (!spec.fds.is_empty() || !spec.maps.is_empty()) {
            return Err(LoadError::new(
                at,
                "a zombie has no fds or maps (it has no live task)",
            ));
        }
        let generation = state.next_process_generation;
        state.next_process_generation += 1;
        state.procs.insert(
            spec.pid,
            Proc {
                generation,
                exe,
                cwd,
                root: proc_root,
                environ: environ(&spec.environ),
                fds: BTreeMap::new(),
                maps,
                access: spec.proc,
                name,
                uid: Ids {
                    real: uid,
                    effective: euid,
                    saved: euid,
                    fs: euid,
                },
                gid: Ids::same(spec.gid.unwrap_or(DEFAULT_ID)),
                dumpable: !spec.non_dumpable,
                uid_map: match spec.uid_map.as_deref() {
                    None => Some(crate::state::INIT_UID_MAP.to_owned()),
                    Some("none") => None,
                    Some(map) => Some(map.to_owned()),
                },
                cap_permitted: if spec.privileged {
                    1 << CAP_WAKE_ALARM
                } else {
                    0
                },
                nspid,
                zombie: spec.zombie,
                cmdline: match &spec.cmdline {
                    None => Some(spec.exe.iter().map(|e| e.as_os_str().to_owned()).collect()),
                    Some(crate::spec::CmdlineSpec::Unreadable(_)) => None,
                    Some(crate::spec::CmdlineSpec::Args(args)) => {
                        Some(args.iter().map(Into::into).collect())
                    }
                },
                view: view(&at, &spec.view)?,
            },
        );
        for fd in &spec.fds {
            let r = reference(state, &format!("{at}.fds"), &fd.path)?;
            let ofd = state.open_ofd(r.loc, r.path, Access::Read);
            state
                .proc_attach(spec.pid, ofd, fd.fd)
                .map_err(|f| LoadError::fault(format!("{at}.fds"), f))?;
        }
    }
    Ok(())
}

/// A process's `view`: absolute, normal prefixes on both sides (the
/// targets need not exist: a lookup through a missing one fails as on
/// Linux).
fn view(
    at: &str,
    view: &BTreeMap<PathBuf, PathBuf>,
) -> Result<BTreeMap<PathBuf, PathBuf>, LoadError> {
    for (from, to) in view {
        require_normal(&format!("{at}.view"), from)?;
        require_normal(&format!("{at}.view"), to)?;
    }
    Ok(view.clone())
}

/// The environment of these `NAME=value` entries (split at the first `=`
/// when read).
fn environ(entries: &[String]) -> Environ {
    Environ::from_entries(entries)
}

fn load_locks(state: &mut State, model: &HostModel) -> Result<(), LoadError> {
    for (i, spec) in model.locks.iter().enumerate() {
        let at = format!("locks[{i}]");
        match state.procs.get(&spec.holder) {
            None => {
                return Err(LoadError::new(
                    at,
                    format!("holder {} is not one of the processes", spec.holder),
                ));
            }
            Some(proc) if proc.zombie => {
                return Err(LoadError::new(at, "a zombie holds no lock"));
            }
            Some(_) => {}
        }
        let r = reference(state, &format!("{at}.path"), &spec.path)?;
        let mode = match spec.mode {
            LockModeSpec::Shared => LockMode::Shared,
            LockModeSpec::Exclusive => LockMode::Exclusive,
        };
        let ofd = state.open_ofd(r.loc, r.path, Access::Read);
        if !state.lock_free(ofd, mode) {
            return Err(LoadError::new(
                at,
                "conflicts with another lock on the same file",
            ));
        }
        state.ofds.get_mut(&ofd).expect("just opened").lock = Some(mode);
        state
            .proc_attach(spec.holder, ofd, None)
            .map_err(|f| LoadError::fault(&at, f))?;
    }
    Ok(())
}

/// Removes the names of files that processes hold open as deleted.
fn remove_deleted(state: &mut State, model: &HostModel) -> Result<(), LoadError> {
    let deleted: BTreeSet<&PathBuf> = model
        .processes
        .iter()
        .flat_map(|p| p.fds.iter())
        .filter(|fd| fd.deleted)
        .map(|fd| &fd.path)
        .collect();
    for path in deleted {
        let loc = state.lookup(path).map_err(|f| LoadError::fault("fds", f))?;
        if state.inode(loc).entries().is_some() {
            return Err(LoadError::new(
                format!("fds[{}]", path.display()),
                "only files are modelled as deleted while open",
            ));
        }
        state
            .remove_path(path)
            .map_err(|f| LoadError::fault(format!("fds[{}]", path.display()), f))?;
    }
    Ok(())
}

fn load_events(state: &State, model: &HostModel) -> Result<Vec<Event>, LoadError> {
    // Children forked by events may be named by any event (events fire in
    // trace order, not in declaration order).
    let forked: BTreeSet<u32> = model
        .events
        .iter()
        .filter_map(|e| e.fork.as_ref().map(|f| f.child))
        .collect();
    model
        .events
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            let at = format!("events[{index}]");
            Ok(Event {
                index,
                trigger: trigger(&spec.at).map_err(|m| LoadError::new(format!("{at}.at"), m))?,
                action: action(state, spec, &forked).map_err(|m| LoadError::new(&at, m))?,
            })
        })
        .collect()
}

fn trigger(at: &At) -> Result<Trigger, String> {
    match (at.op, at.call, at.elapsed) {
        (Some(op), None, None) => {
            if let Some(path) = &at.path {
                normal_names(path).map_err(|f| format!("path: {f:?}"))?;
            }
            let nth = at.nth.unwrap_or(1);
            if nth == 0 {
                return Err("nth counts from 1".to_owned());
            }
            Ok(Trigger::Call {
                op,
                path: at.path.clone(),
                nth,
                when: at.when.ok_or("a call trigger needs 'when: before|after'")?,
            })
        }
        (None, Some(index), None) => {
            if at.path.is_some() || at.nth.is_some() {
                return Err("'call' takes no 'path' or 'nth'".to_owned());
            }
            if index == 0 {
                return Err("calls count from 1".to_owned());
            }
            Ok(Trigger::Index {
                index,
                when: at.when.ok_or("a call trigger needs 'when: before|after'")?,
            })
        }
        (None, None, Some(elapsed)) => {
            if at.path.is_some() || at.nth.is_some() || at.when.is_some() {
                return Err("'elapsed' takes no 'path', 'nth' or 'when'".to_owned());
            }
            Ok(Trigger::Elapsed(elapsed.0))
        }
        _ => Err("give exactly one of 'op', 'call' or 'elapsed'".to_owned()),
    }
}

fn action(state: &State, spec: &EventSpec, forked: &BTreeSet<u32>) -> Result<Action, String> {
    let known_pid = |pid: u32| {
        if state.procs.contains_key(&pid) || forked.contains(&pid) {
            Ok(pid)
        } else {
            Err(format!(
                "pid {pid} is not one of the processes (or a child an earlier event forks)"
            ))
        }
    };
    let path = |p: &PathBuf| {
        normal_names(p)
            .map(|_| p.clone())
            .map_err(|f| format!("{f:?}"))
    };
    let mut actions = Vec::new();
    if let Some(signal) = spec.signal {
        actions.push(Action::DeliverSignal(match signal {
            crate::spec::ModelSignal::Int => Signal::Int,
            crate::spec::ModelSignal::Term => Signal::Term,
        }));
    }
    if let Some(p) = &spec.create {
        actions.push(Action::Create(path(p)?));
    }
    if let Some(p) = &spec.mkdir {
        actions.push(Action::Mkdir(path(p)?));
    }
    if let Some(p) = &spec.remove {
        actions.push(Action::Remove(path(p)?));
    }
    if let Some(r) = &spec.rename {
        actions.push(Action::Rename {
            from: path(&r.from)?,
            to: path(&r.to)?,
        });
    }
    if let Some(p) = &spec.replace {
        actions.push(Action::Replace(path(p)?));
    }
    if let Some(p) = &spec.replace_reusing_inode {
        actions.push(Action::ReplaceReusingInode(path(p)?));
    }
    if let Some(c) = &spec.chown {
        actions.push(Action::Chown {
            path: path(&c.path)?,
            uid: c.uid,
        });
    }
    if let Some(o) = &spec.open {
        actions.push(Action::Open {
            pid: known_pid(o.pid)?,
            path: path(&o.path)?,
        });
    }
    if let Some(c) = &spec.close {
        actions.push(Action::Close {
            pid: known_pid(c.pid)?,
            path: path(&c.path)?,
        });
    }
    if let Some(pid) = spec.exit {
        actions.push(Action::Exit(known_pid(pid)?));
    }
    if let Some(a) = &spec.access {
        actions.push(Action::Access {
            pid: known_pid(a.pid)?,
            proc: a.proc,
        });
    }
    if let Some(d) = &spec.dumpable {
        actions.push(Action::Dumpable {
            pid: known_pid(d.pid)?,
            value: d.value,
        });
    }
    if let Some(f) = &spec.fork {
        actions.push(Action::Fork {
            pid: known_pid(f.pid)?,
            child: f.child,
        });
    }
    if let Some(d) = spec.advance {
        actions.push(Action::Advance(d.0));
    }
    if let Some(errno) = spec.fail {
        let fallible = [
            Op::Openat2,
            Op::Statx,
            Op::ReadLink,
            Op::ReadDir,
            Op::ReadHead,
            Op::Unlinkat,
            Op::Fchmod,
            Op::BoundUnixSockets,
            Op::Statvfs,
            Op::ProcIdentity,
            Op::ProcIdentityReleased,
            Op::ProcFds,
            Op::ProcLinks,
            Op::ProcMaps,
            Op::ProcEnviron,
            Op::ProcStatus,
            Op::DockerQuery,
            Op::RunCommand,
            Op::ProcUids,
            Op::ProcCmdline,
            Op::ProcRootLookup,
        ];
        match (spec.at.op, spec.at.when) {
            (Some(op), Some(When::Before)) if fallible.contains(&op) => {}
            _ => {
                return Err(format!(
                    "'fail' needs an op trigger with 'when: before' on one of {:?}",
                    fallible.map(Op::name)
                ));
            }
        }
        actions.push(Action::Fail(errno.0));
    }
    if let Some(m) = &spec.mount {
        path(&m.at)?;
        if m.id == 0 || state.mounts.iter().any(|existing| existing.id == m.id) {
            return Err(format!("mount id {} is 0 or taken", m.id));
        }
        if state.fss.contains_key(&m.dev.0) {
            return Err(format!("device {} is mounted already", m.dev.0));
        }
        actions.push(Action::Mount(m.clone()));
    }
    match actions.len() {
        1 => Ok(actions.pop().expect("one")),
        0 => Err(
            "no action (signal, create, mkdir, remove, rename, replace, replace_reusing_inode, chown, open, close, \
                  exit, fork, dumpable, access, advance, fail, mount)"
                .to_owned(),
        ),
        _ => Err("more than one action; use one event per action".to_owned()),
    }
}
