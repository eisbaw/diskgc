//! Whole-container filesystem snapshots for the deletion oracle
//! (docs/testing.md, Grounding, acceptance signal 1).
//!
//! A snapshot maps every path reachable from `/` (crossing into every real
//! mount: the image's overlay root, the scratch tmpfs, `/dev`, bind mounts)
//! to the identity of the object there: (device, inode, birth time). Pseudo
//! filesystems (`/proc`, `/sys`, cgroups, `devpts`, ...) are skipped: they
//! hold no files diskgc could delete and their contents change on their own.
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
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, CWD, FileType, StatxFlags};

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
    /// Mount points not walked (pseudo filesystems).
    pub skipped: BTreeSet<PathBuf>,
    pub key_stats: KeyStats,
    pub took: Duration,
}

/// One line of `/proc/self/mountinfo`, reduced to what the snapshot needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub mount_id: u64,
    pub mount_point: PathBuf,
    pub fs_type: String,
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
            if sep < 6 || fields.len() < sep + 2 {
                return Err(format!("malformed mountinfo line: {line}"));
            }
            Ok(MountInfo {
                mount_id: fields[0]
                    .parse()
                    .map_err(|_| format!("bad mount id in mountinfo line: {line}"))?,
                mount_point: PathBuf::from(OsString::from_vec(unescape_octal(fields[4])?)),
                fs_type: fields[sep + 1].to_owned(),
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
pub fn pseudo_mount_points(mounts: &[MountInfo]) -> BTreeSet<PathBuf> {
    mounts
        .iter()
        .filter(|m| PSEUDO_FS_TYPES.contains(&m.fs_type.as_str()))
        .map(|m| m.mount_point.clone())
        .collect()
}

/// Stats `path` (not following it) into an entry and the type of the
/// filesystem (by mount id) it is on.
fn stat_entry(path: &Path, fs_types: &BTreeMap<u64, String>) -> Result<(Entry, String), String> {
    let stx = rustix::fs::statx(
        CWD,
        path,
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
    Ok((entry, fs_type))
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
/// descending into pseudo filesystems.
pub fn take_from(root: &Path, mounts: &[MountInfo]) -> Result<Snapshot, String> {
    let skip = pseudo_mount_points(mounts);
    let fs_types: BTreeMap<u64, String> = mounts
        .iter()
        .map(|m| (m.mount_id, m.fs_type.clone()))
        .collect();
    let started = Instant::now();
    let mut entries = BTreeMap::new();
    let mut skipped = BTreeSet::new();
    let mut key_stats = KeyStats::default();
    let mut errors = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(path) = pending.pop() {
        if skip.contains(&path) {
            skipped.insert(path);
            continue;
        }
        let (entry, fs_type) = match stat_entry(&path, &fs_types) {
            Ok(found) => found,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
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
            match std::fs::read_dir(&path) {
                Ok(dir) => {
                    for child in dir {
                        match child {
                            Ok(child) => pending.push(child.path()),
                            Err(e) => errors.push(format!("list {}: {e}", path.display())),
                        }
                    }
                }
                Err(e) => errors.push(format!("list {}: {e}", path.display())),
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
        // Optional fields ("shared:9 master:1") before the separator.
        assert_eq!(mounts[4].fs_type, "cgroup2");
        assert_eq!(mounts[5].mount_point, PathBuf::from("/tmp/with space"));
    }

    #[test]
    fn pseudo_filesystems_are_skipped_real_ones_are_not() {
        let skip = pseudo_mount_points(&parse_mountinfo(SAMPLE).unwrap());
        let expected: BTreeSet<PathBuf> = ["/proc", "/dev/pts", "/sys/fs/cgroup"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(skip, expected);
    }

    #[test]
    fn malformed_mountinfo_is_an_error() {
        assert!(parse_mountinfo("1 2 0:1 / /x rw overlay\n").is_err());
        assert!(parse_mountinfo("1 2 0:1 / /x\\04 rw - tmpfs t rw\n").is_err());
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
