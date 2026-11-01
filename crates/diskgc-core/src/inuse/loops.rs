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
//!    `disk` group).
//! 2. Otherwise only the name the kernel prints (`d_path`) is left, which
//!    proves nothing about identity: a mount made over the file's directory
//!    after the device opened it makes a fresh lookup reach another file (a
//!    decoy), and a name diskgc cannot look up may still have another link
//!    inside a candidate.
//!    - The name resolves (by the safe lookup every root takes, no symlink
//!      followed, network and FUSE mounts never touched) to a file none of
//!      the run's owners owns: a candidate holds only its owners' entries
//!      (the eligibility walk refuses any other), so it cannot be in one.
//!      Noted, not blocking. Limit: a decoy owned by another user over a
//!      real file of an owner's (a mount in diskgc's own mount namespace,
//!      which only root makes) hides it.
//!    - The text is not an absolute path: an anonymous object (`d_dname`),
//!      which no directory can name. Noted, not blocking.
//!    - The text ends in ` (deleted)`: the name the device opened is gone;
//!      another link of the file can only be on the same filesystem, so
//!      the candidates on the filesystem of the name's surviving parent
//!      directory are `in_use_unknown`; with no such parent, every
//!      candidate. Limit: a mount made over that parent since.
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
        Ok(resolved) => match resolved.stat.uid {
            Some(uid) if !owners.contains(&uid) => Named::Nowhere(format!(
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
