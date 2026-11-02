//! What visible processes hold, for the project contracts' "no visible
//! use" rule (docs/safety.md, Project dependency directories, rule 3; A
//! process's paths). Read only when a selected source has a project
//! contract ([`super::Options::project_facts`]), from the same scan as the
//! in-use check:
//!
//! - every held non-directory (an fd on a file, an executable) with its
//!   link text and mount id, for the path test of files the project walk
//!   left out ([`crate::fs_kind::project`]);
//! - every directory a process holds (its cwd, its root, an fd on a
//!   directory) climbed once per scan with `..` by fd
//!   ([`crate::host::Host::proc_climb`]), indexed by every directory the
//!   climb met, so a project is looked up in it once per directory of its
//!   walk instead of one climb per candidate.
//!
//! - every process's command line and working-directory link texts, for
//!   the string test of its arguments (a consumer started from outside
//!   the project as `node ~/p/server.js` or `p/.venv/bin/python
//!   p/server.py`, its modules cached, holds nothing in it);
//! - for a process holding an object through a mount that is not in
//!   diskgc's own mount table (another mount namespace: a rootless
//!   container's pause process, Flatpak), its own mount table and root
//!   link text, to translate that object's link text.
//!
//! The climbs run right after the scan's settled reads: a process that
//! ended meanwhile is gone (as In use takes it), one that moved its cwd
//! since is climbed from where it is now (its old cwd's identity is in the
//! scan's uses).

use std::collections::{BTreeMap, HashMap};

use super::ScanError;
use super::scan::{Held, How, Read, Use};
use crate::host::{DevIno, HeldDir, Host, MonoTime, MountEntry, Pid, ProcError};

/// A climb that could not be made: the held directory's identity (a
/// project on another device cannot hold it) and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClimbFailed {
    pub pid: Pid,
    pub how: How,
    pub id: DevIno,
    pub why: String,
}

/// What the scan's processes hold (see the module docs).
#[derive(Debug, Clone, Default)]
pub struct HeldObjects {
    /// Every held non-directory, by process.
    pub files: Vec<(Pid, Held)>,
    /// Every directory a climb met (the held one first), with the process
    /// and the link that holds the climb's start.
    pub ancestors: HashMap<DevIno, Vec<Use>>,
    /// Climbs that failed (not for a process found gone).
    pub failed: Vec<ClimbFailed>,
    /// The mount ids of diskgc's own table when the objects were collected.
    pub own_mounts: Vec<u64>,
    /// Per process holding something through a mount outside diskgc's
    /// table: its root link text and its own mount table (or why that
    /// could not be read).
    pub foreign: BTreeMap<Pid, Foreign>,
    /// Every process's command line (or why it could not be read) and the
    /// link texts of its working directories, against which a relative
    /// argument is placed.
    pub argv: BTreeMap<Pid, Argv>,
}

/// A process's command line (or why it could not be read) and the link
/// texts of its working directories.
pub type Argv = (
    Result<Vec<std::ffi::OsString>, String>,
    Vec<std::path::PathBuf>,
);

/// A process's view, for translating its held files' link text.
#[derive(Debug, Clone)]
pub struct Foreign {
    /// The link text of its root (in diskgc's frame), if read.
    pub root: Option<std::ffi::OsString>,
    pub mounts: Result<Vec<MountEntry>, String>,
}

impl HeldObjects {
    /// The first holder (by pid, then link) of a directory whose climb met
    /// `id`.
    pub fn holder_above(&self, id: DevIno) -> Option<Use> {
        self.ancestors
            .get(&id)
            .and_then(|uses| uses.first())
            .copied()
    }
}

/// The climb's start for a held directory.
fn held_dir(how: How) -> Option<HeldDir> {
    match how {
        How::Cwd => Some(HeldDir::Cwd),
        How::Root => Some(HeldDir::Root),
        How::Fd(fd) => Some(HeldDir::Fd(fd)),
        How::Exe | How::Map => None,
    }
}

/// Collects the held objects of every readable process of a scan and
/// climbs their directories (see the module docs). diskgc out of file
/// descriptors is an error of the run (TASK-185); the deadline and signals
/// are checked before each process.
pub(crate) fn collect<H: Host>(
    host: &H,
    read: &BTreeMap<Pid, Read>,
    deadline: Option<MonoTime>,
) -> Result<HeldObjects, ScanError> {
    let mut out = HeldObjects {
        own_mounts: host.mountinfo()?.iter().map(|m| m.id).collect(),
        ..HeldObjects::default()
    };
    for (pid, process) in read {
        ScanError::stop_if_due(host, deadline)?;
        let argv = match host.proc_cmdline(*pid) {
            Ok(args) => Some(Ok(args)),
            Err(ProcError::Gone) => None,
            Err(ProcError::Exhausted(e)) => {
                return Err(ScanError::Failed(super::scan::with_descriptor_advice(e)));
            }
            Err(e) => Some(Err(e.to_string())),
        };
        if let Some(argv) = argv {
            out.argv.insert(*pid, (argv, process.seen.cwds.clone()));
        }
        if process
            .held
            .iter()
            .any(|h| h.mnt_id.is_none_or(|id| !out.own_mounts.contains(&id)))
        {
            let mounts = match host.proc_mountinfo(*pid) {
                Ok(mounts) => Ok(mounts),
                Err(ProcError::Exhausted(e)) => {
                    return Err(ScanError::Failed(super::scan::with_descriptor_advice(e)));
                }
                Err(e) => Err(e.to_string()),
            };
            let root = process
                .held
                .iter()
                .find(|h| h.how == How::Root)
                .map(|h| h.text.clone());
            out.foreign.insert(*pid, Foreign { root, mounts });
        }
        let mut climbed: Vec<HeldDir> = Vec::new();
        // Found gone during a climb: its other directories are not climbed
        // (it uses nothing now); what the scan read of it is kept.
        let mut gone = false;
        for held in &process.held {
            if !held.dir {
                out.files.push((*pid, held.clone()));
                continue;
            }
            let Some(from) = held_dir(held.how) else {
                continue;
            };
            // Per task cwds differ rarely; the leader's is climbed once
            // (the others are matched by identity, as In use does).
            if gone || climbed.contains(&from) {
                continue;
            }
            climbed.push(from);
            match host.proc_climb(*pid, from) {
                Ok(chain) => {
                    for id in chain {
                        let uses = out.ancestors.entry(id).or_default();
                        let this = Use {
                            pid: *pid,
                            how: held.how,
                        };
                        if !uses.contains(&this) {
                            uses.push(this);
                        }
                    }
                }
                Err(ProcError::Gone) => gone = true,
                Err(ProcError::Exhausted(e)) => {
                    return Err(ScanError::Failed(super::scan::with_descriptor_advice(e)));
                }
                Err(ProcError::Denied(e) | ProcError::Failed(e)) => {
                    out.failed.push(ClimbFailed {
                        pid: *pid,
                        how: held.how,
                        id: held.id,
                        why: e.to_string(),
                    });
                }
            }
        }
    }
    for uses in out.ancestors.values_mut() {
        uses.sort_by_key(|u| (u.pid, u.how));
    }
    Ok(out)
}
