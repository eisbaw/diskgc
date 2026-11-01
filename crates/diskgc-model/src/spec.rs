//! The host-model format (docs/testing.md, Model tests): what a YAML fixture
//! says, as plain serde data. Every struct denies unknown keys. References
//! (pids, paths, mount parents) are resolved by [`crate::load`], which turns
//! a [`HostModel`] into the state `ModelHost` answers from.
//!
//! The JSON Schema for editors is generated from these types
//! ([`json_schema`]) and committed as `host-model.schema.json` next to this
//! crate's manifest.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use diskgc_core::host::{Dev, Errno};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The only format version so far (`host_model: 1`).
pub const FORMAT_VERSION: u32 = 1;

/// A host model: the machine a model test runs against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostModel {
    /// Format version; must be 1.
    pub host_model: u32,
    /// Wall-clock time when the run starts (RFC 3339, e.g.
    /// `2026-10-15T03:00:00Z`). Default time stamp of tree entries.
    pub clock: WallTime,
    /// What `uname -r` reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// What `uname -n` reports; default `model-host`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// The mount table, in mount order (the order of mountinfo lines).
    /// Exactly one mount, at `/`, has no parent.
    pub mounts: Vec<MountSpec>,
    /// Files and directories, keyed by absolute path. Missing parent
    /// directories are created with default attributes; a key below another
    /// key adds entries to that directory.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tree: BTreeMap<PathBuf, NodeSpec>,
    /// `flock`s held by processes (each through an fd of its own).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locks: Vec<LockSpec>,
    /// The visible processes other than diskgc itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<ProcessSpec>,
    /// diskgc's own pid (`own_pid`; default 65000). It is listed among the
    /// pids; its fds are the ones the code under test holds open, its other
    /// `/proc` entries read as `Failed` (not modelled), its uid is
    /// `own_uid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_pid: Option<u32>,
    /// diskgc's own `uid_map` as it reads it; default the initial user
    /// namespace's (`none`: unreadable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_uid_map: Option<String>,
    /// diskgc's own user id (`geteuid`, and the uid its `status` shows);
    /// default 1000. `0` models diskgc run as root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_uid: Option<u32>,
    /// diskgc's own effective (and permitted) capability set as its own
    /// `proc_status` shows it (`CapEff:`, bit n is capability n; YAML
    /// takes `0x...`). Default: every capability up to
    /// `CAP_CHECKPOINT_RESTORE` (40) when `own_uid` is 0, as real root has
    /// them, and none otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_cap_effective: Option<u64>,
    /// The account database (`account_uid`, through which `owners:` names
    /// resolve): user name to uid. A name not listed has no account.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub users: BTreeMap<String, u32>,
    /// The PID namespace diskgc runs in, as its held `/proc` answers
    /// (docs/safety.md, Producer liveness; In use: Visibility): `initial`
    /// (the default), `nested`, or `test_container` (nested, the e2e
    /// harness's build in a marked test container:
    /// `Host::test_container_pid_namespace`).
    #[serde(default, skip_serializing_if = "PidNamespaceSpec::is_initial")]
    pub pid_namespace: PidNamespaceSpec,
    /// The held `/proc` is mounted with `hidepid`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub proc_hidepid: bool,
    /// Configured loop devices that no mount shows (`losetup`, `udisksctl
    /// loop-setup` without a mount, a VM's disk): with the mounts'
    /// [`MountSpec::loop_backing`], what `Host::loop_backing_files` lists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops: Vec<LoopSpec>,
    /// The pre-release sandbox gate as `LinuxHost` has it (docs/safety.md):
    /// `open` (the default; the model deletes nothing real), `closed`
    /// (outside a test container: `may_delete`, `unlinkat`, `fchmod` and
    /// `run_command` are refused with `Cause::SandboxGate`, nothing is
    /// attempted), or
    /// `refuses_deletes` (`may_delete` answers open but the deleting calls
    /// are refused: what a caller that asked first must still survive).
    #[serde(default, skip_serializing_if = "GateSpec::is_open")]
    pub sandbox_gate: GateSpec,
    /// Exit code returned by a modelled command action (default 0). A
    /// deadline already passed returns `DeadlineBeforeStart` instead. The model runs
    /// no child and changes no filesystem state for the command itself.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub command_exit: u8,
    /// Model an output-forwarding or wait failure after the command was
    /// spawned. The filesystem itself is unchanged by the model command.
    #[serde(default, skip_serializing_if = "is_false")]
    pub command_error_after_start: bool,
    /// Closed Docker query responses and client redirects for backend model
    /// tests. No real daemon or filesystem is contacted by ModelHost.
    #[serde(default, skip_serializing_if = "DockerSpec::is_empty")]
    pub docker: DockerSpec,
    /// The system-wide pressure stall information (`Host::pressure`) over
    /// the run: each step holds from its `after` (time since the start of
    /// the run) until the next step's. Absent: the kernel has no PSI
    /// (every read fails with `ENOENT`, as without `CONFIG_PSI`). Steps in
    /// increasing `after`, the first at `0s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<Vec<PressureStep>>,
    /// Virtual time every port call takes, but the clock reads and
    /// `sleep` (default none: calls take no time). For the background
    /// tally's duty cycle, whose batches are timed on the clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_cost: Option<Elapsed>,
    /// diskgc's state directory, as `LinuxHost` is given it
    /// (`$XDG_STATE_HOME/diskgc`): where the state operations keep the
    /// index snapshot. Absent: none (the state operations fail, as
    /// `LinuxHost` without one). Missing directories are created by
    /// `state_lock` (mode 0700, diskgc's own uid).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<PathBuf>,
    /// The home of diskgc's user as `LinuxHost` finds it for the per-user
    /// lock (its account entry's, else `$HOME`): the lock is
    /// `<home>/.local/state/diskgc/lock` unless `/run/user/<own_uid>` is a
    /// directory of diskgc's user with mode 0700 (then
    /// `/run/user/<own_uid>/diskgc/lock`; docs/safety.md, Host lock).
    /// Absent, and no such runtime directory: no place for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<PathBuf>,
    /// The account database lookup of diskgc's user's home fails with this
    /// errno (an NSS source unreachable: `getpwuid_r`'s error), so where
    /// the per-user lock is cannot be told unless `/run/user/<own_uid>` is
    /// the user's; `home` is then not used (TASK-288).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_lookup_error: Option<ErrnoName>,
    /// What `boot_id` reports; default a fixed id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// Changes applied between two `Host` calls, or at a point in time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<EventSpec>,
    /// What the scenario must produce (checked by the harness).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expect>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DockerSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_show: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<String>,
}

impl DockerSpec {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

fn is_zero_u8(value: &u8) -> bool {
    *value == 0
}

impl HostModel {
    /// The model as YAML (how property tests print a shrunk failing case,
    /// to be saved under `tests/regressions/`).
    pub fn to_yaml(&self) -> String {
        serde_saphyr::to_string(self).expect("a host model always serialises")
    }
}

/// The pressure readings from `after` on: the `some avg10` of `io`,
/// `memory` and `cpu`, in hundredths of a percent (`1000` is 10.00 %).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PressureStep {
    pub after: Elapsed,
    pub io: u32,
    pub memory: u32,
    pub cpu: u32,
}

/// The modelled sandbox gate (`sandbox_gate`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GateSpec {
    #[default]
    Open,
    Closed,
    RefusesDeletes,
}

impl GateSpec {
    pub fn is_open(&self) -> bool {
        *self == GateSpec::Open
    }
}

/// One mount.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MountSpec {
    /// Mount id (mountinfo, `stx_mnt_id`); unique, not 0.
    pub id: u64,
    /// Mount point.
    pub at: PathBuf,
    /// Filesystem type, e.g. `ext4`, `tmpfs`, `nfs`, `btrfs`.
    pub fs: String,
    /// Device number `major:minor`. Reuse requires `bind_of`.
    pub dev: DevSpec,
    /// Bind the filesystem of an earlier mount (its root, or `bind_root`).
    /// The device and filesystem metadata must match that mount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_of: Option<u64>,
    /// With `bind_of`: the directory of that filesystem this mount shows, as
    /// a path from the filesystem's root (mountinfo's root field; absent:
    /// `/`). Missing directories are created, as for a mount point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_root: Option<PathBuf>,
    /// An overlayfs mount (`fs: overlay`) whose upper directory is this
    /// path, on a mount declared earlier (missing directories are
    /// created): the mount shows that directory's tree, under its own
    /// `dev` in `statx` and mountinfo (root field `/`), so neither place nor
    /// identity relates the two names of one entry, and an unlink through
    /// either removes it from both. Its space is the upper's filesystem's
    /// (`size` is not used). Lower layers, whiteouts and copy-up are not
    /// modelled, and mountinfo shows no `upperdir=` (diskgc reads none).
    /// Only `statx`, mountinfo and `fstatfs` report the overlay: a memory
    /// map shows the upper filesystem's device (as Linux shows the
    /// layer's), and processes' open files and directories are matched by
    /// the object itself, whichever name opened it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_upper: Option<PathBuf>,
    /// For a filesystem on a loop device (`dev` of major 7): the file the
    /// device reads and writes (`/sys/dev/block/7:N/loop/backing_file`, as
    /// printed, ` (deleted)` included), a path of the model's tree or not.
    /// Absent on such a device: reading it fails (`EACCES`). The image's
    /// contents are not tied to that file's: deleting on the mount changes
    /// no byte of it in the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_backing: Option<PathBuf>,
    /// A btrfs subvolume (`fs: btrfs`): the device `statx` reports for
    /// every object reached through this mount (the subvolume's anonymous
    /// device), while mountinfo keeps `dev`, the filesystem's superblock's
    /// (TASK-286). Two mounts of subvolumes of one btrfs share `dev` and
    /// their space and differ here. A per-mount simplification: Linux
    /// reports the device of the subvolume an inode belongs to, whichever
    /// mount reaches it, and a nested subvolume (not mounted) is not
    /// modelled. Absent: `statx` reports `dev`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subvolume: Option<DevSpec>,
    /// An idmapped mount (mountinfo shows `idmapped` among its per-mount
    /// options): every owner and group `statx` reports through it is the
    /// stored id mapped by this list, `stored` shown as `shown`; an id the
    /// list does not name shows as 65534 (the overflow id, as Linux shows
    /// an unmapped one). Listing permissions use the shown owner too. A
    /// bind of it made by an event keeps it. Not on an overlay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub idmap: Vec<IdMapSpec>,
    /// A btrfs on an anonymous device (`fs: btrfs`, `dev` of major 0): the
    /// block devices it is stored on, in order, as `BTRFS_IOC_FS_INFO` on
    /// a descriptor of it and `BTRFS_IOC_DEV_INFO` list them
    /// (`loop_devices`); a loop device among them (major 7) is one of
    /// `loops:`. mountinfo's source names only the first. Partitions of a
    /// loop device are not modelled. Not with `bind_of` (a bind shows the
    /// filesystem's own).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<DevSpec>,
    /// The filesystem's UUID as `fs_uuid` reports it (the udev link
    /// `/dev/disk/by-uuid`); absent: none (tmpfs, no udev).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Filesystem size.
    pub size: Bytes,
    /// Space used by things outside the model; available space is `size -
    /// other_used -` the blocks of every live inode.
    #[serde(default, skip_serializing_if = "Bytes::is_zero")]
    pub other_used: Bytes,
    /// The mount this one is mounted on (stacked mounts: the mount at the
    /// same point it covers). Required except for the mount at `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    /// The filesystem's server never answers (a dead FUSE daemon or NFS
    /// server): `mountinfo` still lists the mount, but a call that reaches
    /// it (`openat2` to or below it, `statx`, `getdents`, a read, `statfs`,
    /// `statvfs`) would hang, so the model fails the test at that call. A
    /// bind of a dead mount needs its own `dead: true`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dead: bool,
    /// `statx` fields the filesystem does not answer (kernel capabilities):
    /// they come back as `None`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub statx_missing: Vec<StatxField>,
}

/// One id of an idmapped mount ([`MountSpec::idmap`]): the owner or group
/// `stored` on the filesystem, shown through the mount as `shown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IdMapSpec {
    pub stored: u32,
    pub shown: u32,
}

/// A `statx` field that can be missing from the answer.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StatxField {
    Type,
    Mode,
    Nlink,
    Uid,
    Gid,
    Ino,
    Size,
    Blocks,
    Mtime,
    Ctime,
    Btime,
    MntId,
    MountRoot,
    /// `stx_subvol` (`STATX_SUBVOL`, Linux 6.10 on): reported on `btrfs`
    /// and `bcachefs` mounts only, as Linux does; listed here for a kernel
    /// before 6.10.
    Subvol,
}

/// A tree node. A mapping of scalars is a file (its attributes; `{}` is an
/// empty file); a mapping of mappings is a directory (its entries, plus its
/// own attributes under the key `.`, which no entry can be called; `{.:
/// {}}` is an empty directory). Mixing both is an error.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeSpec {
    File(FileSpec),
    Dir(DirSpec),
}

/// A non-directory: regular file (default), symlink, hard link, socket or
/// FIFO.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    /// `socket`, `fifo` or `char_device`; a regular file if absent.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub special: Option<Special>,
    /// A symlink with this target text (never followed by the model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink: Option<PathBuf>,
    /// A hard link: another name of the file at this absolute path (same
    /// filesystem). Takes no other attributes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<PathBuf>,
    /// File size; allocated space is the size rounded up to 4 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Bytes>,
    /// Modification time (also ctime and birth time); default `clock`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<WallTime>,
    /// Owner; default 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// Group; default 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    /// Permission bits in octal, e.g. `"0644"` (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// Holds a valid `CACHEDIR.TAG` signature.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cachedir_tag: bool,
    /// UTF-8 bytes returned by `read_head` for a regular file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Only with `type: socket`: a socket is bound to this file in
    /// diskgc's network namespace (`bound_unix_sockets` reports it).
    #[serde(default, skip_serializing_if = "is_false")]
    pub bound: bool,
}

/// `socket`, `fifo` or `char_device`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Special {
    Socket,
    Fifo,
    /// A character device node (only its type is modelled).
    CharDevice,
}

/// A directory: its own attributes and its entries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DirSpec {
    pub attrs: DirAttrs,
    pub entries: BTreeMap<String, NodeSpec>,
}

/// A directory's own attributes (the `.` key).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DirAttrs {
    /// Modification time (also ctime and birth time); default `clock`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<WallTime>,
    /// Owner; default 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// Group; default 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    /// Permission bits in octal; default `"0755"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// The top of a nested btrfs subvolume or snapshot (not mounted):
    /// `statx` reports this device for it and everything in its subtree
    /// (TASK-286 review), while mountinfo shows nothing there. A nested one
    /// inside it overrides it below its own top. On a `btrfs` mount only.
    /// Entries created later inside it take it too; an `rmdir` of its
    /// empty top succeeds, as Linux 4.18 and later allow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subvolume: Option<DevSpec>,
    /// The top of a nested bcachefs subvolume or snapshot (not mounted),
    /// with this subvolume id: `statx` reports it as `stx_subvol` for the
    /// top and its subtree and keeps the filesystem's device, as bcachefs
    /// does (TASK-307: unlike btrfs, no device of its own). On a
    /// `bcachefs` mount only; the filesystem's own subvolume is 1. A
    /// nested one overrides it below its own top; entries created later
    /// inside it take it too; an `rmdir` of its empty top succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subvol: Option<u64>,
}

/// A `flock` held by a process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LockSpec {
    pub path: PathBuf,
    /// The pid holding it; must be one of `processes`.
    pub holder: u32,
    pub mode: LockModeSpec,
}

/// `shared` or `exclusive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LockModeSpec {
    Shared,
    Exclusive,
}

/// A process, as its `/proc` entry shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessSpec {
    pub pid: u32,
    /// Its executable (`/proc/<pid>/exe`); none if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<PathBuf>,
    /// Working directory; default `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Root directory; default `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
    /// Initial environment, as `NAME=value` entries in order (an entry
    /// without `=` is kept as such).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environ: Vec<String>,
    /// Open files (`flock`s held go in `locks`, which add fds of their own).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fds: Vec<FdSpec>,
    /// Files it has mapped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub maps: Vec<PathBuf>,
    /// How reading its `/proc` entry goes.
    #[serde(default, skip_serializing_if = "ProcAccess::is_readable")]
    pub proc: ProcAccess,
    /// Its command name (`Name:` in `status`); default: the last name of
    /// `exe`, or `?`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Its real, saved and filesystem uid; default 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// Its effective uid (and saved, as after a setuid exec); default
    /// `uid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub euid: Option<u32>,
    /// Its four gids; default 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    /// Not dumpable: its `/proc/<pid>/fd` is owned by root (0) instead of
    /// its effective uid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub non_dumpable: bool,
    /// Its `/proc/<pid>/uid_map` as diskgc reads it; default the initial
    /// user namespace's, diskgc's own (another text: a user namespace of
    /// its own, e.g. `"0 1000 1"` after `unshare -r`; `none`: unreadable,
    /// as on a kernel without user namespaces).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid_map: Option<String>,
    /// Holds a permitted capability (`CAP_WAKE_ALARM`, as `systemd --user`
    /// does) that diskgc lacks.
    #[serde(default, skip_serializing_if = "is_false")]
    pub privileged: bool,
    /// Its `NSpid:` line; default `[pid]` (diskgc's namespace is its own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nspid: Option<NspidSpec>,
    /// Exited but not reaped: listed, its `status` (and `NSpid`) readable,
    /// its pidfd open, every other reader `Gone` (no live task). Has no
    /// fds or maps.
    #[serde(default, skip_serializing_if = "is_false")]
    pub zombie: bool,
    /// Its command line (`/proc/<pid>/cmdline`), one string per argument;
    /// default `[exe]` (the executable's path, or nothing without one);
    /// `unreadable`: every read fails (`Failed`). Readable whatever
    /// `proc:` says, as on Linux; a zombie's reads empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<CmdlineSpec>,
    /// Path prefixes as its root and mount namespace see them, mapped to
    /// the model's paths (`{/tmp: /tmp/nix-shell.AB}`: its `/tmp` is the
    /// model's `/tmp/nix-shell.AB`), used only by temp-root lookups
    /// (`proc_root_lookup`); the longest matching prefix wins, and a path
    /// no prefix matches is below its `root`. Default: none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub view: BTreeMap<PathBuf, PathBuf>,
}

/// A process's command line, or `unreadable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum CmdlineSpec {
    Args(Vec<String>),
    Unreadable(UnreadableTag),
}

/// Which PID namespace diskgc runs in, as Owner PID's precondition and the
/// in-use scan's see it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PidNamespaceSpec {
    /// The initial PID namespace (nsfs inode `0xEFFFFFFC`).
    #[default]
    Initial,
    /// Any other (a container, `unshare --pid`).
    Nested,
    /// Nested, and diskgc is the e2e harness's build inside a marked
    /// disposable test container: the in-use scan assumes the namespace
    /// complete, for tests; Owner PID does not (docs/safety.md, In use:
    /// Visibility). Refused with `sandbox_gate: closed`.
    TestContainer,
}

/// One configured loop device that no mount shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoopSpec {
    /// The loop block device (major 7).
    pub dev: DevSpec,
    /// Its `loop/backing_file` as the kernel prints it (` (deleted)`
    /// included), a path of the model's tree or not. When it is the held
    /// file's own path (no `file`), later reads follow that file's renames
    /// and deletion as `d_path` does.
    pub backing: PathBuf,
    /// The file the device holds, by the name it opened, when the text
    /// does not reach it here (another mount namespace's rendering, or a
    /// mount made over its directory since: a decoy). Default: `backing`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    /// diskgc can open the block device for `LOOP_GET_STATUS64` (the `disk`
    /// group); root always can. Default: it cannot.
    #[serde(default, skip_serializing_if = "is_false")]
    pub readable: bool,
}

impl PidNamespaceSpec {
    fn is_initial(&self) -> bool {
        *self == Self::Initial
    }
}

/// A process's `NSpid:` line: its pids from diskgc's namespace down to its
/// own (the first must be its `pid`), or `unreadable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum NspidSpec {
    Levels(Vec<u32>),
    Unreadable(UnreadableTag),
}

/// The word `unreadable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnreadableTag {
    Unreadable,
}

/// The outcome of every `/proc/<pid>` read of a process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcAccess {
    /// Answered from the model. As on `LinuxHost`, an empty fd table,
    /// memory map or environment, or no exe, is `ProcError::Failed`.
    #[default]
    Readable,
    /// `ProcError::Denied` (`EACCES`): another user's or a non-dumpable
    /// process.
    Denied,
    /// `ProcError::Failed`: a live process that could not be inspected.
    Failed,
}

impl ProcAccess {
    fn is_readable(&self) -> bool {
        *self == Self::Readable
    }
}

/// An open file of a process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FdSpec {
    /// The fd number; default: the lowest free one from 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fd: Option<u32>,
    pub path: PathBuf,
    /// The file was deleted after it was opened: its name is removed when
    /// the model loads, and it keeps its space while it is open.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

/// A change to the model, applied when its trigger `at` is reached. Exactly
/// one action key besides `at`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventSpec {
    pub at: At,
    /// Deliver SIGINT or SIGTERM to the modelled diskgc run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<ModelSignal>,
    /// Create an empty regular file (and missing parent directories).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<PathBuf>,
    /// Create a directory (and missing parents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mkdir: Option<PathBuf>,
    /// Remove a name and, for a directory, every entry below it (`rm -r`).
    /// An object still open or mapped keeps existing, but only itself: an
    /// open directory stays, empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<PathBuf>,
    /// `rename(from, to)`: `to` may exist if both are files, or both
    /// directories and `to` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename: Option<RenameSpec>,
    /// Replace the object at the path by a new, empty one of the same type
    /// (a new inode). The old one is detached from every modelled
    /// directory, as if renamed away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace: Option<PathBuf>,
    /// Replace a file or directory with a fresh object deliberately assigned its
    /// old inode number. Simulates reuse after unlink for identity tests;
    /// the old object must have no other names or open references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_reusing_inode: Option<PathBuf>,
    /// Change the owner of the object at a path (not following it); the
    /// inode stays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chown: Option<ChownSpec>,
    /// A process opens a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<ProcPath>,
    /// A process closes every fd it opened on a path (releasing their
    /// locks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close: Option<ProcPath>,
    /// A process exits: its fds close and its pid is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<u32>,
    /// A process forks: a new process `child`, a copy of `pid` whose fds
    /// share `pid`'s open file descriptions (and their locks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<ForkSpec>,
    /// A process becomes dumpable (`true`) or not (`false`): what the owner
    /// of its `fd` directory shows from then on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dumpable: Option<DumpableSpec>,
    /// How reading a process's `/proc` entry goes from then on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<AccessSpec>,
    /// The clock jumps forward (e.g. a suspend).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advance: Option<Elapsed>,
    /// The call the event is addressed to fails with this errno, given by
    /// name (e.g. `EIO`, `EACCES`), without acting.
    /// Only with an `op` trigger, `when: before`, and an op that can fail
    /// this way: `openat2`, `statx`, `read_link`, `read_dir`, `read_head`,
    /// `unlinkat`, `fchmod`, `bound_unix_sockets`, `statvfs`, the `/proc`
    /// readers (`proc_identity`, `proc_identity_released`, `proc_fds`,
    /// `proc_links`, `proc_maps`, `proc_environ`, `proc_status`,
    /// `proc_uids`, `proc_cmdline`, `proc_root_lookup`, `pid_namespace`,
    /// `proc_nspid`, `pid_alive`), `docker_query` (e.g.
    /// `ENOENT`: no Docker client installed) or `run_command` (the child
    /// does not start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail: Option<ErrnoName>,
    /// A new filesystem is mounted on an existing directory (a user's
    /// FUSE mount appearing in a candidate while diskgc works).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount: Option<MountEventSpec>,
    /// The space other writers use on a mount's filesystem changes (its
    /// `other_used`): available space falls or rises without a file of the
    /// model changing (`watch`'s scripted curve).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_used: Option<OtherUsedSpec>,
    /// An account of the name service appears, changes its uid, or
    /// (`uid: null`) disappears (`owners:` names resolve again at every
    /// reclaim of a `watch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountSpec>,
}

/// An account name and its uid from now on (`None`: no such account).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountSpec {
    pub name: String,
    pub uid: Option<u32>,
}

/// A mount, by its mount point, and its filesystem's new `other_used`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OtherUsedSpec {
    pub mount: PathBuf,
    pub bytes: Bytes,
}

/// Signals the model can deliver to diskgc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelSignal {
    Int,
    Term,
}

/// A mount an event makes on the existing directory `at`, on the mount that
/// directory is on: a fresh, empty filesystem of type `fs`, or with
/// `bind_of` a bind of an existing mount (TASK-285 review: a mount made
/// after the protected matches were enumerated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MountEventSpec {
    /// Mount id; unique, not 0.
    pub id: u64,
    pub at: PathBuf,
    /// Filesystem type, e.g. `fuse`, `nfs4`; with `bind_of`, the source
    /// mount's (`overlay` for an overlay).
    pub fs: String,
    /// Device number `major:minor`, not one already mounted; with
    /// `bind_of`, the one the source mount shows.
    pub dev: DevSpec,
    /// Ignored with `bind_of` (the source's filesystem's space).
    pub size: Bytes,
    /// Bind the filesystem an existing mount shows (overlays included: the
    /// bind shows the same overlay), its root or `bind_root`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_of: Option<u64>,
    /// With `bind_of`: the directory of the source mount's filesystem the
    /// bind shows, as a path from that filesystem's root (mountinfo's root
    /// field); it must exist. Not for an overlay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_root: Option<PathBuf>,
}

/// A process and how reading its `/proc` entry goes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccessSpec {
    pub pid: u32,
    pub proc: ProcAccess,
}

/// A process and whether it is dumpable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DumpableSpec {
    pub pid: u32,
    pub value: bool,
}

/// A process and the pid of the child it forks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForkSpec {
    pub pid: u32,
    pub child: u32,
}

/// A path and its new owner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChownSpec {
    pub path: PathBuf,
    pub uid: u32,
}

/// Source and destination of a rename.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameSpec {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// A process and a path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcPath {
    pub pid: u32,
    pub path: PathBuf,
}

/// When an event fires: at the `nth` call of `op` (on `path`, if given), at
/// the `call`-th call of any kind, or once the clock has run `elapsed`
/// since the start. Call triggers need `when`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct At {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<Op>,
    /// The path the call acts on (an fd's path joined with the call's
    /// relative path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Which matching call, from 1; default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nth: Option<u32>,
    /// The index of the call in the whole trace, from 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<u64>,
    /// Time since the start of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed: Option<Elapsed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<When>,
}

/// Before the call runs, or after it ran (before it returns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum When {
    Before,
    After,
}

/// A `Host` operation, named after its syscall (`flock` covers both lock
/// calls).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Mountinfo,
    Openat2,
    Statx,
    ExecutableFile,
    ReadLink,
    ReadDir,
    // `Host::listable`: the open a listing starts with.
    Listable,
    ReadHead,
    ReopenRegular,
    Statvfs,
    Flock,
    Now,
    WallClock,
    Sleep,
    KernelRelease,
    Hostname,
    Pids,
    ProcIdentity,
    ProcIdentityReleased,
    ProcFds,
    ProcLinks,
    ProcMaps,
    ProcEnviron,
    OwnPid,
    EffectiveUid,
    AccountUid,
    InheritLock,
    ProcStatus,
    BoundUnixSockets,
    PidNamespace,
    ProcNspid,
    PidAlive,
    FsMagic,
    ProcUids,
    ProcCmdline,
    ProcRootLookup,
    MayDelete,
    Unlinkat,
    Fchmod,
    RunCommand,
    DockerQuery,
    Pressure,
    LowerPriority,
    FsUuid,
    LoopBackingFile,
    LoopBackingFiles,
    LoopBackingIdentity,
    LoopDevices,
    BootId,
    StateLock,
    StateRead,
    StateWrite,
    UserLock,
    UserLockPath,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Self::Mountinfo => "mountinfo",
            Self::Openat2 => "openat2",
            Self::Statx => "statx",
            Self::ExecutableFile => "executable_file",
            Self::ReadLink => "read_link",
            Self::ReadDir => "read_dir",
            Self::Listable => "listable",
            Self::ReadHead => "read_head",
            Self::ReopenRegular => "reopen_regular",
            Self::Statvfs => "statvfs",
            Self::Flock => "flock",
            Self::Now => "now",
            Self::WallClock => "wall_clock",
            Self::Sleep => "sleep",
            Self::KernelRelease => "kernel_release",
            Self::Hostname => "hostname",
            Self::Pids => "pids",
            Self::ProcIdentity => "proc_identity",
            Self::ProcIdentityReleased => "proc_identity_released",
            Self::ProcFds => "proc_fds",
            Self::ProcLinks => "proc_links",
            Self::ProcMaps => "proc_maps",
            Self::ProcEnviron => "proc_environ",
            Self::OwnPid => "own_pid",
            Self::EffectiveUid => "effective_uid",
            Self::AccountUid => "account_uid",
            Self::InheritLock => "inherit_lock",
            Self::ProcStatus => "proc_status",
            Self::BoundUnixSockets => "bound_unix_sockets",
            Self::PidNamespace => "pid_namespace",
            Self::ProcNspid => "proc_nspid",
            Self::PidAlive => "pid_alive",
            Self::FsMagic => "fs_magic",
            Self::ProcUids => "proc_uids",
            Self::ProcCmdline => "proc_cmdline",
            Self::ProcRootLookup => "proc_root_lookup",
            Self::MayDelete => "may_delete",
            Self::Unlinkat => "unlinkat",
            Self::Fchmod => "fchmod",
            Self::RunCommand => "run_command",
            Self::DockerQuery => "docker_query",
            Self::Pressure => "pressure",
            Self::LowerPriority => "lower_priority",
            Self::FsUuid => "fs_uuid",
            Self::LoopBackingFile => "loop_backing_file",
            Self::LoopBackingFiles => "loop_backing_files",
            Self::LoopBackingIdentity => "loop_backing_identity",
            Self::LoopDevices => "loop_devices",
            Self::BootId => "boot_id",
            Self::StateLock => "state_lock",
            Self::StateRead => "state_read",
            Self::StateWrite => "state_write",
            Self::UserLock => "user_lock",
            Self::UserLockPath => "user_lock_path",
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a scenario must produce. Only the keys given are checked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The exit code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// The skipped candidates, in report order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<Skipped>>,
}

/// A skipped candidate and its reason (docs/reference.md, JSON output).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Skipped {
    pub candidate: PathBuf,
    pub reason: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

// ---- Tree nodes: mappings of scalars or of mappings ----

impl NodeSpec {
    /// Classifies a parsed mapping (see [`NodeSpec`]).
    fn from_value(value: serde_json::Value) -> Result<Self, String> {
        let serde_json::Value::Object(map) = value else {
            return Err(format!(
                "a tree node must be a mapping (file attributes such as {{size: 1G}}, or \
                 directory entries), found {value}"
            ));
        };
        let nested = map.values().filter(|v| v.is_object()).count();
        if nested == 0 {
            return serde_json::from_value(serde_json::Value::Object(map))
                .map(NodeSpec::File)
                .map_err(|e| e.to_string());
        }
        if nested != map.len() {
            let scalars: Vec<&str> = map
                .iter()
                .filter(|(_, v)| !v.is_object())
                .map(|(k, _)| k.as_str())
                .collect();
            return Err(format!(
                "mixes file attributes {scalars:?} with directory entries; a directory's own \
                 attributes go under the key '.'"
            ));
        }
        let mut dir = DirSpec::default();
        for (name, value) in map {
            if name == "." {
                dir.attrs = serde_json::from_value(value).map_err(|e| format!(".: {e}"))?;
                continue;
            }
            if name.is_empty() || name == ".." || name.contains('/') {
                return Err(format!(
                    "{name:?} is not a directory entry name (one component, not '..')"
                ));
            }
            let node = Self::from_value(value).map_err(|e| format!("{name}: {e}"))?;
            dir.entries.insert(name, node);
        }
        Ok(NodeSpec::Dir(dir))
    }
}

impl<'de> Deserialize<'de> for NodeSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        NodeSpec::from_value(value).map_err(D::Error::custom)
    }
}

impl Serialize for NodeSpec {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            NodeSpec::File(file) => file.serialize(s),
            NodeSpec::Dir(dir) => {
                // An empty directory still needs '.' to read back as one.
                let attrs = dir.attrs != DirAttrs::default() || dir.entries.is_empty();
                let mut map = s.serialize_map(Some(dir.entries.len() + usize::from(attrs)))?;
                if attrs {
                    map.serialize_entry(".", &dir.attrs)?;
                }
                for (name, node) in &dir.entries {
                    map.serialize_entry(name, node)?;
                }
                map.end()
            }
        }
    }
}

impl JsonSchema for NodeSpec {
    fn schema_name() -> Cow<'static, str> {
        "Node".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let file = generator.subschema_for::<FileSpec>();
        let attrs = generator.subschema_for::<DirAttrs>();
        let node = generator.subschema_for::<NodeSpec>();
        json_schema!({
            "description": "A file (a mapping of attributes; {} is an empty file) or a directory \
                (a mapping of entry names to nodes, with its own attributes under '.'; {.: {}} \
                is an empty directory).",
            "oneOf": [
                file,
                {
                    "type": "object",
                    "properties": { ".": attrs },
                    "additionalProperties": node,
                    "minProperties": 1
                }
            ]
        })
    }
}

// ---- Scalars written as text ----

/// Implements serde and JSON Schema for a type written as a string, through
/// its `FromStr` and `Display`.
macro_rules! text_scalar {
    ($ty:ident, $name:literal, $pattern:literal, $description:literal) => {
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let text = String::deserialize(d)?;
                text.parse().map_err(D::Error::custom)
            }
        }

        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }

        impl JsonSchema for $ty {
            fn schema_name() -> Cow<'static, str> {
                $name.into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "type": "string",
                    "pattern": $pattern,
                    "description": $description
                })
            }
        }
    };
}

/// A byte count: an integer with an optional unit `K`, `M`, `G` or `T`
/// (powers of 1024, as in docs/reference.md).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bytes(pub u64);

const UNITS: [(char, u32); 4] = [('T', 40), ('G', 30), ('M', 20), ('K', 10)];

impl Bytes {
    fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

impl FromStr for Bytes {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let bad = || format!("{text:?} is not a size (digits and an optional K, M, G or T)");
        let (digits, shift) = match UNITS.iter().find(|(unit, _)| text.ends_with(*unit)) {
            Some((_, shift)) => (&text[..text.len() - 1], *shift),
            None => (text, 0),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let n: u64 = digits.parse().map_err(|_| bad())?;
        n.checked_mul(1 << shift)
            .map(Bytes)
            .ok_or_else(|| format!("{text:?} overflows 64 bits"))
    }
}

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (unit, shift) in UNITS {
            if self.0 != 0 && self.0.is_multiple_of(1 << shift) {
                return write!(f, "{}{unit}", self.0 >> shift);
            }
        }
        write!(f, "{}", self.0)
    }
}

/// A size is text (`8G`) or a plain integer (bytes).
impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Count(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Count(n) => Ok(Bytes(n)),
            Raw::Text(text) => text.parse().map_err(D::Error::custom),
        }
    }
}

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl JsonSchema for Bytes {
    fn schema_name() -> Cow<'static, str> {
        "Bytes".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A size: digits and an optional K, M, G or T (powers of 1024), or a number of bytes.",
            "anyOf": [
                { "type": "string", "pattern": "^[0-9]+[KMGT]?$" },
                { "type": "integer", "minimum": 0 }
            ]
        })
    }
}

/// A duration: an integer and a unit `ms`, `s`, `m`, `h` or `d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Elapsed(pub Duration);

const DURATION_UNITS: [(&str, u64); 5] = [
    ("d", 86_400_000),
    ("h", 3_600_000),
    ("m", 60_000),
    ("s", 1_000),
    ("ms", 1),
];

impl FromStr for Elapsed {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let bad = || format!("{text:?} is not a duration (digits and ms, s, m, h or d)");
        let split = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
        let (digits, unit) = text.split_at(split);
        let millis = DURATION_UNITS
            .iter()
            .find(|(name, _)| *name == unit)
            .map(|(_, millis)| *millis)
            .ok_or_else(bad)?;
        let n: u64 = digits.parse().map_err(|_| bad())?;
        n.checked_mul(millis)
            .map(|ms| Elapsed(Duration::from_millis(ms)))
            .ok_or_else(|| format!("{text:?} overflows"))
    }
}

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let millis = self.0.as_millis();
        for (unit, per) in DURATION_UNITS {
            let per = u128::from(per);
            if millis != 0 && millis.is_multiple_of(per) {
                return write!(f, "{}{unit}", millis / per);
            }
        }
        write!(f, "0s")
    }
}

text_scalar!(
    Elapsed,
    "Elapsed",
    "^[0-9]+(ms|s|m|h|d)$",
    "A duration: digits and ms, s, m, h or d."
);

/// A point in wall-clock time, written in RFC 3339.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WallTime(pub jiff::Timestamp);

impl FromStr for WallTime {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        text.parse()
            .map(WallTime)
            .map_err(|e| format!("{text:?} is not an RFC 3339 time: {e}"))
    }
}

impl fmt::Display for WallTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

text_scalar!(
    WallTime,
    "WallTime",
    "^[0-9]{4}-[0-9]{2}-[0-9]{2}T",
    "A time in RFC 3339, e.g. 2026-10-15T03:00:00Z."
);

/// A device number written `major:minor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DevSpec(pub Dev);

impl FromStr for DevSpec {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let bad = || format!("{text:?} is not a device number major:minor");
        let (major, minor) = text.split_once(':').ok_or_else(bad)?;
        let number = |s: &str| -> Result<u32, String> {
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            s.parse().map_err(|_| bad())
        };
        Ok(DevSpec(Dev {
            major: number(major)?,
            minor: number(minor)?,
        }))
    }
}

impl fmt::Display for DevSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

text_scalar!(
    DevSpec,
    "Dev",
    "^[0-9]+:[0-9]+$",
    "A device number major:minor, e.g. \"254:1\"."
);

/// Permission bits, written in octal as a string (`"0755"`), since YAML
/// versions disagree on what a leading zero means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode(pub u32);

impl FromStr for Mode {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let bad = || format!("{text:?} is not an octal mode such as \"0755\"");
        if !(3..=4).contains(&text.len()) || !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            return Err(bad());
        }
        u32::from_str_radix(text, 8).map(Mode).map_err(|_| bad())
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04o}", self.0)
    }
}

text_scalar!(
    Mode,
    "Mode",
    "^[0-7]{3,4}$",
    "Permission bits in octal, as a string, e.g. \"0755\"."
);

/// An errno by its symbolic name, e.g. `EACCES` (one diskgc knows:
/// `Errno::NAMED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrnoName(pub Errno);

impl FromStr for ErrnoName {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        Errno::from_name(text).map(ErrnoName).ok_or_else(|| {
            let known: Vec<&str> = Errno::NAMED.iter().filter_map(|e| e.name()).collect();
            format!("{text:?} is not an errno name diskgc knows ({known:?})")
        })
    }
}

impl fmt::Display for ErrnoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.name().expect("made from a name"))
    }
}

text_scalar!(
    ErrnoName,
    "ErrnoName",
    "^E[A-Z]+$",
    "An errno by its symbolic name, e.g. \"EACCES\"."
);

/// The JSON Schema of the host-model format.
pub fn json_schema() -> Schema {
    schemars::schema_for!(HostModel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_in_powers_of_1024_and_print_back() {
        assert_eq!("8G".parse::<Bytes>(), Ok(Bytes(8 << 30)));
        assert_eq!("1000".parse::<Bytes>(), Ok(Bytes(1000)));
        assert_eq!("3K".parse::<Bytes>(), Ok(Bytes(3072)));
        for bad in ["", "G", "8GB", "-1", "1.5G", "8 G", "99999999999T"] {
            assert!(bad.parse::<Bytes>().is_err(), "{bad:?}");
        }
        for text in ["8G", "1000", "3K", "0", "1025"] {
            assert_eq!(text.parse::<Bytes>().unwrap().to_string(), text);
        }
    }

    #[test]
    fn durations_modes_and_devices_parse() {
        assert_eq!("5s".parse::<Elapsed>(), Ok(Elapsed(Duration::from_secs(5))));
        assert_eq!(
            "250ms".parse::<Elapsed>(),
            Ok(Elapsed(Duration::from_millis(250)))
        );
        assert_eq!("90s".parse::<Elapsed>().unwrap().to_string(), "90s");
        assert_eq!("120s".parse::<Elapsed>().unwrap().to_string(), "2m");
        for bad in ["5", "s", "5x", "1.5s", "-1s"] {
            assert!(bad.parse::<Elapsed>().is_err(), "{bad:?}");
        }
        assert_eq!("0755".parse::<Mode>(), Ok(Mode(0o755)));
        assert_eq!("644".parse::<Mode>(), Ok(Mode(0o644)));
        for bad in ["0855", "75", "07555", "rwx"] {
            assert!(bad.parse::<Mode>().is_err(), "{bad:?}");
        }
        assert_eq!(
            "254:1".parse::<DevSpec>(),
            Ok(DevSpec(Dev {
                major: 254,
                minor: 1
            }))
        );
        for bad in ["254", "254:", ":1", "a:b", "1:2:3"] {
            assert!(bad.parse::<DevSpec>().is_err(), "{bad:?}");
        }
    }
}
