//! Loop devices in the in-use scan (docs/safety.md, In use: Loop devices).
//!
//! A configured loop device holds its backing file open in the kernel, where
//! no process's fds, maps or directories show it: an image set up with
//! `udisksctl loop-setup` or `losetup` and the file deleted under it loses
//! its data the moment the device detaches (TASK-169). So every scan lists
//! the configured loop devices ([`Host::loop_backing_files`]) and turns each
//! backing file into what the candidates are matched by, its identity
//! (device, inode), found by the safe lookup every root takes
//! ([`crate::resolve`]: no symlink followed, network and FUSE mounts never
//! touched).
//!
//! The kernel prints the backing file's path for diskgc's root and mount
//! namespace (`d_path`). What that text can and cannot tell decides each
//! device:
//!
//! - it names an object here: that object's identity is a use, whatever
//!   name a candidate holds it by (hard links included);
//! - it ends in ` (deleted)`: the name the device opened is gone and the
//!   file's identity cannot be learned; another name of it (a hard link)
//!   could be inside any candidate, so the scan blocks every candidate
//!   (`in_use_unknown`) until the device is detached;
//! - it names nothing the eligibility walk could reach (missing,
//!   unsearchable, a symlink or a network or FUSE mount on the way, not an
//!   absolute path): the walk reaches candidates by the same safe lookups
//!   as the diskgc user, so no candidate holds the file by that name. The
//!   text is read once more first: a backing file renamed between the
//!   listing and the lookup shows its new name (`d_path` follows renames),
//!   which is then looked up instead. What stays unreachable is noted, not
//!   blocking. Limit: a device set up in another mount namespace (a
//!   privileged container's) prints that namespace's path, which can name
//!   nothing or another file here while the real file is in a candidate;
//! - anything else (an I/O error, a kernel without the inode number): the
//!   scan blocks every candidate.
//!
//! diskgc out of file descriptors, a listing that fails or a backing file
//! that cannot be read is an error of the run (as the process list's is,
//! TASK-166), never a shorter list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::ScanError;
use super::scan::with_descriptor_advice;
use crate::host::{Dev, DevIno, Errno, Host, HostError, MountEntry};
use crate::resolve::{ResolveError, Want, resolve_in};

/// How often a backing file's text is read again when it names nothing
/// here and changed since the last read (a rename racing the lookup). A
/// text still changing after that blocks every candidate.
pub const LOOP_REREADS: usize = 3;

/// What every `in_use_unknown` detail a loop device causes starts with: the
/// human summary recognises the cause by it (not the host's load).
pub const LOOP_UNKNOWN: &str = "a loop device's backing file cannot be told apart from a candidate";

/// The configured loop devices as the in-use check uses them.
#[derive(Debug, Clone, Default)]
pub struct LoopBackings {
    /// Each backing file's identity, with its loop device.
    pub uses: HashMap<DevIno, Dev>,
    /// Why some device's backing file could be in any candidate unseen:
    /// every candidate is `in_use_unknown`. The first cause found.
    pub unknown: Option<String>,
    /// Devices whose backing file names nothing a candidate can hold.
    pub notes: Vec<String>,
}

/// Lists the configured loop devices and judges each backing file (module
/// docs).
pub fn read<H: Host>(host: &H) -> Result<LoopBackings, ScanError> {
    let listed = host.loop_backing_files().map_err(run_error)?;
    let mut backings = LoopBackings::default();
    if listed.is_empty() {
        return Ok(backings);
    }
    let mounts = host.mountinfo().map_err(run_error)?;
    for (device, text) in listed {
        judge(host, &mounts, device, text, &mut backings)?;
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

/// What one lookup of a backing file's text found.
enum Found {
    Object(DevIno),
    /// Names nothing a candidate can hold: why.
    Unreachable(String),
    /// Could be anywhere: why.
    Unknown(String),
}

fn judge<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    device: Dev,
    first: PathBuf,
    backings: &mut LoopBackings,
) -> Result<(), ScanError> {
    let mut text = first;
    for _ in 0..=LOOP_REREADS {
        let found = look_up(host, mounts, &text)?;
        let why = match found {
            Found::Object(id) => {
                backings.uses.insert(id, device);
                return Ok(());
            }
            Found::Unknown(why) => {
                block(backings, device, &text, &why);
                return Ok(());
            }
            Found::Unreachable(why) => why,
        };
        match host.loop_backing_file(device).map_err(run_error)? {
            // Detached meanwhile: it holds nothing now.
            None => return Ok(()),
            Some(again) if again == text => {
                backings.notes.push(format!(
                    "loop device {device}: its backing file {} names nothing a candidate can \
                     hold here ({why}); if the device was set up in another mount namespace, \
                     a candidate holding the file is not seen",
                    text.display()
                ));
                return Ok(());
            }
            Some(again) => text = again,
        }
    }
    block(
        backings,
        device,
        &text,
        &format!("its name kept changing over {} reads", LOOP_REREADS + 1),
    );
    Ok(())
}

fn block(backings: &mut LoopBackings, device: Dev, text: &Path, why: &str) {
    backings.unknown.get_or_insert_with(|| {
        format!(
            "{LOOP_UNKNOWN}: loop device {device} is backed by {} ({why}); detach it \
             (`losetup -d`, `udisksctl loop-delete`) or wait until it is",
            text.display()
        )
    });
}

fn look_up<H: Host>(host: &H, mounts: &[MountEntry], text: &Path) -> Result<Found, ScanError> {
    if text.as_os_str().as_encoded_bytes().ends_with(b" (deleted)") {
        return Ok(Found::Unknown(
            "the name it was opened by was deleted; another name of the file could be in \
             any candidate"
                .to_owned(),
        ));
    }
    if !text.is_absolute() {
        // `d_path` prints a relative text only for objects in no
        // directory (an anonymous inode).
        return Ok(Found::Unreachable("not an absolute path".to_owned()));
    }
    Ok(match resolve_in(host, mounts, text, Want::Any) {
        Ok(resolved) => match resolved.stat.ino {
            Some(ino) => Found::Object(DevIno {
                dev: resolved.stat.dev,
                ino,
            }),
            None => Found::Unknown("the kernel did not report its inode number".to_owned()),
        },
        Err(
            e @ (ResolveError::Missing { .. }
            | ResolveError::UnsafePath { .. }
            | ResolveError::Screened { .. }),
        ) => Found::Unreachable(e.to_string()),
        // Not searchable by diskgc's user: the walk, which runs as that
        // user, cannot enter what holds it either.
        Err(ResolveError::Host(e)) if e.is(Errno::EACCES) => Found::Unreachable(e.to_string()),
        Err(ResolveError::Host(e)) if e.is_out_of_descriptors() => return Err(run_error(e)),
        Err(e @ (ResolveError::Host(_) | ResolveError::UnsupportedFs { .. })) => {
            Found::Unknown(e.to_string())
        }
    })
}
