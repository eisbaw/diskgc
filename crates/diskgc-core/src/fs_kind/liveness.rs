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
//! name), `pytest` (the ordered decision table of
//! [`crate::liveness::pytest::verdict`], whose facts this module gathers
//! beside the scan's: the listing of `P`, `S`'s `.lock` and Owner PID's
//! answer on it, `U`, and `R`'s identity), and the project contracts
//! `node-project` and `python-project` ([`super::project`]: their project
//! walk and rules 1 and 2 are gathered before the scan, [`prepare`], so a
//! large project never runs into a deletion's freshness window; rule 3 is
//! judged on the scan). The name rule alone ([`name_filter`]) also runs
//! before the walk, beside `app_not_running`, so a name that is no
//! instance is `filtered` cheaply.

use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::Find;
use super::discover::Found;
use crate::engine::{ProcessBlocker, ProcessEvidence, Reason, Skip, WalkRecord};
use crate::host::MonoTime;
use crate::host::{Access, Base, DirListing, Errno, FileType, Host, HostError, Pid};
use crate::inuse::Scan;
use crate::liveness::pytest::{self, Evidence, Facts, Lock, Outcome};
use crate::liveness::{OwnerPid, PidTable, owner_pid, parse_pid, parse_pid_marker};
use crate::outcome::{Judged, Stop};
use crate::profile::LivenessContract;
use crate::resolve::{ResolveError, Want, open_beneath, resolve_in};

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

/// What a contract recognises in the candidate's own path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Instance {
    /// The source has no contract.
    Unjudged,
    /// `nix-shell`: the pid in the name.
    RcPid(u32),
    /// `pytest`: a session `P/pytest-<N>` (its number), `P` named
    /// `pytest-of-*`.
    Session,
    /// `node-project`, `python-project`: by name, not inside a
    /// `node_modules`; the rest of the instance check needs I/O
    /// ([`super::project::gather`]).
    Project,
}

/// The most names a `pytest-of-<user>` listing may hold: more is a listing
/// cut short (row 2, `unreadable`). pytest numbers sessions one by one and
/// keeps three; a directory of more than this many names is not one
/// pytest made.
pub(crate) const PYTEST_OF_NAMES_MAX: usize = 65_536;

/// The contract's instance in the candidate's path, or the `filtered` skip
/// of a path that is no instance of it.
fn instance(find: &Find, path: &Path) -> Result<Instance, Skip> {
    let name = path.file_name().map(OsStrExt::as_bytes).unwrap_or_default();
    match find.liveness_contract {
        None => Ok(Instance::Unjudged),
        Some(contract @ (LivenessContract::NodeProject | LivenessContract::PythonProject)) => {
            match crate::liveness::project::inside_node_modules(path) {
                None => Ok(Instance::Project),
                Some(outer) => Err(Skip::new(
                    Reason::Filtered,
                    path.to_owned(),
                    format!(
                        "liveness_contract {}: a nested dependency directory (inside {}): it \
                         belongs to a package, and removing it would let Node resolve another \
                         copy",
                        contract.name(),
                        outer.display()
                    ),
                )),
            }
        }
        Some(LivenessContract::NixShell) => {
            rc_name_pid(name).map(Instance::RcPid).ok_or_else(|| {
                Skip::new(
                    Reason::Filtered,
                    path.to_owned(),
                    "liveness_contract nix-shell: the name holds no valid pid \
                     (nix-shell-<pid>-<suffix> or nix-develop-<pid>-<suffix>)",
                )
            })
        }
        Some(LivenessContract::Pytest) => {
            let parent = path
                .parent()
                .and_then(Path::file_name)
                .map(OsStrExt::as_bytes)
                .unwrap_or_default();
            if parent.starts_with(b"pytest-of-") && pytest::session_number(name).is_some() {
                Ok(Instance::Session)
            } else {
                Err(Skip::new(
                    Reason::Filtered,
                    path.to_owned(),
                    format!(
                        "liveness_contract pytest row {} ({}): not a session name \
                         (pytest-of-<user>/pytest-<N>, N without a leading zero, fitting \
                         64 bits)",
                        pytest::Row::NotSession.number(),
                        pytest::Row::NotSession.rule()
                    ),
                ))
            }
        }
    }
}

/// The contract's rule on the name alone, before the walk: a name that is
/// no instance of the contract is `filtered` (pytest's row 1).
pub(crate) fn name_filter(find: &Find, found: &Found) -> Result<(), Skip> {
    instance(find, &found.path).map(drop)
}

/// What a contract gathered before its process scan ([`prepare`]): for the
/// project contracts, the instance check, the project walk and rules 1
/// and 2, which need no scan and can take longer than a deletion's
/// freshness window; nothing for the others.
#[derive(Debug, Clone)]
pub(crate) enum Prepared {
    /// The source has no contract, or one judged on the scan alone.
    Nothing,
    /// The contract already refuses the candidate (`filtered`: not an
    /// instance; `not_idle`, `producer_unknown`, ...).
    Refused(Skip),
    /// A project contract's walked project, for rule 3 on the scan.
    Project(super::project::Tree),
}

/// Gathers what `judge`'s contract needs before the scan (see
/// [`Prepared`]) for the candidate `found`, in `parent`, walked as
/// `record`; the term's idle bound is `judge.idle`'s.
pub(crate) fn prepare<H: Host>(
    host: &H,
    judge: &super::Judge<'_>,
    found: &Found,
    parent: &H::Fd,
    record: &WalkRecord,
    deadline: Option<MonoTime>,
) -> Result<Prepared, Stop> {
    let Some(contract) = judge.find.liveness_contract.filter(|c| c.is_project()) else {
        return Ok(Prepared::Nothing);
    };
    Ok(
        match super::project::gather(
            host,
            contract,
            found,
            parent,
            record,
            judge.idle.at_least,
            judge.rules.max_entries,
            deadline,
        )? {
            Ok(tree) => Prepared::Project(tree),
            Err(skip) => Prepared::Refused(skip),
        },
    )
}

/// The union rule: every admitted source that found the candidate and
/// whose contract recognises it as an instance must permit it; the first
/// refusal stands, with the index of the source that refused (in
/// `sources`' order). A source whose contract does not recognise the
/// candidate (by its name, or for a project contract its prepared
/// instance check: `filtered`) carries no fact about a producer and does
/// not veto (its own term filters it). Each source comes with what
/// [`prepare`] gathered for it. `parent` is the candidate's parent
/// directory, `record` the walk of the candidate this judgement belongs
/// to.
pub(crate) fn veto<'a, H: Host>(
    host: &H,
    scan: &Scan,
    sources: impl IntoIterator<Item = (&'a Find, &'a Found, &'a Prepared)>,
    parent: &H::Fd,
    record: &WalkRecord,
) -> Result<Result<(), (usize, Skip)>, Stop> {
    for (index, (find, found, prepared)) in sources.into_iter().enumerate() {
        // The toolchain guard judges every candidate of its source, an
        // instance by name or not (TASK-94).
        if find.protect_toolchains.is_none() {
            match instance(find, &found.path) {
                Ok(Instance::RcPid(_) | Instance::Session) => {}
                Ok(Instance::Project) => {
                    if matches!(prepared, Prepared::Refused(skip) if skip.reason == Reason::Filtered)
                    {
                        continue;
                    }
                }
                Ok(Instance::Unjudged) | Err(_) => continue,
            }
        }
        if let Err(skip) = check(host, scan, find, found, parent, record, prepared)? {
            return Ok(Err((index, skip)));
        }
    }
    Ok(Ok(()))
}

/// The earliest start of a process scan that may judge the liveness
/// contracts of `sources` (each a find and what it found): the latest
/// moment any source with a contract found the candidate (Owner PID's
/// ordering, [`check`]); `None` when no source has a contract.
pub(crate) fn contract_scan_floor<'a>(
    sources: impl IntoIterator<Item = (&'a Find, &'a Found)>,
) -> Option<MonoTime> {
    sources
        .into_iter()
        .filter(|(find, _)| find.liveness_contract.is_some())
        .map(|(_, found)| found.found_at)
        .max()
}

/// Whether `find`'s contract (if any) permits reclaiming the candidate
/// `found`, judged from `scan`, which must have been made with the PID
/// lookup ([`crate::inuse::Options::with_pid_lookup`]; for `pytest` also
/// its process facts, [`crate::inuse::Options::with_pytest_facts`]) and
/// started after the candidate was found. Either violation is a bug of
/// the caller: an error of the run, never a verdict. `parent` is the
/// candidate's parent directory: the candidate is never a mount
/// (`Judge::place` refuses one, and the walk records a mount inside), so
/// its filesystem is the candidate's. `record` is the candidate's walk
/// (pytest's old-session age and the tree a `--basetemp` value may lie
/// in).
pub(crate) fn check<H: Host>(
    host: &H,
    scan: &Scan,
    find: &Find,
    found: &Found,
    parent: &H::Fd,
    record: &WalkRecord,
    prepared: &Prepared,
) -> Judged<()> {
    // The toolchain guard's live half (docs/safety.md, Toolchain pins) is
    // judged from the same scan as a contract.
    judged!(super::toolchain::live(host, scan, find, found));
    let Some(contract) = find.liveness_contract else {
        return Ok(Ok(()));
    };
    let bug = |why: String| Stop::Failed {
        path: found.path.clone(),
        error: HostError::invalid("liveness contract", found.path.display(), why),
    };
    if contract.is_project() {
        if let Err(skip) = instance(find, &found.path) {
            return Ok(Err(skip));
        }
        return match prepared {
            Prepared::Refused(skip) => Ok(Err(skip.clone())),
            Prepared::Project(tree) => {
                let mounts = super::judge::mountinfo(host)?;
                super::project::visible_use(scan, tree, &found.path, &mounts, contract.name())
                    .map_err(bug)
            }
            Prepared::Nothing => Err(bug(format!(
                "`{}` was judged without its project gathered first (prepare)",
                contract.name()
            ))),
        };
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
    let instance = match instance(find, &found.path) {
        Ok(instance) => instance,
        Err(skip) => return Ok(Err(skip)),
    };
    let failed = |error| Stop::Failed {
        path: found.path.clone(),
        error,
    };
    // The parent's filesystem must be the candidate's (it is never a
    // mount): checked, not assumed, since the local list rests on it.
    let parent_stat = host
        .statx(Base::Dir(parent), Path::new(""))
        .map_err(failed)?;
    if parent_stat.dev != found.identity.id.dev {
        return Err(bug(format!(
            "the parent is on device {}, the candidate on {}",
            parent_stat.dev, found.identity.id.dev
        )));
    }
    let magic = || host.fs_magic(parent).map_err(|e| e.to_string());
    let probe = |pid: Pid| host.pid_alive(pid);
    match instance {
        Instance::Unjudged | Instance::Project => unreachable!("judged above, or no contract"),
        Instance::RcPid(number) => Ok(nix_shell(
            scan,
            found,
            number,
            owner_pid(table, magic(), number, probe).map_err(failed)?,
        )),
        Instance::Session => {
            let Some((users, processes)) = scan.pytest_facts() else {
                return Err(bug(
                    "the process scan was made without the process facts the pytest \
                     contract needs"
                        .to_owned(),
                ));
            };
            pytest_session(
                host,
                scan,
                found,
                parent,
                &parent_stat,
                record,
                table,
                users,
                processes,
                magic,
                probe,
            )
        }
    }
}

/// `nix-shell`'s verdict from Owner PID's answer on the pid in the name.
fn nix_shell(scan: &Scan, found: &Found, number: u32, answer: OwnerPid) -> Result<(), Skip> {
    match answer {
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
    }
}

/// The pytest contract (docs/safety.md, pytest sessions): gathers the
/// facts of the decision table that are not in the scan (the listing of
/// `P`, `S`'s `.lock` and Owner PID's answer on it, `U`, `R`'s identity)
/// and maps [`pytest::verdict`] to a skip. Every detail names the row that
/// decided, so a test can assert the rule and not only the outcome.
#[allow(clippy::too_many_arguments)]
fn pytest_session<H: Host>(
    host: &H,
    scan: &Scan,
    found: &Found,
    parent: &H::Fd,
    parent_stat: &crate::host::Stat,
    record: &WalkRecord,
    table: &PidTable,
    users: &[u32],
    processes: &pytest::Processes,
    magic: impl Fn() -> Result<u64, String>,
    probe: impl Fn(Pid) -> crate::liveness::Probe,
) -> Judged<()> {
    let failed = |error| Stop::Failed {
        path: found.path.clone(),
        error,
    };
    let skip = |reason, detail: String| Ok(Err(Skip::new(reason, found.path.clone(), detail)));
    // A refusal by the kernel is this candidate's (it changed after the
    // walk read it): kept. Anything else is an error of the run, as for
    // the walk's own reads.
    let refused = |error: &HostError| {
        matches!(
            error.kernel_errno(),
            Some(Errno::EACCES | Errno::EPERM | Errno::ENOENT | Errno::ENOTDIR)
        )
    };
    let name = found.path.file_name().expect("a session has a name");
    let Some(owner) = parent_stat.uid else {
        return skip(
            Reason::ProducerUnknown,
            "liveness_contract pytest: the owner of the pytest-of directory was not reported"
                .to_owned(),
        );
    };
    // The process facts cover only the users diskgc judges for; a
    // pytest-of directory of another uid (a session of the user's inside
    // it is not one pytest makes) has a process set nobody read.
    if !users.contains(&owner) {
        return skip(
            Reason::ProducerUnknown,
            format!(
                "liveness_contract pytest: the pytest-of directory belongs to uid {owner}, \
                 whose processes this run does not read"
            ),
        );
    }
    // S itself, below P, without following anything: the lock is read
    // from the walked object, never a name looked up again elsewhere.
    let mount_id = parent_stat.mnt_id.ok_or_else(|| {
        failed(HostError::invalid(
            "statx",
            found.path.display(),
            "the kernel did not report the parent's mount id",
        ))
    })?;
    let session = match open_beneath(
        host,
        parent,
        mount_id,
        Path::new(name),
        Access::Path,
        Want::Directory,
    ) {
        Ok((fd, stat)) if stat.dev_ino() == Some(found.identity.id) => fd,
        Ok(_) => {
            return skip(
                Reason::Changed,
                "liveness_contract pytest: the session directory was replaced".to_owned(),
            );
        }
        Err(ResolveError::Host(error)) if refused(&error) => {
            return skip(
                Reason::Unreadable,
                format!("liveness_contract pytest: the session cannot be opened again: {error}"),
            );
        }
        Err(ResolveError::Host(error)) => return Err(failed(error)),
        Err(e) => {
            return skip(
                Reason::Changed,
                format!("liveness_contract pytest: the session cannot be reached again: {e}"),
            );
        }
    };
    let lock = match read_lock(host, &session) {
        Ok(lock) => lock,
        // pytest's own cleanup keeps a session whose lock it cannot
        // examine; so does row 3.
        Err(error) if refused(&error) => {
            Some(Lock::Malformed(format!("it cannot be examined: {error}")))
        }
        Err(error) => return Err(failed(error)),
    };
    let lock_owner = match &lock {
        Some(Lock::Pid { pid, .. }) => {
            Some(owner_pid(table, magic(), *pid, probe).map_err(failed)?)
        }
        _ => None,
    };
    let listing = match host.read_dir(parent, PYTEST_OF_NAMES_MAX) {
        Ok(DirListing::Names(names)) => Ok(names),
        Ok(DirListing::TooMany) => Err(format!(
            "more than {PYTEST_OF_NAMES_MAX} names (the listing was cut short)"
        )),
        // diskgc out of descriptors says nothing about P: an error of the
        // run, not an unknown listing for this session (TASK-185).
        Err(e) if e.is_out_of_descriptors() => return Err(failed(e)),
        Err(e) => Err(e.to_string()),
    };
    let process_list = table.process_list_complete();
    // R, the temp root: P's parent, reached again from the root.
    let root_path = found
        .path
        .parent()
        .and_then(Path::parent)
        .expect("a session lies in P, which lies in R");
    let mounts = super::judge::mountinfo(host)?;
    let parent_name = found
        .path
        .parent()
        .and_then(Path::file_name)
        .expect("a session lies in P");
    let root_id = match resolve_in(host, &mounts, root_path, Want::Directory) {
        // R by path must be P's parent by identity: a renamed or replaced
        // directory on the way would compare the processes' temp roots
        // with another directory.
        Ok(root) => match host.statx(Base::Dir(&root.fd), Path::new(parent_name)) {
            Ok(p) if p.dev_ino().is_some() && p.dev_ino() == parent_stat.dev_ino() => {
                root.stat.dev_ino()
            }
            Ok(_) => None,
            Err(error) if refused(&error) => None,
            Err(error) => return Err(failed(error)),
        },
        Err(ResolveError::Host(error)) if !refused(&error) => return Err(failed(error)),
        Err(_) => None,
    };
    let Some(root_id) = root_id else {
        return skip(
            Reason::ProducerUnknown,
            format!(
                "liveness_contract pytest: the temp root {} cannot be reached safely as \
                 the session's grandparent, so its use cannot be decided",
                root_path.display()
            ),
        );
    };
    let session_tree: HashSet<_> = record.objects().collect();
    let facts = Facts {
        session_name: name,
        listing: listing.as_deref().map_err(Clone::clone),
        lock,
        lock_owner,
        now: host.wall_clock(),
        newest_change: record.newest_change(),
        process_list,
        owner,
        session_path: &found.path,
        session_tree: &session_tree,
        root_path,
        root_id,
        processes,
    };
    let verdict = pytest::verdict(&facts);
    let detail = format!(
        "liveness_contract pytest row {} ({}): {}",
        verdict.row.number(),
        verdict.row.rule(),
        verdict.detail
    );
    let blockers = |entries: Vec<(Pid, Evidence)>| -> Vec<ProcessBlocker> {
        entries
            .into_iter()
            .map(|(pid, evidence)| ProcessBlocker {
                pid: pid.0,
                command: scan.command_option(pid),
                evidence: match evidence {
                    Evidence::OwnerPid => ProcessEvidence::OwnerPid,
                    Evidence::CommandLine => ProcessEvidence::CommandLine,
                    Evidence::Environment => ProcessEvidence::Environment,
                    Evidence::TempRoot => ProcessEvidence::TempRoot,
                    Evidence::UnresolvedPath => ProcessEvidence::UnresolvedPath,
                    Evidence::Unreadable => ProcessEvidence::Unreadable,
                },
            })
            .collect()
    };
    let at = found.path.clone();
    Ok(match verdict.outcome {
        Outcome::Eligible => Ok(()),
        Outcome::Filtered => Err(Skip::new(Reason::Filtered, at, detail)),
        Outcome::Unreadable => Err(Skip::new(Reason::Unreadable, at, detail)),
        Outcome::ProducerUnknown(entries) => {
            Err(Skip::new(Reason::ProducerUnknown, at, detail)
                .with_process_blockers(blockers(entries)))
        }
        Outcome::InUse(entries) => {
            Err(Skip::new(Reason::InUse, at, detail).with_process_blockers(blockers(entries)))
        }
    })
}

/// `S`'s `.lock` (Owner PID step 1's marker): absent (`None`), a regular
/// file of at most [`crate::liveness::MARKER_MAX_BYTES`] holding a pid, or
/// anything else ([`Lock::Malformed`]: not a regular file, larger, a read
/// that failed, no pid). Opened below `S`'s fd without following a
/// symlink or crossing a mount; the object read must be the one `statx`ed.
fn read_lock<H: Host>(host: &H, session: &H::Fd) -> Result<Option<Lock>, HostError> {
    let name = Path::new(".lock");
    let stat = match host.statx(Base::Dir(session), name) {
        Ok(stat) => stat,
        Err(e) if e.kernel_errno() == Some(Errno::ENOENT) => return Ok(None),
        Err(e) => return Err(e),
    };
    let malformed = |why: &str| Ok(Some(Lock::Malformed(why.to_owned())));
    if stat.file_type != Some(FileType::Regular) {
        return malformed("not a regular file");
    }
    if stat
        .size
        .is_none_or(|size| size > crate::liveness::MARKER_MAX_BYTES as u64)
    {
        return malformed("larger than a pid marker");
    }
    let Some(mtime) = stat.mtime else {
        return malformed("its mtime was not reported");
    };
    let Some(mount_id) = host.statx(Base::Dir(session), Path::new(""))?.mnt_id else {
        return malformed("the session's mount id was not reported");
    };
    let fd = match open_beneath(host, session, mount_id, name, Access::Path, Want::Any) {
        Ok((fd, again)) if again.dev_ino() == stat.dev_ino() => fd,
        Ok(_) => return malformed("replaced while it was read"),
        Err(ResolveError::Host(e)) => return Err(e),
        Err(e) => return malformed(&format!("cannot be reached: {e}")),
    };
    let content = match host.read_head(&fd, crate::liveness::MARKER_MAX_BYTES + 1) {
        Ok(content) => content,
        Err(e) => return malformed(&format!("cannot be read: {e}")),
    };
    Ok(Some(match parse_pid_marker(&content) {
        Some(pid) => Lock::Pid { pid, mtime },
        None => Lock::Malformed("its content is not a pid".to_owned()),
    }))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::rc_name_pid;
    use crate::liveness::PID_MAX_LIMIT;

    proptest! {
        // No regression file, as in liveness::tests.
        #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
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
