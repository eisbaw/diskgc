//! Which filesystem a path's bytes are taken from and freed on: the one
//! rule of "the same filesystem" (TASK-286; docs/architecture.md,
//! Filesystem matching), and the filesystem types it rests on.
//!
//! The same filesystem is the device mountinfo names for a mount, its
//! superblock's ([`space_of`]): every mount of one filesystem shows it, a
//! bind mount and every btrfs subvolume of one btrfs alike. A `statx`
//! `st_dev` identifies an object (with its inode) and is never compared
//! with it: on btrfs it is each subvolume's own anonymous device. Where a
//! superblock's device does not identify one filesystem alone (bcachefs,
//! which takes it from one member device and keeps it when that member is
//! removed and reused), the filesystem's id read from a held descriptor
//! (`statfs` `f_fsid`, [`space_at`]) must agree too; unread, it never does.
//!
//! Deleting needs the same space proved ([`same_space`], [`Space::same_as`]).
//! Writing diskgc's own state needs the opposite proved: [`write_guard`].

use std::fmt;

use crate::host::{Dev, Host, MountEntry};

/// Filesystem types diskgc never takes candidates from: network and
/// cluster filesystems, and filesystems shared with another machine (a VM
/// host's), whose files another host may be using while no process of this
/// one is (docs/safety.md, Before the walk: `unsupported_fs`). Their locks
/// are not the local kernel's alone (Cargo does not lock there), and a dead
/// server can hang any operation on them. Compared with mountinfo's type in
/// full, and a FUSE type (`fuse.<name>`) by its `<name>` too. Every FUSE
/// type is refused anyway ([`may_be_networked`]); the FUSE names here only
/// make the reason clear.
pub const NETWORK_FS_TYPES: &[&str] = &[
    // Network filesystems in the kernel.
    "9p",
    "afs",
    "ceph",
    "cifs",
    "coda",
    "ncpfs",
    "nfs",
    "nfs4",
    "smb3",
    "smbfs",
    // Cluster and parallel filesystems (shared storage of several hosts).
    "beegfs",
    "gfs2",
    "gpfs",
    "lustre",
    "ocfs2",
    "orangefs",
    "pvfs2",
    // Shared with a VM's host.
    "vboxsf",
    "virtiofs",
    "vmhgfs",
    // FUSE clients of network storage (`fuse.<name>`).
    "ceph-fuse",
    "curlftpfs",
    "davfs",
    "gcsfuse",
    "glusterfs",
    "goofys",
    "gvfsd-fuse",
    "juicefs",
    "lizardfs",
    "mfs",
    "moosefs",
    "rclone",
    "s3fs",
    "sshfs",
    "vmhgfs-fuse",
];

/// mountinfo's type of an automount trigger that has not been triggered
/// (systemd `x-systemd.automount`, the automounter daemon). Once mounted,
/// the real filesystem is stacked on it and is the one screened.
pub const AUTOFS: &str = "autofs";

/// Whether mountinfo's filesystem type `fs_type` may hold files another
/// host uses: a type in [`NETWORK_FS_TYPES`] (directly, or as
/// `fuse.<type>`), or any FUSE type (`fuse`, `fuseblk`, and their subtypes).
/// A FUSE type cannot establish where its backing data is stored.
///
/// An untriggered automount point ([`AUTOFS`]) too (TASK-180, fail
/// closed): a path lookup through it mounts whatever it stands for, most
/// often NFS, and with that server down the lookup hangs in the kernel
/// before any deadline can act. mountinfo does not say what it would
/// mount, so a root at or below one is refused without being looked up.
pub fn may_be_networked(fs_type: &str) -> bool {
    NETWORK_FS_TYPES.contains(&fs_type)
        || fs_type == AUTOFS
        || matches!(fs_type, "fuse" | "fuseblk")
        || fs_type.starts_with("fuse.")
        || fs_type.starts_with("fuseblk.")
}

/// Why [`may_be_networked`] refuses `fs_type`, for messages.
pub fn networked_why(fs_type: &str) -> &'static str {
    let named = |name: &str| NETWORK_FS_TYPES.contains(&name);
    if named(fs_type) || fs_type.strip_prefix("fuse.").is_some_and(named) {
        "a network, cluster or shared filesystem"
    } else if fs_type == AUTOFS {
        "an untriggered automount point (fail closed: a lookup would mount it, often over \
         the network)"
    } else {
        "a FUSE filesystem (fail closed: its data may be on another host)"
    }
}

/// The block major of loop devices (`Documentation/admin-guide/devices.txt`).
pub const LOOP_MAJOR: u32 = 7;

/// The type name overlayfs has in mountinfo.
pub const OVERLAY: &str = "overlay";

/// Kernel filesystems that stack on directories of other filesystems and
/// write through to them: eCryptfs (its ciphertext directory), aufs (its
/// branches), shiftfs (the directory it shifts). Refused only when mounted
/// at or below a match; on the candidate's or match's own mount they are
/// taken as local (docs/safety.md, limits), or an eCryptfs-encrypted home
/// would stop every run.
pub const STACKED_ON_DIRECTORIES: &[&str] = &["ecryptfs", "aufs", "shiftfs"];

/// Filesystem types whose superblock device does not identify one live
/// filesystem alone: bcachefs takes `s_dev` from one member device and
/// keeps it after that member is removed, so a filesystem made on the
/// freed device can show the same number (Codex review of TASK-286).
pub const DEVICE_NOT_UNIQUE: &[&str] = &["bcachefs"];

/// Whether a filesystem of `fs_type` may store its data in directories of
/// another filesystem, which mountinfo does not name: overlayfs, the
/// [`STACKED_ON_DIRECTORIES`], and every type the mount screen refuses
/// (FUSE such as `bindfs` or `fuse-overlayfs`, an NFS export of this host,
/// an automount point).
pub fn stored_elsewhere(fs_type: &str) -> bool {
    fs_type == OVERLAY || STACKED_ON_DIRECTORIES.contains(&fs_type) || may_be_networked(fs_type)
}

/// The pool of space a mount's files take and free ([`space_of`],
/// [`space_at`]). A type of its own, so it is never compared with a `statx`
/// device. Its `==` is the value's (for records and tests); whether two are
/// one filesystem is [`Self::same_as`], which an unconfirmed identity never
/// satisfies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    dev: Dev,
    confirm: Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirm {
    /// The superblock's device identifies the filesystem alone.
    NotNeeded,
    /// [`DEVICE_NOT_UNIQUE`]: the filesystem's id (`statfs` `f_fsid`) read
    /// from a held descriptor.
    FsId(u64),
    /// [`DEVICE_NOT_UNIQUE`] without an id read: the same as nothing.
    Unknown,
}

impl Space {
    /// The space of a filesystem on `dev` whose superblock device
    /// identifies it alone (any type but [`DEVICE_NOT_UNIQUE`]), for a
    /// caller that holds no mount entry: tests. The core takes every space
    /// from a mount ([`space_of`], [`space_at`]).
    pub fn unique_device(dev: Dev) -> Self {
        Self {
            dev,
            confirm: Confirm::NotNeeded,
        }
    }

    /// The superblock's device as mountinfo names it: for messages, for
    /// keys and for what is placed by the mount table alone (the index's
    /// tries, `held_by_deleted_files`).
    pub fn dev(&self) -> Dev {
        self.dev
    }

    /// Whether deleting on `self` frees space on `other`, proved: one
    /// superblock, and on a type whose device is not unique
    /// ([`DEVICE_NOT_UNIQUE`]) one filesystem id read on both sides.
    pub fn same_as(&self, other: &Space) -> bool {
        self.dev == other.dev
            && match (self.confirm, other.confirm) {
                (Confirm::NotNeeded, Confirm::NotNeeded) => true,
                (Confirm::FsId(a), Confirm::FsId(b)) => a == b,
                _ => false,
            }
    }
}

impl fmt::Display for Space {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.confirm {
            Confirm::NotNeeded => write!(f, "{}", self.dev),
            Confirm::FsId(id) => write!(f, "{} (filesystem id {id:#x})", self.dev),
            Confirm::Unknown => write!(f, "{} (filesystem id not read)", self.dev),
        }
    }
}

/// The space of `mount` from the mount table alone: the device mountinfo
/// names for it (the superblock's). On a type whose device is not unique
/// ([`DEVICE_NOT_UNIQUE`]) it is the same as nothing: [`space_at`] reads
/// its identity.
pub fn space_of(mount: &MountEntry) -> Space {
    let confirm = if DEVICE_NOT_UNIQUE.contains(&mount.fs_type.as_str()) {
        Confirm::Unknown
    } else {
        Confirm::NotNeeded
    };
    Space {
        dev: mount.dev,
        confirm,
    }
}

/// The space of `mount`, reached as `fd` (a directory on it): [`space_of`],
/// and on a type whose device is not unique the filesystem's id read from
/// `fd` (`statfs` `f_fsid`; bcachefs makes it from its UUID). An id that
/// cannot be read leaves it the same as nothing (fail closed for
/// deleting).
pub fn space_at<H: Host>(host: &H, mount: &MountEntry, fd: &H::Fd) -> Space {
    let space = space_of(mount);
    if space.confirm == Confirm::NotNeeded {
        return space;
    }
    match host.fs_id(fd) {
        Ok(id) => Space {
            confirm: Confirm::FsId(id),
            ..space
        },
        Err(_) => space,
    }
}

/// Whether deleting on mount `a` frees space on mount `b`, from the table
/// alone ([`space_of`]): one superblock, on a type whose device is unique.
pub fn same_space(a: &MountEntry, b: &MountEntry) -> bool {
    space_of(a).same_as(&space_of(b))
}

/// Why writing on mount `other` may take space from mount `watched`'s
/// filesystem, from the table alone; `None` when the table shows them
/// independent: different superblocks, neither one a filesystem that may
/// store its data on another ([`stored_elsewhere`]) or on a loop device by
/// its major, and not two datasets of one ZFS pool. One superblock counts
/// whatever its type ([`DEVICE_NOT_UNIQUE`] too: sharing is the cautious
/// side). Loop devices that only the system can name (a partition of one,
/// btrfs on one) are [`write_guard`]'s.
pub fn shared_space(other: &MountEntry, watched: &MountEntry) -> Option<String> {
    if other.dev == watched.dev {
        return Some(format!("the same filesystem (device {})", watched.dev));
    }
    for mount in [other, watched] {
        let at = mount.mount_point.display();
        if stored_elsewhere(&mount.fs_type) {
            return Some(format!(
                "{} at {at} may store its data on another filesystem",
                mount.fs_type
            ));
        }
        if mount.dev.major == LOOP_MAJOR {
            return Some(format!(
                "{at} is on the loop device {}, whose image is a file on another filesystem",
                mount.dev
            ));
        }
    }
    let pool = |mount: &MountEntry| {
        (mount.fs_type == "zfs").then(|| {
            mount
                .source
                .split('/')
                .next()
                .unwrap_or_default()
                .to_owned()
        })
    };
    match (pool(other), pool(watched)) {
        (Some(a), Some(b)) if a == b => Some(format!("two datasets of the ZFS pool {a}")),
        _ => None,
    }
}

/// Whether the filesystem of `mount` can be stored on a loop device, from
/// the table alone: a loop device's own major, the extended major
/// partitions take ([`BLOCK_EXT_MAJOR`]), or btrfs on an anonymous device
/// (its members are read through a descriptor, [`on_loop`]). Any other is
/// on none (device-mapper or md over a loop device is not followed: a
/// stated limit).
pub fn may_be_on_loop(mount: &MountEntry) -> bool {
    match mount.dev.major {
        LOOP_MAJOR | BLOCK_EXT_MAJOR => true,
        0 => mount.fs_type == "btrfs",
        _ => false,
    }
}

/// The loop devices the filesystem of `mount` is stored on
/// ([`Host::loop_devices`]): a loop device itself, a partition of one, or
/// for btrfs each member that is one, read through `fd`, a directory on the
/// mount (its FSID, never mountinfo's source, which names one member:
/// TASK-299). A btrfs without `fd`, or whose membership cannot be read
/// completely, is an error. Only a mount that [`may_be_on_loop`] is asked.
pub fn on_loop<H: Host>(
    host: &H,
    mount: &MountEntry,
    fd: Option<&H::Fd>,
) -> Result<Vec<Dev>, crate::host::HostError> {
    if may_be_on_loop(mount) {
        host.loop_devices(mount, fd)
    } else {
        Ok(Vec::new())
    }
}

/// Filesystem types that stack on another filesystem's directories yet
/// show their lower files' owners as the mount table can tell them:
/// overlayfs copies each owner from its real inode through the layer's
/// id mapping (`ovl_copyattr`), and a layer that is not idmapped shows it
/// unmapped (its idmapped layers are private clones the table does not
/// list: a documented limit, docs/safety.md, Loop devices); eCryptfs
/// copies its lower inode's owner unmapped (`fsstack_copy_attr_all`,
/// `ecryptfs_getattr`), its own mounts cannot be idmapped (no
/// `FS_ALLOW_IDMAP`), and `ecryptfs_mount` refuses an idmapped lower path
/// (`-EINVAL`, since 5.12). Without them a live overlay root or an
/// eCryptfs home with root's snap images would clear nothing (TASK-299
/// review, mped B1).
const SHOWS_STORED_OWNERS: &[&str] = &[OVERLAY, "ecryptfs"];

/// Whether `statx` through `mount` may report an owner other than the one
/// stored, as the mount table shows it (TASK-299): an idmapped mount, and
/// every type that may store its data elsewhere ([`stored_elsewhere`]:
/// shiftfs exists to shift owners, aufs and the FUSE and network types say
/// nothing of theirs) but those of [`SHOWS_STORED_OWNERS`].
pub fn may_show_other_owners(mount: &MountEntry) -> bool {
    mount.idmapped
        || (stored_elsewhere(&mount.fs_type)
            && !SHOWS_STORED_OWNERS.contains(&mount.fs_type.as_str()))
}

/// The warning for a `PATH` on a copy-on-write filesystem with snapshots
/// (btrfs, ZFS; the PRD's non-goal "warned about"): deleting may free
/// nothing while a snapshot shares the extents. `None` on any other type.
pub fn cow_note(mount: &MountEntry) -> Option<String> {
    matches!(mount.fs_type.as_str(), "btrfs" | "zfs").then(|| {
        format!(
            "warning: {} is {}: a deletion frees nothing while a snapshot shares its \
             extents; tallies are estimates, and only the final sample of available space \
             decides",
            mount.mount_point.display(),
            mount.fs_type
        )
    })
}

/// The block major of extended partitions (`blkext`), which a loop
/// device's partitions take (`losetup -P`), as NVMe partitions do.
pub const BLOCK_EXT_MAJOR: u32 = 259;

/// How a write of diskgc's own state on the state directory's mount
/// relates to the watched filesystem's space ([`write_guard`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteGuard {
    /// Proved independent: no guard.
    Independent,
    /// It may take the watched filesystem's space, at most what it writes
    /// (the reason): guarded against the lower of the two samples.
    Sample(String),
    /// Its effect on the watched filesystem's space cannot be bounded (the
    /// reason): a state directory on a loop device's filesystem, whose
    /// image may be a sparse file there filling by more than the write, or
    /// one whose loop devices cannot be told. Not written.
    Unbounded(String),
}

/// How a write in the state directory (`dir`, on `dir_mount`) relates to
/// the space of `watched` (reached as `path`): [`shared_space`] from the
/// table, then the loop devices under each ([`on_loop`]). A state
/// directory on the watched filesystem itself is [`WriteGuard::Sample`];
/// one stored on a loop device, or whose loop devices cannot be read, is
/// [`WriteGuard::Unbounded`]; a watched filesystem on a loop device is
/// [`WriteGuard::Sample`] (the write takes the state directory's space,
/// which may hold the image).
pub fn write_guard<H: Host>(
    host: &H,
    dir: &H::Fd,
    dir_mount: &MountEntry,
    path: &H::Fd,
    watched: &MountEntry,
) -> WriteGuard {
    if dir_mount.dev == watched.dev {
        return WriteGuard::Sample(format!("the same filesystem (device {})", watched.dev));
    }
    match on_loop(host, dir_mount, Some(dir)) {
        Ok(loops) if loops.is_empty() => {}
        Ok(loops) => {
            let shown: Vec<String> = loops.iter().map(ToString::to_string).collect();
            return WriteGuard::Unbounded(format!(
                "the state directory's filesystem at {} is stored on loop device {}, whose \
                 image may be a sparse file on the reclaimed filesystem",
                dir_mount.mount_point.display(),
                shown.join(", ")
            ));
        }
        Err(e) => {
            return WriteGuard::Unbounded(format!(
                "cannot tell whether the state directory's filesystem at {} is on a loop \
                 device: {e}",
                dir_mount.mount_point.display()
            ));
        }
    }
    if let Some(why) = shared_space(dir_mount, watched) {
        return WriteGuard::Sample(why);
    }
    match on_loop(host, watched, Some(path)) {
        Ok(loops) if loops.is_empty() => WriteGuard::Independent,
        Ok(loops) => WriteGuard::Sample(format!(
            "the reclaimed filesystem is stored on loop device {}, whose image may be on the \
             state directory's filesystem",
            loops[0]
        )),
        Err(e) => WriteGuard::Sample(format!(
            "cannot tell whether the reclaimed filesystem is on a loop device: {e}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(minor: u32) -> Dev {
        Dev { major: 254, minor }
    }

    /// Codex review of TASK-286 (bcachefs): one superblock device is the
    /// same space only with one filesystem id read on both sides; two live
    /// filesystems sharing a device number (a member removed and its device
    /// reused) differ by id, and an id not read is the same as nothing.
    #[test]
    fn a_device_shared_by_two_filesystems_is_one_space_only_by_their_id() {
        let ext4 = Space::unique_device(dev(1));
        assert!(ext4.same_as(&Space::unique_device(dev(1))));
        assert!(!ext4.same_as(&Space::unique_device(dev(2))));
        let read = |id| Space {
            dev: dev(3),
            confirm: Confirm::FsId(id),
        };
        assert!(read(7).same_as(&read(7)));
        assert!(!read(7).same_as(&read(8)), "two filesystems on one device");
        let unread = Space {
            dev: dev(3),
            confirm: Confirm::Unknown,
        };
        assert!(
            !unread.same_as(&unread),
            "an identity not read proves nothing"
        );
        assert!(!unread.same_as(&read(7)));
        assert!(!read(7).same_as(&Space::unique_device(dev(3))));
    }
}
