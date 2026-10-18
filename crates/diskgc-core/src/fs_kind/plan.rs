//! Discovery and judgement of `fs` candidates for a dry run.
//!
//! **Discovery** walks each root by the patterns, one level per segment:
//! a directory is listed only when an active pattern has a `*` at that
//! level, otherwise its literal names are looked up directly; only names
//! that match some active pattern are ever `statx`ed. Below a root it
//! never follows a symlink (`statx` does not follow, directories are
//! opened with `open_beneath`), never crosses a mount, and never descends
//! into a candidate (so a candidate inside another one of the same
//! profile is not found: the outer one is). A name that is a mount point
//! in `mountinfo` is never `statx`ed (a dead FUSE mount cannot hang the
//! walk); as a candidate it is `contains_mount`. `systemd-private-*` is
//! never a candidate and never entered, and a root inside one is never
//! searched. The deadline is checked before every `statx` and after every
//! listing. Roots are walked outermost first, and a candidate reached
//! again (overlapping roots, two patterns) is listed once, by identity.
//!
//! **Judgement** of each candidate, as it is found (its parent directory is
//! open then; the walk holds at most one fd per pattern level), in this
//! order: a mount inside it by `mountinfo` (`contains_mount`, before
//! anything looks inside), the source's own `where` (a candidate at all?),
//! another filesystem than `PATH`'s (`other_fs`), a keep marker in any
//! directory from the root down to it (`kept`), a directory on the way
//! below the root named like a checkout's metadata (`contains_checkout`,
//! by name only: `proj/.git/objects`), the engine's eligibility
//! walk (ownership of every entry, keep markers, sockets and FIFOs,
//! special content, VCS checkouts, the entry budget, fail closed), the
//! identity found, the idle requirement on the newest change in the tree,
//! and the in-use check. Read-only: no lock, no delete, so a `flock`ed
//! candidate (`locked`) cannot be told.
//!
//! Fusing discovery and judgement is a dry-run shape: a reclaim needs the
//! candidate's `flock`, the walk, a fresh in-use check and the delete
//! together, so task-18 splits them (discover, then act per candidate).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Find, PRIVATE_TMP_PREFIX};
use crate::cargo_target::Stop;
use crate::engine::{
    self, Candidate, DEFAULT_MAX_ENTRIES, Identity, KEEP_MARKER, Reason, Rules, Skip, WalkStop,
    checkout_metadata_below, is_network_fs,
};
use crate::host::{
    Access, Base, Dev, DevIno, DirListing, Errno, FileType, Host, MonoTime, Stat, Timestamp,
};
use crate::inuse::{Options, REUSE_LIMIT, Scan, scan};
use crate::profile::{EntryType, IdleFor, Idleness, Unknown};
use crate::resolve::{ResolveError, Resolved, Want, open_beneath, resolve};

/// A candidate that passed every check at the time of the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eligible {
    /// The newest mtime or ctime in its tree (the `fs` idle measure);
    /// `None` when some entry's time was not reported.
    pub newest_change: Option<Timestamp>,
    /// Entries the walk checked, the candidate included.
    pub entries: usize,
    /// Their allocated bytes (per name: an estimate).
    pub bytes: u64,
}

/// What [`plan`] found.
#[derive(Debug, Default)]
pub struct Planned {
    /// Every candidate, judged, in the order found.
    pub judged: Vec<(PathBuf, Result<Eligible, Skip>)>,
    /// Roots and directories on the way that could not be read or must not
    /// be (a missing root, a root on a network filesystem or reached through
    /// a symlink, an unreadable directory, a candidate whose own `where`
    /// cannot be told), each with its reason.
    pub refused: Vec<Skip>,
    /// Candidates found again (another root or pattern reached the same
    /// object): listed once.
    pub duplicates: Vec<PathBuf>,
    /// `systemd-private-*` names a pattern matched: never candidates.
    pub private_tmp: usize,
}

/// How a candidate is judged: the walk's rules, the idle requirement, the
/// device of `PATH`'s filesystem, and the process scans.
struct Judge<'r> {
    rules: &'r Rules<'r>,
    idle_for: Option<IdleFor>,
    dev: Dev,
    scans: Scans<'r>,
}

/// The process scans of a plan: one is reused for later candidates while
/// it is younger than `reuse` (a plan deletes nothing, so the deletion's
/// freshness bound, which `inuse::Checker` enforces, does not apply). A
/// scan that blocks every candidate (unsettled: processes kept starting
/// while it read; or an unreadable process of the user) is reused only
/// within [`REUSE_LIMIT`], as for a deletion, so one busy moment does not
/// mark a minute's worth of candidates `in_use`.
struct Scans<'o> {
    options: Options<'o>,
    reuse: Duration,
    latest: Option<Scan>,
}

impl Scans<'_> {
    fn current<H: Host>(&mut self, host: &H) -> Result<&Scan, crate::host::HostError> {
        let now = host.now();
        let fresh = self.latest.as_ref().is_some_and(|s| {
            let reuse = if s.blocked().is_some() {
                self.reuse.min(REUSE_LIMIT)
            } else {
                self.reuse
            };
            now.saturating_since(s.started()) < reuse
        });
        if !fresh {
            self.latest = None;
            self.latest = Some(scan(host, &self.options)?);
        }
        Ok(self.latest.as_ref().expect("made above"))
    }
}

/// Finds the candidates of `find` below `roots` (absolute, `{uid}`
/// replaced) and judges each (see the module docs). `dev` is the device of
/// `PATH`'s filesystem. Errors of the run (a failing disk, no fds left, a
/// process scan that fails, the deadline) stop it: they are returned with
/// what was judged until then. Anything about one root, directory or
/// candidate is in the result. A process scan serves the candidates
/// judged within `scan_reuse` of its start. `log` gets a progress line
/// every 10,000 candidates.
#[allow(clippy::too_many_arguments)]
pub fn plan<H: Host>(
    host: &H,
    roots: &[PathBuf],
    find: &Find,
    dev: Dev,
    rules: &Rules<'_>,
    idle_for: Option<IdleFor>,
    scan_reuse: Duration,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
) -> (Planned, Option<Stop>) {
    let mut out = Planned::default();
    let stopped = plan_into(
        host, roots, find, dev, rules, idle_for, scan_reuse, deadline, log, &mut out,
    )
    .err();
    (out, stopped)
}

#[allow(clippy::too_many_arguments)]
fn plan_into<H: Host>(
    host: &H,
    roots: &[PathBuf],
    find: &Find,
    dev: Dev,
    rules: &Rules<'_>,
    idle_for: Option<IdleFor>,
    scan_reuse: Duration,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
    out: &mut Planned,
) -> Result<(), Stop> {
    let mounts: Vec<PathBuf> = host
        .mountinfo()
        .map_err(|error| Stop::Failed {
            path: PathBuf::from("/proc/self/mountinfo"),
            error,
        })?
        .into_iter()
        .map(|m| m.mount_point)
        .collect();
    let mut seen = HashSet::new();
    let mut judge = Judge {
        rules,
        idle_for,
        dev,
        scans: Scans {
            options: Options::from_rules(rules),
            reuse: scan_reuse,
            latest: None,
        },
    };
    let mut ordered: Vec<&PathBuf> = roots.iter().collect();
    ordered.sort_by_key(|root| root.components().count());
    for root in ordered {
        // Never searched from inside systemd's PrivateTmp, also when a
        // root names one: checked on the root's own names, before it is
        // opened.
        if root.components().any(|c| {
            c.as_os_str()
                .as_bytes()
                .starts_with(PRIVATE_TMP_PREFIX.as_bytes())
        }) {
            out.private_tmp += 1;
            log(&format!(
                "root {}: inside systemd's PrivateTmp ({PRIVATE_TMP_PREFIX}*): never searched",
                root.display()
            ));
            continue;
        }
        let resolved = match resolve(host, root, Want::Directory) {
            Ok(resolved) => resolved,
            Err(e) => {
                out.refused.push(unreachable_root(root, e)?);
                continue;
            }
        };
        if is_network_fs(&resolved.mount.fs_type) {
            out.refused.push(Skip::new(
                Reason::UnsupportedFs,
                root.clone(),
                format!(
                    "on a network filesystem ({}): never a candidate's filesystem",
                    resolved.mount.fs_type
                ),
            ));
            continue;
        }
        let active: Vec<usize> = (0..find.patterns.len()).collect();
        let mut walk = Walk {
            host,
            root,
            find,
            mounts: &mounts,
            judge: &mut judge,
            deadline,
            out,
            seen: &mut seen,
            log,
        };
        walk.visit(&resolved, root, 0, &active, None)?;
    }
    Ok(())
}

/// A root that cannot be reached: its skip reason (`unsafe_path`,
/// `missing`, `unsupported_fs`, `unreadable`), or an error of the run.
fn unreachable_root(root: &Path, e: ResolveError) -> Result<Skip, Stop> {
    let reason = match &e {
        ResolveError::UnsafePath { .. } => Reason::UnsafePath,
        ResolveError::UnsupportedFs { .. } => Reason::UnsupportedFs,
        ResolveError::Missing { .. } => Reason::Missing,
        ResolveError::Host(error) => return skip_or_stop(root.to_owned(), "open", error),
    };
    Ok(Skip::new(reason, root.to_owned(), e.to_string()))
}

/// A host error at `path`, classified as the eligibility walk does
/// (`ENOENT` changed, `EACCES`/`EPERM` unreadable, anything else an error
/// of the run).
fn skip_or_stop(path: PathBuf, what: &str, e: &crate::host::HostError) -> Result<Skip, Stop> {
    match engine::host_failure(path, what, e) {
        WalkStop::Skipped(skip) => Ok(skip),
        WalkStop::Failed { path, error } => Err(Stop::Failed { path, error }),
        WalkStop::DeadlinePassed { path } => Err(Stop::DeadlinePassed { path }),
    }
}

fn type_matches(wanted: EntryType, found: FileType) -> bool {
    match wanted {
        EntryType::Dir => found == FileType::Directory,
        EntryType::File => found == FileType::Regular,
        EntryType::Any => matches!(found, FileType::Directory | FileType::Regular),
    }
}

/// The walk of one root.
struct Walk<'a, 'r, H: Host> {
    host: &'a H,
    /// The root being walked (the path rules look below it).
    root: &'a Path,
    find: &'a Find,
    mounts: &'a [PathBuf],
    judge: &'a mut Judge<'r>,
    deadline: Option<MonoTime>,
    out: &'a mut Planned,
    seen: &'a mut HashSet<DevIno>,
    log: &'a mut dyn FnMut(&str),
}

impl<H: Host> Walk<'_, '_, H> {
    fn check_deadline(&self, path: &Path) -> Result<(), Stop> {
        if self.deadline.is_some_and(|d| self.host.now() >= d) {
            return Err(Stop::DeadlinePassed {
                path: path.to_owned(),
            });
        }
        Ok(())
    }

    /// The directory `dir` at `path`, `level` names below the root, with
    /// the patterns `active` (indices) still matching the way down, and
    /// the first keep marker above it (`kept`).
    fn visit(
        &mut self,
        dir: &Resolved<H::Fd>,
        path: &Path,
        level: usize,
        active: &[usize],
        kept: Option<&Path>,
    ) -> Result<(), Stop> {
        self.check_deadline(path)?;
        let (names, marker_here) = match self.names(dir, path, level, active)? {
            Ok(found) => found,
            Err(skip) => {
                self.out.refused.push(skip);
                return Ok(());
            }
        };
        // A listing of a huge directory can itself take long.
        self.check_deadline(path)?;
        let own_marker = marker_here.then(|| path.join(KEEP_MARKER));
        let kept = kept.or(own_marker.as_deref());
        for name in &names {
            let matching: Vec<usize> = active
                .iter()
                .copied()
                .filter(|&i| {
                    self.find.patterns[i]
                        .segment(level)
                        .is_some_and(|s| s.matches(name))
                })
                .collect();
            if matching.is_empty() {
                continue;
            }
            if name.as_bytes().starts_with(PRIVATE_TMP_PREFIX.as_bytes()) {
                self.out.private_tmp += 1;
                continue;
            }
            let at = path.join(name);
            // A big flat directory must not run past the deadline between
            // two candidates: checked before every `statx`.
            self.check_deadline(&at)?;
            let ends = matching
                .iter()
                .any(|&i| self.find.patterns[i].depth() == level + 1);
            if self.mounts.contains(&at) {
                // Never statted: a dead mount would hang it.
                if ends {
                    self.push(
                        at.clone(),
                        Err(Skip::new(
                            Reason::ContainsMount,
                            at,
                            "a mount point (mountinfo): not touched",
                        )),
                    );
                }
                continue;
            }
            let stat = match self.host.statx(Base::Dir(&dir.fd), Path::new(name)) {
                Ok(stat) => stat,
                Err(e) if e.is(Errno::ENOENT) => continue,
                Err(e) => {
                    let skip = skip_or_stop(at, "stat", &e)?;
                    self.out.refused.push(skip);
                    continue;
                }
            };
            let (Some(file_type), Some(mnt_id)) = (stat.file_type, stat.mnt_id) else {
                self.out.refused.push(Skip::new(
                    Reason::UnsupportedFs,
                    at,
                    format!("the kernel did not report its type and mount id: {stat:?}"),
                ));
                continue;
            };
            if ends
                && type_matches(self.find.entry_type, file_type)
                && self.candidate(dir, &at, name, &stat, kept)?
            {
                // Never descended into: the outer candidate is the one.
                continue;
            }
            let deeper: Vec<usize> = matching
                .into_iter()
                .filter(|&i| self.find.patterns[i].depth() > level + 1)
                .collect();
            if deeper.is_empty() || file_type != FileType::Directory || mnt_id != dir.mount.id {
                continue;
            }
            let child = match open_beneath(
                self.host,
                &dir.fd,
                dir.mount.id,
                Path::new(name),
                Access::Path,
                Want::Directory,
            ) {
                Ok((fd, stat)) => Resolved {
                    fd,
                    mount: dir.mount.clone(),
                    stat,
                },
                Err(ResolveError::Host(e)) => {
                    let skip = skip_or_stop(at, "open", &e)?;
                    if skip.reason != Reason::Changed {
                        self.out.refused.push(skip);
                    }
                    continue;
                }
                // Gone, replaced by a symlink, or another mount: nothing of
                // this root's to find there.
                Err(_) => continue,
            };
            // A checkout's metadata is still searched, so what is found
            // in it is reported (`contains_checkout`), never just missing.
            self.visit(&child, &at, level + 1, &deeper, kept)?;
        }
        Ok(())
    }

    /// The names to look at in `dir` (sorted) and whether it holds a keep
    /// marker: the whole listing when an active pattern has a `*` at this
    /// level, otherwise the literal names (and the marker looked up).
    fn names(
        &mut self,
        dir: &Resolved<H::Fd>,
        path: &Path,
        level: usize,
        active: &[usize],
    ) -> Result<Result<(Vec<std::ffi::OsString>, bool), Skip>, Stop> {
        let mut literals = Vec::new();
        let mut glob = false;
        for &i in active {
            match self.find.patterns[i].segment(level).map(|s| s.literal()) {
                Some(Some(name)) => literals.push(name.to_owned()),
                Some(None) => glob = true,
                None => {}
            }
        }
        if glob {
            return Ok(match self.host.read_dir(&dir.fd, DEFAULT_MAX_ENTRIES) {
                Ok(DirListing::Names(mut names)) => {
                    names.sort();
                    let marker = names.iter().any(|n| n == KEEP_MARKER);
                    Ok((names, marker))
                }
                Ok(DirListing::TooMany) => Err(Skip::new(
                    Reason::TooManyEntries,
                    path.to_owned(),
                    format!("more than {DEFAULT_MAX_ENTRIES} entries: not listed"),
                )),
                Err(e) => Err(skip_or_stop(path.to_owned(), "list", &e)?),
            });
        }
        literals.sort();
        literals.dedup();
        let marker = match self.host.statx(Base::Dir(&dir.fd), Path::new(KEEP_MARKER)) {
            Ok(_) => true,
            Err(e) if e.is(Errno::ENOENT) => false,
            Err(e) => return Ok(Err(skip_or_stop(path.join(KEEP_MARKER), "stat", &e)?)),
        };
        Ok(Ok((literals, marker)))
    }

    fn push(&mut self, path: PathBuf, verdict: Result<Eligible, Skip>) {
        self.out.judged.push((path, verdict));
        if self.out.judged.len().is_multiple_of(10_000) {
            (self.log)(&format!(
                "{} candidates judged so far",
                self.out.judged.len()
            ));
        }
    }

    /// The entry `name` (at `at`, `stat` not followed) in `dir`, of the
    /// type wanted, at the end of a pattern: a candidate unless the
    /// source's own `where` says no. Judged and recorded if it is one;
    /// `true` then (it is not descended into).
    fn candidate(
        &mut self,
        dir: &Resolved<H::Fd>,
        at: &Path,
        name: &OsStr,
        stat: &Stat,
        kept: Option<&Path>,
    ) -> Result<bool, Stop> {
        // Mounts first (docs/safety.md, Before the walk): the source's own
        // `where` looks inside the candidate, and a dead mount there would
        // hang it. Refused whatever `where` would say: it cannot be asked.
        if let Some(mount) = self.mounts.iter().find(|m| m.starts_with(at) && *m != at) {
            let skip = Skip::new(
                Reason::ContainsMount,
                mount.clone(),
                "a mount point inside (mountinfo): not looked into",
            );
            if Identity::of(stat).is_none_or(|identity| self.seen.insert(identity.id)) {
                self.push(at.to_owned(), Err(skip));
            } else {
                self.out.duplicates.push(at.to_owned());
            }
            return Ok(true);
        }
        match self.source_where(dir, at, name, stat)? {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(skip) => {
                self.out.refused.push(skip);
                return Ok(false);
            }
        }
        let Some(identity) = Identity::of(stat) else {
            self.push(
                at.to_owned(),
                Err(Skip::new(
                    Reason::UnsupportedFs,
                    at.to_owned(),
                    format!("the kernel did not report its type, inode and owner: {stat:?}"),
                )),
            );
            return Ok(true);
        };
        if !self.seen.insert(identity.id) {
            self.out.duplicates.push(at.to_owned());
            return Ok(true);
        }
        let verdict = self.judge(dir, at, identity, kept)?;
        self.push(at.to_owned(), verdict);
        Ok(true)
    }

    /// The source's own `has` / `has_not` on the candidate `name` in `dir`:
    /// a regular file of that name directly in it. A file candidate holds
    /// nothing. A directory that cannot be read is neither (`Err`: the
    /// candidate is refused as `unreadable`, never taken).
    fn source_where(
        &mut self,
        dir: &Resolved<H::Fd>,
        at: &Path,
        name: &OsStr,
        stat: &Stat,
    ) -> Result<Result<bool, Skip>, Stop> {
        let (has, has_not) = (&self.find.has, &self.find.has_not);
        if has.is_none() && has_not.is_none() {
            return Ok(Ok(true));
        }
        if stat.file_type != Some(FileType::Directory) {
            return Ok(Ok(has.is_none()));
        }
        let fd = match open_beneath(
            self.host,
            &dir.fd,
            dir.mount.id,
            Path::new(name),
            Access::Path,
            Want::Directory,
        ) {
            Ok((fd, _)) => fd,
            Err(ResolveError::Host(e)) => return Ok(Err(skip_or_stop(at.to_owned(), "open", &e)?)),
            Err(e) => {
                return Ok(Err(Skip::new(
                    Reason::Changed,
                    at.to_owned(),
                    format!("not the directory it was: {e}"),
                )));
            }
        };
        let holds = |file: &str| -> Result<Result<bool, Skip>, Stop> {
            match self.host.statx(Base::Dir(&fd), Path::new(file)) {
                Ok(stat) => Ok(Ok(stat.file_type == Some(FileType::Regular))),
                Err(e) if e.is(Errno::ENOENT) => Ok(Ok(false)),
                Err(e) => Ok(Err(skip_or_stop(at.join(file), "stat", &e)?)),
            }
        };
        if let Some(file) = has {
            match holds(file)? {
                Ok(true) => {}
                other => return Ok(other),
            }
        }
        if let Some(file) = has_not {
            return Ok(holds(file)?.map(|holds| !holds));
        }
        Ok(Ok(true))
    }

    /// The checks of one candidate (see the module docs).
    fn judge(
        &mut self,
        dir: &Resolved<H::Fd>,
        at: &Path,
        identity: Identity,
        kept: Option<&Path>,
    ) -> Result<Result<Eligible, Skip>, Stop> {
        let skip = |reason, path: &Path, detail: String| {
            Ok(Err(Skip::new(reason, path.to_owned(), detail)))
        };
        if identity.id.dev != self.judge.dev {
            return skip(
                Reason::OtherFs,
                at,
                format!(
                    "on device {}, not on PATH's filesystem (device {})",
                    identity.id.dev, self.judge.dev
                ),
            );
        }
        if let Some(marker) = kept {
            return skip(
                Reason::Kept,
                marker,
                "a keep marker between the root and the candidate".to_owned(),
            );
        }
        if let Some(metadata) = checkout_metadata_below(self.root, at) {
            return skip(
                Reason::ContainsCheckout,
                &metadata,
                "inside a version-control checkout's metadata (a directory on the way below the \
                 root): deleting part of it can destroy the repository"
                    .to_owned(),
            );
        }
        let candidate = Candidate::new(dir, at).expect("a candidate's path ends in its name");
        let record = match engine::walk(self.host, &candidate, self.judge.rules, self.deadline) {
            Ok(record) => record,
            Err(WalkStop::Skipped(skip)) => return Ok(Err(skip)),
            Err(WalkStop::DeadlinePassed { path }) => return Err(Stop::DeadlinePassed { path }),
            Err(WalkStop::Failed { path, error }) => return Err(Stop::Failed { path, error }),
        };
        if record.root().identity != identity {
            return skip(
                Reason::Changed,
                at,
                "not the entry found (replaced in between)".to_owned(),
            );
        }
        let newest = record.newest_change();
        if let Some(idle_for) = self.judge.idle_for {
            let now = self.host.wall_clock();
            match idle_for.verdict(newest, now) {
                Idleness::Idle => {}
                Idleness::NotIdle => {
                    let last = newest.map_or(0, |t| t.secs);
                    return skip(
                        Reason::NotIdle,
                        at,
                        format!(
                            "newest change at {last} s, {} s before now; idle_for {} s required",
                            now.secs - last,
                            idle_for.0.as_secs()
                        ),
                    );
                }
                Idleness::Unknown(unknown) => {
                    let reason = match unknown {
                        Unknown::Unanswered => Reason::UnsupportedFs,
                        Unknown::InFuture => Reason::FutureTime,
                    };
                    return skip(
                        reason,
                        at,
                        format!(
                            "its newest change cannot be told ({unknown:?}): an unknown age is \
                             never idle"
                        ),
                    );
                }
            }
        }
        let scan = self
            .judge
            .scans
            .current(self.host)
            .map_err(|error| Stop::Failed {
                path: at.to_owned(),
                error,
            })?;
        if let Err(skip) = scan.check(&candidate, &record) {
            return Ok(Err(skip));
        }
        Ok(Ok(Eligible {
            newest_change: newest,
            entries: record.entries(),
            bytes: record.bytes(),
        }))
    }
}
