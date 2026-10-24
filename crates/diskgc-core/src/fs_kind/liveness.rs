//! The `liveness_contract` source predicate of kind `fs` (docs/reference.md,
//! Liveness contract predicate; docs/safety.md, Producer liveness). It is
//! judged from the same process scan as the in-use check: in the plan after
//! [`crate::inuse::Scan::check`], and in the reclaim and before a command
//! action inside the deletion's last check on that scan (under the
//! candidate's lock, immediately before the delete). A contract only
//! narrows: every other check still applies.
//!
//! In a union, every source that discovered the candidate and has a
//! contract must permit it ([`veto`], as retention guards veto): a
//! contract states a fact about the candidate's producer, which no other
//! term's admission changes.
//!
//! Implemented: `nix-shell` (Owner PID on the pid in the candidate's own
//! name). The loader refuses the other values until their steps land. The
//! name rule alone ([`name_filter`]) also runs before the walk, beside
//! `app_not_running`, so a name that is no instance is `filtered` cheaply.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::Find;
use super::discover::Found;
use crate::engine::{ProcessBlocker, ProcessEvidence, Reason, Skip};
use crate::host::{Base, Host, HostError};
use crate::inuse::Scan;
use crate::liveness::{OwnerPid, owner_pid, parse_pid};
use crate::outcome::{Judged, Stop};
use crate::profile::LivenessContract;

/// The pid in a `nix-shell` rc directory's name: `nix-shell-<pid>-<suffix>`
/// or `nix-develop-<pid>-<suffix>`, `<pid>` as Owner PID step 1 reads it,
/// `<suffix>` not empty. `None`: not an instance of the contract.
pub(crate) fn rc_name_pid(name: &[u8]) -> Option<u32> {
    let rest = name
        .strip_prefix(b"nix-shell-")
        .or_else(|| name.strip_prefix(b"nix-develop-"))?;
    let dash = rest.iter().position(|b| *b == b'-')?;
    let (digits, suffix) = (&rest[..dash], &rest[dash + 1..]);
    if suffix.is_empty() {
        return None;
    }
    parse_pid(digits)
}

/// The number a contract reads from the candidate's own name, or the
/// `filtered` skip of a name that is no instance of it. `Ok(None)`: no
/// contract.
fn instance(find: &Find, path: &Path) -> Result<Option<u32>, Skip> {
    match find.liveness_contract {
        None => Ok(None),
        // Each contract decides its own instance rule when it lands; the
        // loader refuses the others.
        Some(
            LivenessContract::Pytest
            | LivenessContract::NodeProject
            | LivenessContract::PythonProject,
        ) => Ok(None),
        Some(LivenessContract::NixShell) => path
            .file_name()
            .and_then(|name| rc_name_pid(name.as_bytes()))
            .map(Some)
            .ok_or_else(|| {
                Skip::new(
                    Reason::Filtered,
                    path.to_owned(),
                    "liveness_contract nix-shell: the name holds no valid pid \
                     (nix-shell-<pid>-<suffix> or nix-develop-<pid>-<suffix>)",
                )
            }),
    }
}

/// The contract's rule on the name alone, before the walk: a name that is
/// no instance of the contract is `filtered`.
pub(crate) fn name_filter(find: &Find, found: &Found) -> Result<(), Skip> {
    instance(find, &found.path).map(drop)
}

/// The union rule: every admitted source that found the candidate and
/// whose contract recognises it as an instance must permit it; the first
/// refusal stands, with the index of the source that refused (in
/// `sources`' order). A source whose contract does not recognise the name
/// carries no fact about a producer and does not veto (its own term
/// filters it). `parent` is the candidate's parent directory.
pub(crate) fn veto<'a, H: Host>(
    host: &H,
    scan: &Scan,
    sources: impl IntoIterator<Item = (&'a Find, &'a Found)>,
    parent: &H::Fd,
) -> Result<Result<(), (usize, Skip)>, Stop> {
    for (index, (find, found)) in sources.into_iter().enumerate() {
        if !matches!(instance(find, &found.path), Ok(Some(_))) {
            continue;
        }
        if let Err(skip) = check(host, scan, find, found, parent)? {
            return Ok(Err((index, skip)));
        }
    }
    Ok(Ok(()))
}

/// Whether `find`'s contract (if any) permits reclaiming the candidate
/// `found`, judged from `scan`, which must have been made with the PID
/// lookup ([`crate::inuse::Options::with_pid_lookup`]) and started after
/// the candidate was found. Either violation is a bug of the caller: an
/// error of the run, never a verdict. `parent` is the candidate's parent
/// directory: the candidate is never a mount (`Judge::place` refuses one,
/// and the walk record a mount inside), so its filesystem is the
/// candidate's.
pub(crate) fn check<H: Host>(
    host: &H,
    scan: &Scan,
    find: &Find,
    found: &Found,
    parent: &H::Fd,
) -> Judged<()> {
    let Some(contract) = find.liveness_contract else {
        return Ok(Ok(()));
    };
    let bug = |why: String| Stop::Failed {
        path: found.path.clone(),
        error: HostError::invalid("liveness contract", found.path.display(), why),
    };
    match contract {
        LivenessContract::NixShell => {}
        other => {
            return Err(bug(format!(
                "`{}` is not implemented; the loader refuses it",
                other.name()
            )));
        }
    }
    let Some(table) = scan.pid_table() else {
        return Err(bug(
            "the process scan was made without the PID lookup its contract needs".to_owned(),
        ));
    };
    // Owner PID's ordering (inuse::scan::pid_table): an owner alive when
    // the candidate was found is in every later listing.
    if scan.started() < found.found_at {
        return Err(bug(format!(
            "the process scan started at {:?}, before the candidate was found at {:?}",
            scan.started(),
            found.found_at
        )));
    }
    let number = match instance(find, &found.path) {
        Ok(Some(number)) => number,
        Ok(None) => unreachable!("the find has a contract"),
        Err(skip) => return Ok(Err(skip)),
    };
    // The parent's filesystem must be the candidate's (it is never a
    // mount): checked, not assumed, since the local list rests on it.
    let parent_dev = host
        .statx(Base::Dir(parent), Path::new(""))
        .map_err(|error| Stop::Failed {
            path: found.path.clone(),
            error,
        })?
        .dev;
    if parent_dev != found.identity.id.dev {
        return Err(bug(format!(
            "the parent is on device {parent_dev}, the candidate on {}",
            found.identity.id.dev
        )));
    }
    let magic = host.fs_magic(parent).map_err(|e| e.to_string());
    let answer = owner_pid(table, magic, number, |pid| {
        host.pid_alive(pid).map_err(|e| e.to_string())
    });
    Ok(match answer {
        OwnerPid::Ended => Ok(()),
        OwnerPid::MayBeInUse(pid) => Err(Skip::new(
            Reason::InUse,
            found.path.clone(),
            format!(
                "liveness_contract nix-shell: pid {pid} ({}) carries the owner pid {number} \
                 recorded in the name (a number match, not proof of identity)",
                scan.command(pid)
            ),
        )
        .with_process_blockers(vec![ProcessBlocker {
            pid: pid.0,
            command: scan.command_option(pid),
            evidence: ProcessEvidence::OwnerPid,
        }])),
        OwnerPid::Unknown(unknown) => Err(Skip::new(
            Reason::ProducerUnknown,
            found.path.clone(),
            format!(
                "liveness_contract nix-shell: whether owner pid {number} ended cannot be \
                 decided: {}",
                unknown.rule
            ),
        )
        .with_process_blockers(
            unknown
                .blockers
                .into_iter()
                .map(|pid| ProcessBlocker {
                    pid: pid.0,
                    command: scan.command_option(pid),
                    evidence: ProcessEvidence::Unreadable,
                })
                .collect(),
        )),
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::rc_name_pid;
    use crate::liveness::PID_MAX_LIMIT;

    proptest! {
        /// The rc-name split: a name built from either shape and a valid
        /// pid yields exactly that pid, whatever the suffix (dashes
        /// included).
        #[test]
        fn rc_name_yields_its_pid(
            pid in 1..=PID_MAX_LIMIT,
            develop in any::<bool>(),
            suffix in "[A-Za-z0-9-]{1,12}",
        ) {
            let shape = if develop { "nix-develop" } else { "nix-shell" };
            let name = format!("{shape}-{pid}-{suffix}");
            prop_assert_eq!(rc_name_pid(name.as_bytes()), Some(pid));
        }
    }

    #[test]
    fn names_without_a_valid_pid_are_not_instances() {
        for name in [
            "nix-shell-0042-1",
            "nix-shell-4194305-1",
            "nix-shell-abc-1",
            "nix-shell-42-",
            "nix-shell-42",
            "nix-shell--1",
            "nix-shell.ab",
            "nix-shellx-42-1",
            "Nix-shell-42-1",
        ] {
            assert_eq!(rc_name_pid(name.as_bytes()), None, "{name}");
        }
    }
}
