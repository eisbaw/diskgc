//! `statx` results as the port's [`Stat`]: a field is `Some` only when the
//! kernel answered it (docs/safety.md, Path traversal: "statx results are
//! trusted only when the attribute's bit is set").

use diskgc_core::host::{Dev, FileType, Stat, Timestamp};
use rustix::fs::{StatxAttributes, StatxFlags, StatxTimestamp};

/// `STATX_SUBVOL` (Linux 6.10, `include/uapi/linux/stat.h`), which rustix
/// does not name. A kernel before 6.10 ignores the request bit and leaves
/// it out of `stx_mask`; so does a filesystem without subvolumes.
const STATX_SUBVOL: StatxFlags = StatxFlags::from_bits_retain(0x8000);

/// What every `statx` of the port asks for.
pub(crate) const STATX_MASK: StatxFlags = StatxFlags::BASIC_STATS
    .union(StatxFlags::BTIME)
    .union(StatxFlags::MNT_ID)
    .union(STATX_SUBVOL);

fn timestamp(t: StatxTimestamp) -> Timestamp {
    Timestamp {
        secs: t.tv_sec,
        nanos: t.tv_nsec,
    }
}

/// Converts a raw `struct statx`, keeping only answered fields: each by its
/// bit in `stx_mask`, `mount_root` by its bit in `stx_attributes_mask`.
pub(crate) fn stat_from_statx(x: &rustix::fs::Statx) -> Stat {
    let answered = |bit: StatxFlags| x.stx_mask & bit.bits() == bit.bits();
    let attribute = |attr: StatxAttributes| {
        x.stx_attributes_mask
            .contains(attr)
            .then(|| x.stx_attributes.contains(attr))
    };
    let mode = u32::from(x.stx_mode);
    Stat {
        dev: Dev {
            major: x.stx_dev_major,
            minor: x.stx_dev_minor,
        },
        file_type: answered(StatxFlags::TYPE).then_some(FileType::from_mode(mode)),
        perm: answered(StatxFlags::MODE).then_some(mode & 0o7777),
        nlink: answered(StatxFlags::NLINK).then_some(x.stx_nlink),
        uid: answered(StatxFlags::UID).then_some(x.stx_uid),
        gid: answered(StatxFlags::GID).then_some(x.stx_gid),
        ino: answered(StatxFlags::INO).then_some(x.stx_ino),
        size: answered(StatxFlags::SIZE).then_some(x.stx_size),
        blocks: answered(StatxFlags::BLOCKS).then_some(x.stx_blocks),
        mtime: answered(StatxFlags::MTIME).then(|| timestamp(x.stx_mtime)),
        ctime: answered(StatxFlags::CTIME).then(|| timestamp(x.stx_ctime)),
        btime: answered(StatxFlags::BTIME).then(|| timestamp(x.stx_btime)),
        atime: answered(StatxFlags::ATIME).then(|| timestamp(x.stx_atime)),
        mnt_id: answered(StatxFlags::MNT_ID).then_some(x.stx_mnt_id),
        mount_root: attribute(StatxAttributes::MOUNT_ROOT),
        subvol: answered(STATX_SUBVOL).then_some(x.stx_subvol),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::{AtFlags, CWD};

    /// A real `struct statx` of `/` (the masks are then edited to simulate
    /// a kernel or filesystem that does not answer).
    fn real_statx() -> rustix::fs::Statx {
        rustix::fs::statx(CWD, "/", AtFlags::SYMLINK_NOFOLLOW, STATX_MASK).expect("statx /")
    }

    #[test]
    fn answered_fields_are_kept() {
        let x = real_statx();
        let stat = stat_from_statx(&x);
        assert_eq!(stat.file_type, Some(FileType::Directory));
        assert_eq!(stat.ino, Some(x.stx_ino));
        // Linux 5.8+ answers both on every filesystem this test runs on.
        assert_eq!(stat.mnt_id, Some(x.stx_mnt_id));
        assert_eq!(stat.mount_root, Some(true), "/ is a mount root");
    }

    #[test]
    fn an_attribute_is_ignored_unless_its_mask_bit_is_set() {
        let mut x = real_statx();
        // The attribute bit says "mount root", but the kernel did not claim
        // to know (attributes_mask without MOUNT_ROOT): not trusted.
        x.stx_attributes |= StatxAttributes::MOUNT_ROOT;
        x.stx_attributes_mask -= StatxAttributes::MOUNT_ROOT;
        assert_eq!(stat_from_statx(&x).mount_root, None);
        // And "not a mount root" is not trusted either.
        x.stx_attributes -= StatxAttributes::MOUNT_ROOT;
        assert_eq!(stat_from_statx(&x).mount_root, None);
    }

    #[test]
    fn a_field_is_ignored_unless_its_mask_bit_is_set() {
        let mut x = real_statx();
        x.stx_mask &=
            !(StatxFlags::MNT_ID.bits() | StatxFlags::BTIME.bits() | StatxFlags::INO.bits());
        x.stx_mask &= !STATX_SUBVOL.bits();
        x.stx_subvol = 256;
        let stat = stat_from_statx(&x);
        assert_eq!(stat.subvol, None, "an unanswered stx_subvol is not trusted");
        assert_eq!(stat.mnt_id, None);
        assert_eq!(stat.btime, None);
        assert_eq!(stat.ino, None);
        assert_eq!(stat.dev_ino(), None);
        assert!(stat.uid.is_some(), "other fields stay");
    }
}
