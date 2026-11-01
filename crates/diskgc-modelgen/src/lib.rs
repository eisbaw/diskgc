//! diskgc-modelgen: generates host-model values (docs/testing.md, Property
//! tests): `proptest` strategies for trees, processes and mounts, and a
//! helper that places an event at a chosen call. Test-only: never a
//! dependency of the `diskgc` binary.
//!
//! Generated models are small on purpose: property tests run every call
//! position of a scenario, so a few dozen entries already make thousands of
//! runs. A failing case shrinks to a minimal model; print it with
//! [`HostModel::to_yaml`] and save it under
//! `crates/diskgc-model/tests/regressions/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::host::Dev;
use diskgc_model::spec::{
    At, Bytes, DevSpec, DirAttrs, DirSpec, EventSpec, FdSpec, FileSpec, HostModel, LoopSpec,
    MountSpec, NodeSpec, PidNamespaceSpec, ProcAccess, ProcessSpec, Special, WallTime, When,
};
use proptest::prelude::*;
use proptest::sample::select;

/// The wall clock of every generated model.
pub const CLOCK: &str = "2026-10-15T03:00:00Z";

/// Where generated trees live: below the tmpfs mounted at `/tmp`.
pub const ROOT: &str = "/tmp/gen";

/// The executable every generated process runs (and maps), outside the
/// generated tree.
pub const EXE: &str = "/usr/bin/tool";

/// Mount id of the generated `/tmp`.
pub const TMP_MOUNT: u64 = 30;

/// The uid diskgc runs as in generated models, and another user's.
pub const OWNERS: [u32; 2] = [1000, 1001];

fn clock() -> WallTime {
    CLOCK.parse().expect("CLOCK is RFC 3339")
}

/// Entry names: short, never `.` or `..`, sometimes hidden.
pub fn name() -> impl Strategy<Value = String> {
    "\\.?[a-z][a-z0-9_-]{0,5}"
}

/// A modification time up to 30 days before [`CLOCK`].
pub fn mtime() -> impl Strategy<Value = WallTime> {
    (0u64..30 * 86_400).prop_map(|ago| {
        let start = clock().0;
        WallTime(start - Duration::from_secs(ago))
    })
}

/// Mostly diskgc's own uid, sometimes another user's.
pub fn owner() -> impl Strategy<Value = u32> {
    prop_oneof![4 => Just(OWNERS[0]), 1 => Just(OWNERS[1])]
}

/// A non-directory: mostly regular files up to 64 KiB, sometimes a
/// symlink, socket or FIFO.
pub fn file() -> impl Strategy<Value = FileSpec> {
    let regular = (0u64..=64 << 10, mtime(), owner()).prop_map(|(size, mtime, uid)| FileSpec {
        size: Some(Bytes(size)),
        mtime: Some(mtime),
        uid: Some(uid),
        ..FileSpec::default()
    });
    let symlink =
        prop_oneof![Just("/etc"), Just(".."), Just("elsewhere")].prop_map(|target| FileSpec {
            symlink: Some(PathBuf::from(target)),
            ..FileSpec::default()
        });
    let special =
        prop_oneof![Just(Special::Socket), Just(Special::Fifo)].prop_map(|kind| FileSpec {
            special: Some(kind),
            ..FileSpec::default()
        });
    prop_oneof![8 => regular, 1 => symlink, 1 => special]
}

/// A directory tree at most `depth` levels deep with at most `breadth`
/// entries per directory.
pub fn tree(depth: u32, breadth: usize) -> impl Strategy<Value = DirSpec> {
    let leaf = btree(file().prop_map(NodeSpec::File), breadth);
    leaf.prop_recursive(
        depth,
        (breadth * depth as usize) as u32,
        breadth as u32,
        move |inner| {
            btree(
                prop_oneof![
                    2 => file().prop_map(NodeSpec::File),
                    1 => inner.prop_map(NodeSpec::Dir),
                ],
                breadth,
            )
        },
    )
}

/// A directory of up to `breadth` entries drawn from `entry`, with
/// generated attributes.
fn btree(entry: impl Strategy<Value = NodeSpec>, breadth: usize) -> impl Strategy<Value = DirSpec> {
    (
        proptest::collection::btree_map(name(), entry, 0..=breadth),
        mtime(),
        owner(),
    )
        .prop_map(|(entries, mtime, uid)| DirSpec {
            attrs: DirAttrs {
                mtime: Some(mtime),
                uid: Some(uid),
                ..DirAttrs::default()
            },
            entries,
        })
}

/// The directories and regular files of a tree rooted at `at`.
pub fn paths(at: &Path, dir: &DirSpec) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut dirs = vec![at.to_owned()];
    let mut files = Vec::new();
    for (name, node) in &dir.entries {
        let path = at.join(name);
        match node {
            NodeSpec::Dir(sub) => {
                let (d, f) = paths(&path, sub);
                dirs.extend(d);
                files.extend(f);
            }
            NodeSpec::File(f) if f.symlink.is_none() && f.special.is_none() => files.push(path),
            NodeSpec::File(_) => {}
        }
    }
    (dirs, files)
}

/// A process running [`EXE`] (which it maps) whose cwd is one of `dirs`,
/// that holds some of `files` open and has `PATH` and sometimes `TMPDIR` in
/// its initial environment; sometimes unreadable. With no open file its fd
/// table is empty, which reads as `Failed`, as on `LinuxHost`.
pub fn process(
    pid: u32,
    dirs: Vec<PathBuf>,
    files: Vec<PathBuf>,
) -> impl Strategy<Value = ProcessSpec> {
    let most = files.len().min(2);
    let fds = proptest::sample::subsequence(files, 0..=most);
    (
        select(dirs.clone()),
        fds,
        proptest::option::of(select(dirs)),
        prop_oneof![8 => Just(ProcAccess::Readable), 1 => Just(ProcAccess::Denied), 1 => Just(ProcAccess::Failed)],
    )
        .prop_map(move |(cwd, fds, tmpdir, proc)| ProcessSpec {
            pid,
            exe: Some(PathBuf::from(EXE)),
            cwd: Some(cwd),
            root: None,
            environ: std::iter::once("PATH=/usr/bin".to_owned())
                .chain(tmpdir.map(|d| format!("TMPDIR={}", d.display())))
                .collect(),
            fds: fds
                .into_iter()
                .map(|path| FdSpec {
                    fd: None,
                    path,
                    deleted: false,
                })
                .collect(),
            maps: vec![PathBuf::from(EXE)],
            proc,
            nspid: None,
            zombie: false,
            cmdline: None,
            view: Default::default(),
            name: None,
            uid: None,
            euid: None,
            gid: None,
            non_dumpable: false,
            privileged: false,
            uid_map: None,
        })
}

/// The two mounts every generated model has: ext4 at `/` and a 1 GiB tmpfs
/// at `/tmp` (id [`TMP_MOUNT`]).
pub fn base_mounts() -> Vec<MountSpec> {
    let mount = |id, at: &str, fs: &str, minor, size, parent| MountSpec {
        id,
        at: at.into(),
        fs: fs.into(),
        dev: DevSpec(Dev { major: 0, minor }),
        bind_of: None,
        bind_root: None,
        overlay_upper: None,
        loop_backing: None,
        uuid: None,
        size: Bytes(size),
        other_used: Bytes(0),
        parent,
        read_only: false,
        dead: false,
        statx_missing: Vec::new(),
    };
    vec![
        mount(1, "/", "ext4", 1, 100 << 30, None),
        mount(TMP_MOUNT, "/tmp", "tmpfs", 42, 1 << 30, Some(1)),
    ]
}

/// A host model: [`base_mounts`], a tree at [`ROOT`] (at most `depth`
/// levels, `breadth` entries per directory), sometimes a further tmpfs
/// mounted on one of its directories, and up to `max_procs` processes
/// using it.
pub fn host_model(
    depth: u32,
    breadth: usize,
    max_procs: usize,
) -> impl Strategy<Value = HostModel> {
    tree(depth, breadth)
        .prop_flat_map(move |root| {
            let (dirs, files) = paths(Path::new(ROOT), &root);
            let procs: Vec<_> = (0..max_procs)
                .map(|i| process(4000 + i as u32, dirs.clone(), files.clone()))
                .collect();
            // Below the root, so the tree's own key stays on /tmp.
            let nested = match dirs.get(1..) {
                Some(below) if !below.is_empty() => {
                    proptest::option::of(select(below.to_vec())).boxed()
                }
                _ => Just(None).boxed(),
            };
            (Just(root), procs, 0..=max_procs, nested)
        })
        .prop_map(|(root, procs, live, nested)| {
            let mut mounts = base_mounts();
            if let Some(at) = nested {
                mounts.push(MountSpec {
                    id: TMP_MOUNT + 1,
                    at,
                    fs: "tmpfs".into(),
                    dev: DevSpec(Dev {
                        major: 0,
                        minor: 43,
                    }),
                    bind_of: None,
                    bind_root: None,
                    overlay_upper: None,
                    loop_backing: None,
                    uuid: None,
                    size: Bytes(64 << 20),
                    other_used: Bytes(0),
                    parent: Some(TMP_MOUNT),
                    read_only: false,
                    dead: false,
                    statx_missing: Vec::new(),
                });
            }
            HostModel {
                host_model: diskgc_model::spec::FORMAT_VERSION,
                clock: clock(),
                kernel: None,
                hostname: None,
                mounts,
                tree: BTreeMap::from([
                    (PathBuf::from(ROOT), NodeSpec::Dir(root)),
                    (
                        PathBuf::from(EXE),
                        NodeSpec::File(FileSpec {
                            size: Some(Bytes(1 << 20)),
                            ..FileSpec::default()
                        }),
                    ),
                ]),
                locks: Vec::new(),
                processes: procs.into_iter().take(live).collect(),
                own_pid: None,
                own_uid_map: None,
                own_uid: None,
                own_cap_effective: None,
                users: BTreeMap::new(),
                pid_namespace: Default::default(),
                proc_hidepid: false,
                loops: Vec::new(),
                sandbox_gate: diskgc_model::spec::GateSpec::Open,
                command_exit: 0,
                command_error_after_start: false,
                docker: Default::default(),
                pressure: None,
                call_cost: None,
                state_dir: None,
                home: None,
                boot_id: None,
                events: Vec::new(),
                expect: None,
            }
        })
}

/// What diskgc's held `/proc` can show (docs/safety.md, In use:
/// Visibility): mostly the initial PID namespace without `hidepid`, else a
/// nested namespace (a container), one inside a marked test container, or
/// `hidepid` (with either).
pub fn visibility() -> impl Strategy<Value = (PidNamespaceSpec, bool)> {
    let namespace = prop_oneof![
        3 => Just(PidNamespaceSpec::Initial),
        1 => Just(PidNamespaceSpec::Nested),
        1 => Just(PidNamespaceSpec::TestContainer),
    ];
    (namespace, prop_oneof![3 => Just(false), 1 => Just(true)])
}

/// Up to two configured loop devices (`7:0`, `7:1`) that no mount shows
/// (docs/safety.md, In use: Loop devices), each backed by one of `files`
/// (a generated tree's), by [`EXE`] (outside the tree), by a path that
/// names nothing here (another mount namespace's), or by a file since
/// deleted (` (deleted)`); each one diskgc can or cannot open for
/// `LOOP_GET_STATUS64`.
pub fn loop_devices(files: Vec<PathBuf>) -> impl Strategy<Value = Vec<LoopSpec>> {
    let mut texts: Vec<BoxedStrategy<PathBuf>> = vec![
        Just(PathBuf::from(EXE)).boxed(),
        Just(PathBuf::from("/var/lib/images/disk.img")).boxed(),
        Just(PathBuf::from(format!("{ROOT}/gone.img (deleted)"))).boxed(),
    ];
    if !files.is_empty() {
        texts.push(select(files).boxed());
    }
    let backing = proptest::strategy::Union::new(texts);
    proptest::collection::vec((backing, any::<bool>()), 0..=2).prop_map(|backings| {
        backings
            .into_iter()
            .enumerate()
            .map(|(minor, (backing, readable))| LoopSpec {
                dev: DevSpec(Dev {
                    major: 7,
                    minor: minor as u32,
                }),
                backing,
                file: None,
                readable,
            })
            .collect()
    })
}

/// [`host_model`], also drawing what the in-use scan cannot learn from
/// processes: [`visibility`] and [`loop_devices`] backed by the tree's
/// files.
pub fn host_model_with_kernel_holders(
    depth: u32,
    breadth: usize,
    max_procs: usize,
) -> impl Strategy<Value = HostModel> {
    host_model(depth, breadth, max_procs).prop_flat_map(|model| {
        let NodeSpec::Dir(root) = &model.tree[Path::new(ROOT)] else {
            unreachable!("host_model's root is a directory");
        };
        let (_, files) = paths(Path::new(ROOT), root);
        (Just(model), visibility(), loop_devices(files)).prop_map(
            |(mut model, (namespace, hidepid), loops)| {
                model.pid_namespace = namespace;
                model.proc_hidepid = hidepid;
                model.loops = loops;
                model
            },
        )
    })
}

/// `model` with one more event: `action` (an [`EventSpec`] whose `at` is
/// ignored) at call `k`, `when` before or after it. Property tests place an
/// event at every call of a scenario in turn.
pub fn with_event_at_call(model: &HostModel, k: u64, when: When, action: EventSpec) -> HostModel {
    let mut model = model.clone();
    model.events.push(EventSpec {
        at: At {
            call: Some(k),
            when: Some(when),
            ..At::default()
        },
        ..action
    });
    model
}
