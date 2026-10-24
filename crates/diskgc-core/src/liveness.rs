//! Producer liveness (docs/safety.md, Producer liveness): the Owner PID
//! engine. Pure: the facts come from a settled in-use scan
//! ([`crate::inuse::Scan::pid_table`]) and two answers of the port taken
//! after it (the candidate's filesystem type, the pidfd probe of the
//! number), and [`owner_pid`] decides.
//!
//! Owner PID reverses the in-use scan's reading of the process list: there
//! a missing process only means "not seen using it"; here a missing pid is
//! taken as evidence that the recorded process ended. So it may answer
//! [`OwnerPid::Ended`] only where the list is provably complete: every
//! precondition fails closed ([`OwnerPid::Unknown`]), and a number match at
//! any `NSpid` level of any listed process, of any user, retains
//! ([`OwnerPid::MayBeInUse`]; a reused pid only retains).

use std::collections::BTreeMap;

use crate::host::{Pid, PidNamespace};

pub mod pytest;

/// Linux's `PID_MAX_LIMIT` on 64-bit: no pid is larger.
pub const PID_MAX_LIMIT: u32 = 4_194_304;

/// The most bytes a pid marker file may hold (Owner PID step 1): the
/// number (at most 7 digits) and one newline fit many times over; more is
/// not a marker this engine reads.
pub const MARKER_MAX_BYTES: usize = 16;

/// A pid as a name or marker records it (Owner PID step 1): decimal ASCII,
/// no sign, no leading zero, from 1 to [`PID_MAX_LIMIT`]. Anything else is
/// `None`.
pub fn parse_pid(text: &[u8]) -> Option<u32> {
    let (first, _) = text.split_first()?;
    if *first == b'0' || text.len() > 7 || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value = text
        .iter()
        .fold(0u32, |acc, digit| acc * 10 + u32::from(digit - b'0'));
    (value <= PID_MAX_LIMIT).then_some(value)
}

/// A marker file's content (Owner PID step 1): at most
/// [`MARKER_MAX_BYTES`], a [`parse_pid`] number and at most one trailing
/// newline. `None` is a malformed marker (`producer_unknown`).
pub fn parse_pid_marker(content: &[u8]) -> Option<u32> {
    if content.len() > MARKER_MAX_BYTES {
        return None;
    }
    parse_pid(content.strip_suffix(b"\n").unwrap_or(content))
}

/// The filesystems a recorded pid can be trusted on (Owner PID step 2),
/// by `fstatfs` magic (`include/uapi/linux/magic.h`). Deployment
/// assumption: a filesystem of these types that holds candidates is
/// written only by processes of this kernel. Every other type (virtiofs,
/// 9p, overlay, ZFS, FUSE, NFS, any not yet verified) fails closed.
/// `0xEF53` is shared by ext2, ext3 and ext4, all local.
pub const LOCAL_FILESYSTEMS: &[(u64, &str)] = &[
    (0x0102_1994, "tmpfs"),
    (0xEF53, "ext4"),
    (0x5846_5342, "xfs"),
    (0x9123_683E, "btrfs"),
    (0xCA45_1A4E, "bcachefs"),
    (0xF2F5_2010, "f2fs"),
];

/// The name of a local filesystem's magic, or `None`.
pub fn local_filesystem(magic: u64) -> Option<&'static str> {
    LOCAL_FILESYSTEMS
        .iter()
        .find(|(m, _)| *m == magic)
        .map(|(_, name)| *name)
}

/// What a settled scan read for PID lookups (Owner PID step 5): the held
/// `/proc`'s namespace facts, every number at any `NSpid` level of every
/// listed process with that process's pid in diskgc's namespace (the first
/// level), and the processes whose `NSpid` stayed unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidTable {
    /// The namespace facts, or why they could not be read or why they
    /// cannot be trusted (they changed during the scan).
    namespace: Result<PidNamespace, String>,
    /// The scan's process list settled: the levels are of every process.
    settled: bool,
    /// Number at any level → the smallest diskgc-namespace pid carrying it
    /// (smallest, so reports do not depend on read order).
    levels: BTreeMap<u32, Pid>,
    /// Processes whose `NSpid` could not be read after every re-read, and
    /// that were not found gone. Sorted, without duplicates.
    unreadable: Vec<Pid>,
}

impl PidTable {
    /// Owner PID's preconditions 3 and 4 (docs/safety.md): whether the
    /// held `/proc` can list every process of this kernel, or the rule that
    /// fails. `hidepid` is named before the namespace: both fail closed
    /// alike, and a rootless container can observe `hidepid` only by this
    /// text (owner_pid_is_unknown_under_hidepid). The one home of these
    /// texts for Owner PID and the pytest contract.
    pub fn process_list_complete(&self) -> Result<(), String> {
        match &self.namespace {
            Err(why) => Err(format!("PID namespace unreadable: {why}")),
            Ok(ns) if ns.hidepid => Err("the held /proc is mounted with hidepid".to_owned()),
            Ok(ns) if !ns.initial => Err("diskgc is not in the initial PID namespace".to_owned()),
            Ok(_) => Ok(()),
        }
    }

    /// An empty table of a scan with these namespace facts (read before
    /// its first listing) and whose process list did or did not settle.
    pub fn new(namespace: Result<PidNamespace, String>, settled: bool) -> Self {
        Self {
            namespace,
            settled,
            levels: BTreeMap::new(),
            unreadable: Vec::new(),
        }
    }

    /// Whether the namespace preconditions (Owner PID steps 3 and 4) hold
    /// on the facts read so far.
    pub fn preconditions_hold(&self) -> bool {
        matches!(self.namespace, Ok(ns) if ns.initial && !ns.hidepid)
    }

    /// The facts read again after the lookups: any difference from the
    /// first read, or a failure, makes them unknown.
    pub fn confirm_namespace(&mut self, after: Result<PidNamespace, String>) {
        self.namespace = match (&self.namespace, after) {
            (Ok(before), Ok(after)) if *before == after => Ok(after),
            (Ok(before), Ok(after)) => Err(format!(
                "the PID namespace facts changed during the scan ({before:?}, then {after:?})"
            )),
            (Err(why), _) => Err(why.clone()),
            (Ok(_), Err(why)) => Err(why),
        };
    }

    /// The namespace facts, or why they are unknown.
    pub fn namespace(&self) -> Result<&PidNamespace, &str> {
        self.namespace.as_ref().map_err(String::as_str)
    }

    pub fn settled(&self) -> bool {
        self.settled
    }

    /// Adds one process's `NSpid` levels (the first is its pid here).
    ///
    /// # Panics
    ///
    /// On an empty list: the readers never return one.
    pub fn add_levels(&mut self, levels: &[u32]) {
        let pid = Pid(*levels.first().expect("NSpid has at least one level"));
        for level in levels {
            self.levels
                .entry(*level)
                .and_modify(|seen| *seen = (*seen).min(pid))
                .or_insert(pid);
        }
    }

    /// Records a process whose `NSpid` stayed unreadable.
    pub fn add_unreadable(&mut self, pid: Pid) {
        if let Err(at) = self.unreadable.binary_search(&pid) {
            self.unreadable.insert(at, pid);
        }
    }

    /// The process carrying `number` at some level, if any.
    pub fn carrier(&self, number: u32) -> Option<Pid> {
        self.levels.get(&number).copied()
    }

    pub fn unreadable(&self) -> &[Pid] {
        &self.unreadable
    }
}

/// Owner PID's answer for one recorded number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerPid {
    /// A listed process carries the number at some level, or the pidfd
    /// probe found a process with it: reported `in_use` with evidence
    /// `owner_pid` and this pid (diskgc's namespace).
    MayBeInUse(Pid),
    /// No process carries the number: the recorded process ended.
    Ended,
    /// The answer cannot be reached: `producer_unknown`.
    Unknown(Unknown),
}

/// Why Owner PID could not answer: the failed rule (the report's detail),
/// and the processes that caused it, if any (its `process_blockers`, with
/// evidence `unreadable`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unknown {
    pub rule: String,
    pub blockers: Vec<Pid>,
}

impl Unknown {
    fn rule(rule: impl Into<String>) -> OwnerPid {
        OwnerPid::Unknown(Self {
            rule: rule.into(),
            blockers: Vec::new(),
        })
    }
}

/// The pidfd probe's answer for the number in diskgc's namespace
/// ([`crate::host::Host::pid_alive`]).
pub type Probe = Result<bool, String>;

/// Owner PID steps 2 to 5 for `number`, in order: the candidate's
/// filesystem must be local (`fs_magic`), the held `/proc` without
/// `hidepid` and diskgc in the initial PID namespace (read before and after
/// the scan, unchanged); then a match at any level of the table retains; a process that stayed
/// unreadable, or an unsettled scan, makes the answer unknown; last, only
/// the probe's `ESRCH` (`Ok(false)`) lets "ended" stand. `probe` is called
/// only when every other rule passed.
pub fn owner_pid(
    table: &PidTable,
    fs_magic: Result<u64, String>,
    number: u32,
    probe: impl FnOnce(Pid) -> Probe,
) -> OwnerPid {
    match fs_magic {
        Err(why) => return Unknown::rule(format!("filesystem type unreadable: {why}")),
        Ok(magic) if local_filesystem(magic).is_none() => {
            return Unknown::rule(format!(
                "filesystem type {magic:#x} is not on the local list"
            ));
        }
        Ok(_) => {}
    }
    if let Err(rule) = table.process_list_complete() {
        return Unknown::rule(rule);
    }
    if let Some(carrier) = table.carrier(number) {
        return OwnerPid::MayBeInUse(carrier);
    }
    if !table.settled {
        return Unknown::rule("the process scan did not settle");
    }
    if !table.unreadable.is_empty() {
        return OwnerPid::Unknown(Unknown {
            rule: "a process's NSpid stayed unreadable".to_owned(),
            blockers: table.unreadable.clone(),
        });
    }
    match probe(Pid(number)) {
        Ok(false) => OwnerPid::Ended,
        Ok(true) => OwnerPid::MayBeInUse(Pid(number)),
        Err(why) => Unknown::rule(format!("pidfd probe of {number} failed: {why}")),
    }
}

#[cfg(test)]
mod tests;
