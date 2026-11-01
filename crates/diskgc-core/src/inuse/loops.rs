//! Loop devices in the in-use scan (docs/safety.md, In use: Loop devices).
//!
//! A configured loop device holds its backing file open in the kernel, where
//! no process's fds, maps or directories show it: an image set up with
//! `udisksctl loop-setup` or `losetup` and the file deleted under it loses
//! its data the moment the device detaches (TASK-169). So every scan lists
//! the configured loop devices ([`Host::loop_backing_files`]) and turns
//! each into what candidates are matched by, the backing file's identity
//! (device, inode), or else a refusal of the candidates it could be in.
//!
//! In order, for each device (TASK-263 review, Codex 2 and 3):
//!
//! 1. **The device's own word.** `LOOP_GET_STATUS64` on the opened device
//!    gives the held file's (device, inode), deleted or not, whatever its
//!    name ([`Host::loop_backing_identity`]): a use, matched like an open
//!    fd. Only a reader that can open the block device gets it (root, the
//!    `disk` group). The kernel takes it from the path the file was opened
//!    by (`vfs_getattr`), so a file opened through a filesystem stored in
//!    another's directories (overlayfs: its own or a layer's anonymous
//!    device, perhaps the copy-up origin's inode) shows an identity no
//!    other name of it has: every candidate is `in_use_unknown` while such
//!    a device is configured (TASK-299, [`stacked_alias`]).
//! 2. Otherwise only the name the kernel prints (`d_path`) is left, which
//!    proves nothing about identity: a mount made over the file's directory
//!    after the device opened it makes a fresh lookup reach another file (a
//!    decoy), and a name diskgc cannot look up may still have another link
//!    inside a candidate.
//!    - The name resolves (by the safe lookup every root takes, no symlink
//!      followed, network and FUSE mounts never touched) to a file none of
//!      the run's owners owns: a candidate holds only its owners' entries
//!      (the eligibility walk refuses any other), so it cannot be in one
//!      on a mount that shows owners as stored. Noted; candidates on a
//!      mount that may show other owners (idmapped, an overlay:
//!      [`crate::places::may_show_other_owners`]) are `in_use_unknown` (TASK-299:
//!      an idmapped bind shows root's image as the user's). An owner read
//!      through such a mount proves nothing: as below, every candidate.
//!      Limit: a decoy owned by another user over a real file of an
//!      owner's (a mount in diskgc's own mount namespace, which only root
//!      makes) hides it.
//!    - The text is not an absolute path: an anonymous object (`d_dname`),
//!      which no directory can name. Noted, not blocking.
//!    - The text ends in ` (deleted)`: the name the device opened is gone;
//!      another link of the file can only be on the same filesystem, so
//!      the candidates on the filesystem of the name's surviving parent
//!      directory are `in_use_unknown`; with no such parent, or a parent on
//!      a filesystem stored in another's directories (an overlay, whose
//!      other links lie on its layers' filesystems under their devices:
//!      TASK-299), every candidate. Limit: a mount made over that parent
//!      since.
//!    - Anything else: the name resolves to an owner's file (perhaps a
//!      decoy), or does not resolve: every candidate is `in_use_unknown`
//!      until the device is detached. A name that does not resolve is read
//!      again first (`d_path` follows renames, so a rename racing the
//!      lookup shows the new name), up to [`LOOP_REREADS`] times.
//!
//! diskgc out of file descriptors, a listing that fails or a backing file
//! or device that cannot be read for another reason is an error of the
//! run (as the process list's is, TASK-166), never a shorter list.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::ScanError;
use super::scan::with_descriptor_advice;
use crate::host::{Dev, DevIno, Host, HostError, LoopIdentity, MountEntry};
use crate::places::{may_show_other_owners, mount_for, stored_elsewhere};
use crate::resolve::{Want, resolve_in};

/// How often a backing file's text is read again when it names nothing
/// here and changed since the last read (a rename racing the lookup).
pub const LOOP_REREADS: usize = 3;

/// The configured loop devices as the in-use check uses them.
#[derive(Debug, Clone, Default)]
pub struct LoopBackings {
    /// Each backing file's identity, with its loop device.
    pub uses: HashMap<DevIno, Dev>,
    /// Why some device's backing file could be in any candidate: every
    /// candidate is `in_use_unknown`. The first cause found.
    pub unknown: Option<String>,
    /// Why some device's backing file could be in a candidate on this
    /// filesystem (by `statx` device): those candidates are
    /// `in_use_unknown`.
    pub unknown_on: BTreeMap<Dev, String>,
    /// Why some device's backing file, none of the owners' as stored, could
    /// still show as an owner's on this mount (by mount id: one that may
    /// show other owners, [`crate::places::may_show_other_owners`]): candidates on
    /// it are `in_use_unknown` (TASK-299).
    pub unknown_through: BTreeMap<u64, String>,
    /// Devices whose backing file no candidate can hold, and why.
    pub notes: Vec<String>,
}

/// Lists the configured loop devices and judges each backing file (module
/// docs). `owners` are the users whose files candidates may hold (the
/// walk's owners).
pub fn read<H: Host>(host: &H, owners: &[u32]) -> Result<LoopBackings, ScanError> {
    let listed = host.loop_backing_files().map_err(run_error)?;
    let mut backings = LoopBackings::default();
    if listed.is_empty() {
        return Ok(backings);
    }
    let mounts = host.mountinfo().map_err(run_error)?;
    for (device, text) in listed {
        judge(host, &mounts, owners, device, text, &mut backings)?;
    }
    Ok(backings)
}

fn run_error(e: HostError) -> ScanError {
    if e.is_out_of_descriptors() {
        ScanError::Failed(with_descriptor_advice(e))
    } else {
        ScanError::Failed(e)
    }
}

/// What the backing file's name shows (step 2 of the module docs).
enum Named {
    /// No candidate can hold the file: why.
    Nowhere(String),
    /// None of the owners owns the file as stored: no candidate on a mount
    /// that shows owners as stored can hold it; why.
    ForeignOwner(String),
    /// It could be in a candidate on this filesystem: why.
    On(Dev, String),
    /// It could be in any candidate: why; `reread`: the text may have
    /// changed (a lookup that failed).
    Anywhere { why: String, reread: bool },
}

fn judge<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    owners: &[u32],
    device: Dev,
    first: PathBuf,
    backings: &mut LoopBackings,
) -> Result<(), ScanError> {
    let unavailable = match host.loop_backing_identity(device).map_err(run_error)? {
        LoopIdentity::Known(id) => {
            backings.uses.insert(id, device);
            if let Some(why) = stacked_alias(mounts, id, &first) {
                backings.unknown.get_or_insert_with(|| {
                    format!(
                        "loop device {device} is backed by {} and its file cannot be told apart \
                         from a candidate: {why}. Detach it (`losetup -d`, `udisksctl \
                         loop-delete`)",
                        first.display()
                    )
                });
            }
            return Ok(());
        }
        LoopIdentity::Detached => return Ok(()),
        LoopIdentity::Unavailable(why) => why,
    };
    let mut text = first;
    for _ in 0..=LOOP_REREADS {
        match by_name(host, mounts, owners, &text)? {
            Named::Nowhere(why) => {
                backings.notes.push(format!(
                    "loop device {device}: its backing file {} is in no candidate ({why})",
                    text.display()
                ));
                return Ok(());
            }
            Named::ForeignOwner(why) => {
                let showing: Vec<&MountEntry> = mounts
                    .iter()
                    .filter(|mount| may_show_other_owners(mount))
                    .collect();
                backings.notes.push(format!(
                    "loop device {device}: its backing file {} is in no candidate{} ({why})",
                    text.display(),
                    if showing.is_empty() {
                        ""
                    } else {
                        " but one on a mount that may show other owners"
                    }
                ));
                for mount in showing {
                    let shows = format!(
                        "{why}, but {} mount {} may show it as an owner's",
                        shown_type(mount),
                        mount.mount_point.display()
                    );
                    backings
                        .unknown_through
                        .entry(mount.id)
                        .or_insert_with(|| cause(device, &text, &unavailable, &shows));
                }
                return Ok(());
            }
            Named::On(fs, why) => {
                backings
                    .unknown_on
                    .entry(fs)
                    .or_insert_with(|| cause(device, &text, &unavailable, &why));
                return Ok(());
            }
            Named::Anywhere { why, reread } => {
                if reread {
                    match host.loop_backing_file(device).map_err(run_error)? {
                        // Detached meanwhile: it holds nothing now.
                        None => return Ok(()),
                        Some(again) if again != text => {
                            text = again;
                            continue;
                        }
                        Some(_) => {}
                    }
                }
                backings
                    .unknown
                    .get_or_insert_with(|| cause(device, &text, &unavailable, &why));
                return Ok(());
            }
        }
    }
    backings.unknown.get_or_insert_with(|| {
        cause(
            device,
            &text,
            &unavailable,
            &format!("its name kept changing over {} reads", LOOP_REREADS + 1),
        )
    });
    Ok(())
}

/// The `in_use_unknown` detail of a device whose file cannot be told apart
/// from a candidate.
fn cause(device: Dev, text: &Path, unavailable: &str, why: &str) -> String {
    format!(
        "loop device {device} is backed by {} and its file cannot be told apart from a \
         candidate: {why}; the device does not say ({unavailable}). Detach it (`losetup -d`, \
         `udisksctl loop-delete`), or run diskgc where it can open the device (root, the \
         `disk` group)",
        text.display()
    )
}

fn by_name<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    owners: &[u32],
    text: &Path,
) -> Result<Named, ScanError> {
    if let Some(name) = text.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
        let name = Path::new(std::ffi::OsStr::from_bytes(name));
        let parent = name.parent().filter(|p| p.is_absolute());
        let Some(parent) = parent else {
            return Ok(Named::Anywhere {
                why: "the name it was opened by was deleted, and has no parent directory"
                    .to_owned(),
                reread: false,
            });
        };
        return Ok(match resolve_in(host, mounts, parent, Want::Directory) {
            // An overlay shows its layers' files under its own device;
            // another link lies on a layer's filesystem under that one's.
            Ok(dir) if stored_elsewhere(&dir.mount.fs_type) => Named::Anywhere {
                why: format!(
                    "the name it was opened by was deleted on {} mount {}, which stores its \
                     files in another filesystem's directories, so another link of the file \
                     need not show its device",
                    dir.mount.fs_type,
                    dir.mount.mount_point.display()
                ),
                reread: false,
            },
            Ok(dir) => Named::On(
                dir.stat.dev,
                format!(
                    "the name it was opened by was deleted, and another link of the file can \
                     be anywhere on the filesystem of {}",
                    parent.display()
                ),
            ),
            Err(crate::resolve::ResolveError::Host(e)) if e.is_out_of_descriptors() => {
                return Err(run_error(e));
            }
            Err(e) => Named::Anywhere {
                why: format!(
                    "the name it was opened by was deleted, and its directory cannot be \
                     reached ({e})"
                ),
                reread: false,
            },
        });
    }
    if !text.is_absolute() {
        // `d_path` prints a relative text only for an object in no
        // directory (an anonymous inode).
        return Ok(Named::Nowhere("an object in no directory".to_owned()));
    }
    Ok(match resolve_in(host, mounts, text, Want::Any) {
        Ok(resolved) if may_show_other_owners(&resolved.mount) => Named::Anywhere {
            why: format!(
                "its name reaches the file through {} mount {}, which may show another owner \
                 than the one stored",
                shown_type(&resolved.mount),
                resolved.mount.mount_point.display()
            ),
            reread: false,
        },
        Ok(resolved) => match resolved.stat.uid {
            Some(uid) if !owners.contains(&uid) => Named::ForeignOwner(format!(
                "its name reaches a file of uid {uid}, which no candidate of this run may hold"
            )),
            Some(_) => Named::Anywhere {
                why: "its name reaches a file the run's owners hold, which need not be the \
                      device's own (a mount over its directory hides that)"
                    .to_owned(),
                reread: false,
            },
            None => Named::Anywhere {
                why: "the kernel did not report the owner of the file its name reaches".to_owned(),
                reread: false,
            },
        },
        Err(crate::resolve::ResolveError::Host(e)) if e.is_out_of_descriptors() => {
            return Err(run_error(e));
        }
        Err(e) => Named::Anywhere {
            why: format!("its name reaches nothing here ({e})"),
            reread: true,
        },
    })
}

/// How a mount that may show other owners is named in messages: its type,
/// and `idmapped` where it is.
fn shown_type(mount: &MountEntry) -> String {
    if mount.idmapped {
        format!("idmapped {}", mount.fs_type)
    } else {
        mount.fs_type.clone()
    }
}

/// Why the identity `id` the device reported may be no other name's
/// (TASK-299): the kernel takes it from the path the file was opened by,
/// and a filesystem stored in another's directories ([`stored_elsewhere`]:
/// overlayfs, eCryptfs, FUSE) reports its own device there, or for
/// overlayfs a layer's anonymous one and perhaps the copy-up origin's
/// inode, never what a name on the underlying filesystem shows. Told by
/// the identity's device being such a mount's, or by the name the device
/// prints (its parent, when deleted) lying on such a mount, from the table
/// alone; a name the table cannot place is a doubt too. `None`: an
/// identity other names of the file share.
pub(crate) fn stacked_alias(mounts: &[MountEntry], id: DevIno, text: &Path) -> Option<String> {
    if let Some(mount) = mounts
        .iter()
        .find(|mount| mount.dev == id.dev && stored_elsewhere(&mount.fs_type))
    {
        return Some(format!(
            "its identity {id} is the device of {} mount {}, which shows files stored in \
             another filesystem's directories, where their other names show other identities",
            mount.fs_type,
            mount.mount_point.display()
        ));
    }
    let bytes = text.as_os_str().as_bytes();
    let name = Path::new(std::ffi::OsStr::from_bytes(
        bytes.strip_suffix(b" (deleted)").unwrap_or(bytes),
    ));
    if !name.is_absolute() {
        return None;
    }
    match mount_for(mounts, name) {
        Ok(mount) if stored_elsewhere(&mount.fs_type) => Some(format!(
            "it was opened through {} mount {}, which shows files stored in another \
             filesystem's directories under identities their other names do not have",
            mount.fs_type,
            mount.mount_point.display()
        )),
        Ok(_) => None,
        Err(why) => Some(format!(
            "the mount its name {} is on cannot be told ({why:?})",
            name.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mount(id: u64, dev: (u32, u32), at: &str, fs_type: &str) -> MountEntry {
        MountEntry {
            id,
            parent_id: if id == 1 { 0 } else { 1 },
            dev: Dev {
                major: dev.0,
                minor: dev.1,
            },
            fs_root: PathBuf::from("/"),
            mount_point: PathBuf::from(at),
            fs_type: fs_type.to_owned(),
            source: fs_type.to_owned(),
            read_only: false,
            idmapped: false,
        }
    }

    /// TASK-299: overlayfs without `xino` on layers of several filesystems
    /// shows a layer's file under that layer's anonymous device, which no
    /// mount lists: only the name the device prints shows the overlay. And
    /// an overlay's own device tells it whatever the name says (another
    /// namespace's rendering). Plain filesystems, bind mounts and btrfs
    /// subvolumes (whose `statx` device the table does not list either)
    /// keep an exact identity.
    #[test]
    fn an_identity_shown_through_a_stacking_filesystem_is_an_alias() {
        let mounts = [
            mount(1, (254, 1), "/", "ext4"),
            mount(60, (0, 60), "/merged", "overlay"),
            mount(61, (0, 61), "/home/u", "ecryptfs"),
            mount(62, (0, 62), "/data", "btrfs"),
        ];
        let id = |major, minor| DevIno {
            dev: Dev { major, minor },
            ino: 12,
        };
        let pseudo = id(0, 99);
        for (identity, text, alias) in [
            (pseudo, "/merged/img", true),
            (pseudo, "/merged/img (deleted)", true),
            (id(0, 60), "/ns/img", true),
            (id(0, 61), "/home/u/img", true),
            (id(254, 1), "/srv/img", false),
            (id(254, 1), "/srv/img (deleted)", false),
            (id(0, 77), "/data/sub/img", false),
            (pseudo, "anon_inode:[loop]", false),
        ] {
            assert_eq!(
                stacked_alias(&mounts, identity, Path::new(text)).is_some(),
                alias,
                "{identity} by {text}"
            );
        }
        // A table that cannot place the name: in doubt.
        assert!(stacked_alias(&mounts[1..], id(254, 1), Path::new("/srv/img")).is_some());
    }
}
