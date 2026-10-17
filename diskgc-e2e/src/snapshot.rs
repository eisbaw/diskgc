//! Whole-container filesystem snapshots for the deletion oracle
//! (docs/testing.md, Grounding, acceptance signal 1).
//!
//! A snapshot maps every path reachable from `/` (crossing into every real
//! mount: the image's overlay root, the scratch tmpfs, `/dev`, bind mounts)
//! to the identity of the object there: (device, inode, birth time). Pseudo
//! filesystems (`/proc`, `/sys`, cgroups, `devpts`, ...) are skipped: they
//! hold no files diskgc could delete and their contents change on their own.
//! Read-only mounts (per mount or per superblock; e.g. the binaries under
//! test, later the toolchains) are skipped too: nothing can be deleted
//! through them, and walking them would only make every snapshot slower.
//! Writable mounts nested below a skipped read-only mount are still walked.
//! Which mount an object is on is decided by its `statx` mount id, so a mount
//! stacked over another at the same path is judged by the one on top.
//! Symlinks are recorded, never followed.
//!
//! The key falls back to (device, inode) where the filesystem reports no
//! birth time, and on overlayfs (the container's root): a copy-up (e.g. of a
//! directory whose child is deleted) keeps the overlay inode number but gives
//! the object the birth time of its new upper inode, which would read as a
//! replacement. [`KeyStats`] counts which key was used per filesystem so the
//! harness logs it.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, StatxFlags};

/// Filesystem types never walked (their mount point and everything below).
pub const PSEUDO_FS_TYPES: &[&str] = &[
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devpts",
    "mqueue",
    "debugfs",
    "tracefs",
    "securityfs",
    "pstore",
    "bpf",
    "configfs",
    "fusectl",
    "hugetlbfs",
    "binfmt_misc",
    "autofs",
    "nsfs",
    "efivarfs",
    "rpc_pipefs",
];

/// Filesystem types whose birth times are not part of the key (see the
/// module documentation).
pub const BTIME_UNSTABLE_FS_TYPES: &[&str] = &["overlay"];

/// Identity of the object found at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub dev: (u32, u32),
    pub ino: u64,
    /// Birth time (seconds, nanoseconds) when the filesystem reports one.
    pub btime: Option<(i64, u32)>,
}

impl Key {
    /// Whether `self` and `other` identify the same object, using the
    /// strongest key both sides have (with birth time only when both have it).
    pub fn same_object(&self, other: &Key) -> bool {
        if self.dev != other.dev || self.ino != other.ino {
            return false;
        }
        match (self.btime, other.btime) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }

    /// Name of the key this object is compared by, for logs and reports.
    pub fn kind(&self) -> &'static str {
        if self.btime.is_some() {
            "(path, dev, ino, btime)"
        } else {
            "(path, dev, ino)"
        }
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dev {}:{} ino {}", self.dev.0, self.dev.1, self.ino)?;
        match self.btime {
            Some((s, ns)) => write!(f, " btime {s}.{ns:09}"),
            None => write!(f, " btime n/a"),
        }
    }
}

/// What kind of object is at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One recorded object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub key: Key,
}

/// How many objects were keyed with and without birth time, per filesystem
/// (labelled `<fs type> <major>:<minor>`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KeyStats {
    pub with_btime: BTreeMap<String, usize>,
    pub without_btime: BTreeMap<String, usize>,
}

impl std::fmt::Display for KeyStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total = |m: &BTreeMap<String, usize>| m.values().sum::<usize>();
        let devs = |m: &BTreeMap<String, usize>| {
            m.iter()
                .map(|(fs, n)| format!("{fs}={n}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        write!(
            f,
            "key (path, dev, ino, btime) for {} objects [{}]; fallback (path, dev, ino) for {} objects [{}]",
            total(&self.with_btime),
            devs(&self.with_btime),
            total(&self.without_btime),
            devs(&self.without_btime)
        )
    }
}

/// A snapshot of the whole container filesystem.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub entries: BTreeMap<PathBuf, Entry>,
    /// Mount points not walked, and why.
    pub skipped: BTreeMap<PathBuf, Skip>,
    pub key_stats: KeyStats,
    pub took: Duration,
}

/// Why a mount is not walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// A pseudo filesystem ([`PSEUDO_FS_TYPES`]).
    Pseudo,
    /// Mounted read-only (mount or superblock option `ro`).
    ReadOnly,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Skip::Pseudo => "pseudo",
            Skip::ReadOnly => "read-only",
        })
    }
}

/// One line of `/proc/self/mountinfo`, reduced to what the snapshot needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub mount_id: u64,
    pub parent_id: u64,
    pub mount_point: PathBuf,
    pub fs_type: String,
    /// `ro` among the per-mount options or the superblock options.
    pub read_only: bool,
}

/// Parses `/proc/self/mountinfo` text (proc(5)). Mount points have spaces,
/// tabs, newlines and backslashes octal-escaped.
pub fn parse_mountinfo(text: &str) -> Result<Vec<MountInfo>, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let sep = fields
                .iter()
                .position(|f| *f == "-")
                .ok_or_else(|| format!("mountinfo line without ' - ' separator: {line}"))?;
            // Fields: id parent major:minor root mount-point mount-options
            // [optional...] - fs-type source super-options.
            if sep < 6 || fields.len() < sep + 4 {
                return Err(format!("malformed mountinfo line: {line}"));
            }
            let id = |field: &str| {
                field
                    .parse()
                    .map_err(|_| format!("bad mount id {field:?} in mountinfo line: {line}"))
            };
            let has_ro = |options: &str| options.split(',').any(|o| o == "ro");
            Ok(MountInfo {
                mount_id: id(fields[0])?,
                parent_id: id(fields[1])?,
                mount_point: PathBuf::from(OsString::from_vec(unescape_octal(fields[4])?)),
                fs_type: fields[sep + 1].to_owned(),
                read_only: has_ro(fields[5]) || has_ro(fields[sep + 3]),
            })
        })
        .collect()
}

/// Decodes the `\NNN` octal escapes the kernel uses in mountinfo fields.
fn unescape_octal(field: &str) -> Result<Vec<u8>, String> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let digits = bytes
                .get(i + 1..i + 4)
                .ok_or_else(|| format!("truncated octal escape in {field:?}"))?;
            let text =
                std::str::from_utf8(digits).map_err(|_| format!("bad escape in {field:?}"))?;
            let value = u8::from_str_radix(text, 8)
                .map_err(|_| format!("bad octal escape in {field:?}"))?;
            out.push(value);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// Mount points of pseudo filesystems, which the snapshot does not walk.
/// Which mounts the walk does not enter, by mount id, and the writable mounts
/// nested below a skipped read-only mount, whose mount points the walk must
/// start from separately (it never reaches them from `/`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkPlan {
    pub skip: BTreeMap<u64, Skip>,
    pub nested_roots: BTreeSet<PathBuf>,
}

/// Plans the walk over `mounts`: pseudo filesystems and read-only mounts are
/// skipped. A mount below a pseudo filesystem is never walked (as before:
/// `/proc/sys` and friends); a writable, non-pseudo mount whose nearest
/// skipped ancestor is read-only is a nested root.
pub fn plan_walk(mounts: &[MountInfo]) -> WalkPlan {
    let by_id: BTreeMap<u64, &MountInfo> = mounts.iter().map(|m| (m.mount_id, m)).collect();
    let skip_of = |m: &MountInfo| {
        if PSEUDO_FS_TYPES.contains(&m.fs_type.as_str()) {
            Some(Skip::Pseudo)
        } else if m.read_only {
            Some(Skip::ReadOnly)
        } else {
            None
        }
    };
    let skip: BTreeMap<u64, Skip> = mounts
        .iter()
        .filter_map(|m| skip_of(m).map(|s| (m.mount_id, s)))
        .collect();
    let nearest_skipped_ancestor = |m: &MountInfo| {
        let mut seen = BTreeSet::new();
        let mut parent = m.parent_id;
        // The root mount's parent lies outside the namespace (not in the
        // table); `seen` guards against a malformed cyclic table.
        while let Some(p) = by_id.get(&parent).filter(|_| seen.insert(parent)) {
            if let Some(s) = skip.get(&p.mount_id) {
                return Some(*s);
            }
            parent = p.parent_id;
        }
        None
    };
    let nested_roots = mounts
        .iter()
        .filter(|m| !skip.contains_key(&m.mount_id))
        .filter(|m| nearest_skipped_ancestor(m) == Some(Skip::ReadOnly))
        .map(|m| m.mount_point.clone())
        .collect();
    WalkPlan { skip, nested_roots }
}

/// Paths at least this long (bytes) are reached in steps ([`reach`]): the
/// kernel refuses a path of `PATH_MAX` (4096) or more (`ENAMETOOLONG`), and
/// the deletion engine's tests build trees deeper than that.
const LONG_PATH: usize = 3072;

/// `path` as a directory fd (`None`: the cwd) and the rest, for the `*at`
/// calls: the whole path when it is short; otherwise an `O_PATH` fd of a
/// prefix opened in steps shorter than [`LONG_PATH`], and the rest (shorter
/// than that). Like a full path, the steps follow symlinks on the way; the
/// walk only gets here through directories it recorded.
fn reach(path: &Path) -> Result<(Option<OwnedFd>, PathBuf), String> {
    if path.as_os_str().len() < LONG_PATH {
        return Ok((None, path.to_path_buf()));
    }
    let mut dir: Option<OwnedFd> = None;
    let mut chunk = PathBuf::new();
    for component in path.components() {
        let name = component.as_os_str();
        if chunk.as_os_str().len() + name.len() + 1 >= LONG_PATH {
            let base = dir.as_ref().map_or(CWD, |fd| fd.as_fd());
            let opened = rustix::fs::openat(
                base,
                &chunk,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| format!("open a prefix of {}: {e}", path.display()))?;
            dir = Some(opened);
            chunk = PathBuf::new();
        }
        chunk.push(name);
    }
    Ok((dir, chunk))
}

/// The names in the directory at `path` (any length), without `.` and `..`.
fn list(path: &Path) -> Result<Vec<OsString>, String> {
    let (dir, rest) = reach(path)?;
    let fd = rustix::fs::openat(
        dir.as_ref().map_or(CWD, |fd| fd.as_fd()),
        &rest,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("list {}: {e}", path.display()))?;
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::new(fd).map_err(|e| format!("list {}: {e}", path.display()))? {
        let entry = entry.map_err(|e| format!("list {}: {e}", path.display()))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
        }
    }
    Ok(names)
}

/// Stats `path` (not following it) into an entry, its mount id and the type
/// of the filesystem it is on.
fn stat_entry(
    path: &Path,
    fs_types: &BTreeMap<u64, String>,
) -> Result<(Entry, u64, String), String> {
    let (dir, rest) = reach(path)?;
    let stx = rustix::fs::statx(
        dir.as_ref().map_or(CWD, |fd| fd.as_fd()),
        &rest,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::TYPE | StatxFlags::INO | StatxFlags::BTIME | StatxFlags::MNT_ID,
    )
    .map_err(|e| format!("statx {}: {e}", path.display()))?;
    let need = (StatxFlags::TYPE | StatxFlags::INO | StatxFlags::MNT_ID).bits();
    if stx.stx_mask & need != need {
        return Err(format!(
            "statx {}: type, inode or mount id not reported (mask {:#x})",
            path.display(),
            stx.stx_mask
        ));
    }
    let fs_type = fs_types
        .get(&stx.stx_mnt_id)
        .ok_or_else(|| {
            format!(
                "statx {}: mount id {} not in mountinfo (mounts changed during the snapshot?)",
                path.display(),
                stx.stx_mnt_id
            )
        })?
        .clone();
    let btime = (stx.stx_mask & StatxFlags::BTIME.bits() != 0
        && !BTIME_UNSTABLE_FS_TYPES.contains(&fs_type.as_str()))
    .then_some((stx.stx_btime.tv_sec, stx.stx_btime.tv_nsec));
    let kind = match FileType::from_raw_mode(u32::from(stx.stx_mode)) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    };
    let entry = Entry {
        kind,
        key: Key {
            dev: (stx.stx_dev_major, stx.stx_dev_minor),
            ino: stx.stx_ino,
            btime,
        },
    };
    Ok((entry, stx.stx_mnt_id, fs_type))
}

/// Snapshots the whole filesystem visible to this process. Fails on any
/// object it cannot stat or directory it cannot list: a blind spot would let
/// a deletion go unnoticed, so the oracle fails closed.
pub fn take() -> Result<Snapshot, String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("read /proc/self/mountinfo: {e}"))?;
    take_from(Path::new("/"), &parse_mountinfo(&mountinfo)?)
}

/// Snapshots the tree at `root` with the mount table `mounts`, not
/// descending into pseudo filesystems or read-only mounts ([`plan_walk`]).
pub fn take_from(root: &Path, mounts: &[MountInfo]) -> Result<Snapshot, String> {
    let plan = plan_walk(mounts);
    let fs_types: BTreeMap<u64, String> = mounts
        .iter()
        .map(|m| (m.mount_id, m.fs_type.clone()))
        .collect();
    let started = Instant::now();
    let mut entries = BTreeMap::new();
    let mut skipped = BTreeMap::new();
    let mut key_stats = KeyStats::default();
    let mut errors = Vec::new();
    let mut pending: Vec<PathBuf> = std::iter::once(root.to_path_buf())
        .chain(
            plan.nested_roots
                .iter()
                .filter(|p| p.starts_with(root))
                .cloned(),
        )
        .collect();

    while let Some(path) = pending.pop() {
        if entries.contains_key(&path) || skipped.contains_key(&path) {
            // A nested root that is also reachable through the walk.
            continue;
        }
        let (entry, mnt_id, fs_type) = match stat_entry(&path, &fs_types) {
            Ok(found) => found,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        if let Some(why) = plan.skip.get(&mnt_id) {
            // Only reached at a mount's root: the walk never descends into a
            // skipped mount.
            skipped.insert(path, *why);
            continue;
        }
        let counter = if entry.key.btime.is_some() {
            &mut key_stats.with_btime
        } else {
            &mut key_stats.without_btime
        };
        let (major, minor) = entry.key.dev;
        *counter
            .entry(format!("{fs_type} {major}:{minor}"))
            .or_default() += 1;
        if entry.kind == Kind::Dir {
            match list(&path) {
                Ok(names) => pending.extend(names.into_iter().map(|name| path.join(name))),
                Err(e) => errors.push(e),
            }
        }
        entries.insert(path, entry);
    }

    if !errors.is_empty() {
        let shown = errors
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  ");
        return Err(format!(
            "snapshot of {} incomplete ({} errors), the oracle would be blind there:\n  {shown}",
            root.display(),
            errors.len()
        ));
    }
    Ok(Snapshot {
        entries,
        skipped,
        key_stats,
        took: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
1362 1156 0:90 / / rw,relatime - overlay overlay rw,lowerdir=/x
1363 1362 0:103 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw
1364 1362 0:104 / /dev rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755
1366 1364 0:106 / /dev/pts rw,nosuid,noexec,relatime - devpts devpts rw,gid=100004
1368 1365 0:23 / /sys/fs/cgroup ro,nosuid shared:9 master:1 - cgroup2 cgroup2 rw
1370 1362 0:111 / /tmp/with\\040space rw - tmpfs diskgc-e2e-scratch rw
";

    #[test]
    fn mountinfo_fields_including_optional_ones_and_escapes() {
        let mounts = parse_mountinfo(SAMPLE).unwrap();
        assert_eq!(mounts.len(), 6);
        assert_eq!(mounts[0].mount_point, PathBuf::from("/"));
        assert_eq!(mounts[0].fs_type, "overlay");
        assert_eq!(mounts[0].mount_id, 1362);
        assert_eq!(mounts[0].parent_id, 1156);
        assert!(!mounts[0].read_only);
        // Optional fields ("shared:9 master:1") before the separator.
        assert_eq!(mounts[4].fs_type, "cgroup2");
        assert!(mounts[4].read_only);
        assert_eq!(mounts[5].mount_point, PathBuf::from("/tmp/with space"));
    }

    fn ids(plan: &WalkPlan, why: Skip) -> Vec<u64> {
        plan.skip
            .iter()
            .filter(|(_, s)| **s == why)
            .map(|(id, _)| *id)
            .collect()
    }

    #[test]
    fn pseudo_filesystems_are_skipped_real_ones_are_not() {
        let plan = plan_walk(&parse_mountinfo(SAMPLE).unwrap());
        // /proc, /dev/pts, /sys/fs/cgroup (pseudo wins over its ro option).
        assert_eq!(ids(&plan, Skip::Pseudo), vec![1363, 1366, 1368]);
        assert!(ids(&plan, Skip::ReadOnly).is_empty());
        assert!(plan.nested_roots.is_empty());
    }

    /// Read-only per mount or per superblock is skipped; a writable mount
    /// below a read-only one is walked from its own mount point; nothing
    /// below a pseudo filesystem is (podman's masks of /proc paths).
    const WITH_READ_ONLY: &str = "\
1 0 0:90 / / rw,relatime - overlay overlay rw
2 1 0:98 / /proc rw,nosuid - proc proc rw
3 2 0:98 /sys /proc/sys ro,nosuid - proc proc rw
4 2 0:70 /crun/.empty-directory /proc/acpi ro,nosuid - tmpfs tmpfs rw
5 2 0:71 / /proc/rw-tmpfs rw - tmpfs tmpfs rw
10 1 254:1 /nix/store/x-under-test/bin /opt/diskgc-e2e/bin ro,nosuid,nodev - ext4 /dev/dm-1 rw
11 1 254:2 / /opt/toolchains rw,relatime - ext4 /dev/dm-2 ro,errors=remount-ro
12 11 0:120 / /opt/toolchains/scratch rw - tmpfs tmpfs rw
13 12 0:121 / /opt/toolchains/scratch/ro-again ro - tmpfs tmpfs rw
14 13 0:122 / /opt/toolchains/scratch/ro-again/rw-deep rw - tmpfs tmpfs rw
";

    #[test]
    fn read_only_mounts_are_skipped_and_writable_mounts_below_them_walked() {
        let mounts = parse_mountinfo(WITH_READ_ONLY).unwrap();
        assert!(mounts[6].read_only, "superblock ro counts");
        let plan = plan_walk(&mounts);
        assert_eq!(ids(&plan, Skip::Pseudo), vec![2, 3]);
        assert_eq!(ids(&plan, Skip::ReadOnly), vec![4, 10, 11, 13]);
        let expected: BTreeSet<PathBuf> = [
            "/opt/toolchains/scratch",
            "/opt/toolchains/scratch/ro-again/rw-deep",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(plan.nested_roots, expected);
    }

    #[test]
    fn a_cyclic_mount_table_does_not_hang_the_plan() {
        let mounts =
            parse_mountinfo("1 2 0:1 / /a rw - tmpfs t rw\n2 1 0:2 / /b rw - tmpfs t rw\n")
                .unwrap();
        let plan = plan_walk(&mounts);
        assert!(plan.skip.is_empty() && plan.nested_roots.is_empty());
    }

    #[test]
    fn malformed_mountinfo_is_an_error() {
        assert!(parse_mountinfo("1 2 0:1 / /x rw overlay\n").is_err());
        assert!(parse_mountinfo("1 2 0:1 / /x\\04 rw - tmpfs t rw\n").is_err());
        // No superblock options.
        assert!(parse_mountinfo("1 2 0:1 / /x rw - tmpfs t\n").is_err());
        assert!(parse_mountinfo("1 x 0:1 / /x rw - tmpfs t rw\n").is_err());
    }

    #[test]
    fn same_object_uses_btime_only_when_both_sides_have_it() {
        let a = Key {
            dev: (0, 1),
            ino: 7,
            btime: Some((10, 0)),
        };
        assert!(a.same_object(&a));
        assert!(!a.same_object(&Key {
            btime: Some((11, 0)),
            ..a
        }));
        assert!(a.same_object(&Key { btime: None, ..a }));
        assert!(!a.same_object(&Key { ino: 8, ..a }));
        assert!(!a.same_object(&Key { dev: (0, 2), ..a }));
    }
}
