//! The judgement of one `fs` candidate that discovery found
//! ([`super::Found`]): the one set of checks that the dry run
//! ([`super::plan`]), the tally and the reclaim ([`super::reclaim`]) apply,
//! so that a plan predicts what a reclaim does (docs/safety.md, Filesystem
//! candidates). The sequence has two halves, so the reclaim can put the
//! candidate's `flock` between them and repeat the first under it:
//! [`Judge::approach`] (steps 1 and 2; again under the lock:
//! [`Judge::again`]) and [`Judge::contents`] (steps 3 and 4):
//!
//! 1. [`Judge::place`], before anything about the candidate is opened: the
//!    checkout guard's path rule (`contains_checkout`, fail closed
//!    `unsafe_path`), another filesystem than `PATH`'s (`other_fs`), a
//!    mount at or inside it by `mountinfo` (`contains_mount`, never looked
//!    into);
//! 2. [`reach`]: from its root again, one directory at a time (no symlink,
//!    no mount crossed), its root refused by its mount's line first (a
//!    network filesystem, a `systemd-private-*` bind mount), and the
//!    candidate itself `statx`ed: gone (`missing`) or not the object found
//!    (`changed`, [`same_object`]); then [`no_keep_markers`]: a keep marker
//!    in the root or any directory on the way (`kept`);
//! 3. [`Judge::source_where`]: the source's own `has` / `has_not` again; a
//!    candidate that no longer passes it is `changed`;
//! 4. [`Judge::walk`]: the deletion engine's eligibility walk with the
//!    tier's rules (ownership of every entry, keep markers, sockets and
//!    FIFOs, special content, checkouts as the profile says, the entry
//!    budget, fail closed), whose root must be the object found
//!    (`changed`), then the idle requirement on its newest change (none
//!    known: never idle).
//!
//! The in-use check is the caller's: the dry run reuses a scan for a while
//! (it deletes nothing), the reclaim checks against a fresh one under the
//! candidate's lock ([`crate::inuse::Checker`]).

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use super::discover::{Found, check_deadline};
use super::{Find, PRIVATE_TMP_PREFIX, path_in_private_tmp};
use crate::engine::{
    self, Candidate, KEEP_MARKER, Reason, Rules, Skip, WalkRecord, WalkStop,
    checkout_metadata_below, may_be_networked, networked_why,
};
use crate::host::{
    Access, Base, Dev, Errno, FileType, Host, HostError, MonoTime, MountEntry, Timestamp,
};
use crate::outcome::{Judged, Stop};
use crate::profile::{IdleFor, Idleness, Unknown};
use crate::resolve::{ResolveError, Resolved, Want, mount_for, open_beneath, resolve_in};

/// What a candidate of one tier is judged by: the source's `find` (its own
/// `where`), the walk's rules (owners, IPC policy, entry budget, checkout
/// setting), the term's idle requirement, and the device of `PATH`'s
/// filesystem.
#[derive(Debug, Clone, Copy)]
pub struct Judge<'r> {
    pub find: &'r Find,
    pub rules: &'r Rules<'r>,
    pub idle_for: Option<IdleFor>,
    pub dev: Dev,
}

/// What a walked candidate showed: the walk's record and its newest
/// change (the `fs` idle measure; `None` when some time was not reported).
#[derive(Debug)]
pub struct Walked {
    pub record: WalkRecord,
    pub newest_change: Option<Timestamp>,
}

/// What a judgement measured of a candidate: the tier's order key and its
/// size (the tally's result, and an eligible plan entry's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    /// The newest mtime or ctime in its tree; `None` when some time was not
    /// reported (ordered last, never idle).
    pub newest_change: Option<Timestamp>,
    /// Entries the walk checked, the candidate included.
    pub entries: usize,
    /// Their allocated bytes, each object once ([`Walked::bytes`]).
    pub bytes: u64,
}

impl Walked {
    /// Its order key and size.
    pub fn tally(&self) -> Tally {
        Tally {
            newest_change: self.newest_change,
            entries: self.record.entries(),
            bytes: self.bytes(),
        }
    }

    /// The allocated bytes of the candidate's objects, each object counted
    /// once however many names it has inside (docs/architecture.md, The
    /// index: `est_bytes`): an estimate of what deleting it frees. An
    /// object also linked from outside frees nothing; the measure is the
    /// observed change in available space.
    pub fn bytes(&self) -> u64 {
        let mut seen = std::collections::HashSet::new();
        self.record
            .all_entries()
            .filter(|e| seen.insert(e.identity.id))
            .map(|e| e.bytes)
            .fold(0u64, u64::saturating_add)
    }
}

impl Judge<'_> {
    /// Steps 1 and 2 (see the module docs), the approach every use of a
    /// found candidate starts with (the plan, the tally, the reclaim before
    /// its lock): its place under the mount table `mounts`, reached again
    /// from its root, and no keep marker on the way. A kept candidate is
    /// never locked.
    pub(crate) fn approach<H: Host>(
        &self,
        host: &H,
        found: &Found,
        mounts: &[MountEntry],
        deadline: Option<MonoTime>,
    ) -> Judged<Reached<H::Fd>> {
        if let Err(skip) = self.place(found, mounts) {
            return Ok(Err(skip));
        }
        let reached = judged!(reach(host, found, mounts, deadline));
        judged!(no_keep_markers(host, &reached, mounts, deadline));
        Ok(Ok(reached))
    }

    /// Steps 1 and 2 again on a candidate already reached (the reclaim,
    /// under its lock): its place under the mount table `mounts`, read
    /// again, and the keep markers on the very directories it was reached
    /// through.
    pub(crate) fn again<H: Host>(
        &self,
        host: &H,
        found: &Found,
        reached: &Reached<H::Fd>,
        mounts: &[MountEntry],
        deadline: Option<MonoTime>,
    ) -> Judged<()> {
        if let Err(skip) = self.place(found, mounts) {
            return Ok(Err(skip));
        }
        no_keep_markers(host, reached, mounts, deadline)
    }

    /// Steps 3 and 4, what the candidate holds: the source's own `where`
    /// and the eligibility walk of `candidate` (the object `found`, reached
    /// as `reached`), with the idle requirement when `idle` (the tally
    /// orders without it).
    pub(crate) fn contents<H: Host>(
        &self,
        host: &H,
        reached: &Reached<H::Fd>,
        candidate: &Candidate<'_, H::Fd>,
        found: &Found,
        idle: bool,
        deadline: Option<MonoTime>,
    ) -> Judged<Walked> {
        judged!(self.source_where(host, reached.parent(), found, deadline));
        self.walk(host, candidate, found, idle, deadline)
    }

    /// Step 1: what can be judged from the path, the identity found and the
    /// mount table, before anything is opened.
    fn place(&self, found: &Found, mounts: &[MountEntry]) -> Result<(), Skip> {
        if let Some(refused) =
            checkout_metadata_below(self.rules.checkouts, &found.root, &found.path)
        {
            return Err(refused);
        }
        if found.identity.id.dev != self.dev {
            return Err(Skip::new(
                Reason::OtherFs,
                found.path.clone(),
                format!(
                    "on device {}, not on PATH's filesystem (device {})",
                    found.identity.id.dev, self.dev
                ),
            ));
        }
        mounted_at_or_inside(mounts, &found.path).map_or(Ok(()), Err)
    }

    /// Step 3: the source's own `where` on the candidate (reached: its
    /// parent directory `parent`). It held when discovery found it, so a
    /// candidate that no longer passes it is `changed`.
    fn source_where<H: Host>(
        &self,
        host: &H,
        parent: &Resolved<H::Fd>,
        found: &Found,
        deadline: Option<MonoTime>,
    ) -> Judged<()> {
        let name = file_name(found)?;
        match source_where(
            host,
            self.find,
            parent,
            &found.path,
            name,
            Some(found.identity.file_type),
            deadline,
        )? {
            Ok(true) => Ok(Ok(())),
            Ok(false) => Ok(Err(Skip::new(
                Reason::Changed,
                found.path.clone(),
                "no longer passes its source profile's own `where` (has / has_not), which it \
                 passed when found",
            ))),
            Err(skip) => Ok(Err(skip)),
        }
    }

    /// Step 4: the eligibility walk of `candidate` (the object `found`,
    /// reached), its root matched against the identity found, and, when
    /// `idle` is asked for, the term's idle requirement on its newest
    /// change.
    fn walk<H: Host>(
        &self,
        host: &H,
        candidate: &Candidate<'_, H::Fd>,
        found: &Found,
        idle: bool,
        deadline: Option<MonoTime>,
    ) -> Judged<Walked> {
        let record = match engine::walk(host, candidate, self.rules, deadline) {
            Ok(record) => record,
            Err(WalkStop::Skipped(skip)) => return Ok(Err(skip)),
            Err(WalkStop::DeadlinePassed { path }) => return Err(Stop::DeadlinePassed { path }),
            Err(WalkStop::Failed { path, error }) => return Err(Stop::Failed { path, error }),
        };
        if let Err(skip) = same_object(found, Some(record.root().identity), "walked") {
            return Ok(Err(skip));
        }
        let newest_change = record.newest_change();
        if idle && let Err(skip) = judge_idle(host, &found.path, newest_change, self.idle_for) {
            return Ok(Err(skip));
        }
        Ok(Ok(Walked {
            record,
            newest_change,
        }))
    }
}

/// The idle requirement `idle_for` (none: anything passes) on a candidate
/// whose newest change is `newest`: an unknown age is never idle
/// (docs/reference.md, Predicates).
pub(crate) fn judge_idle<H: Host>(
    host: &H,
    at: &Path,
    newest: Option<Timestamp>,
    idle_for: Option<IdleFor>,
) -> Result<(), Skip> {
    let Some(idle_for) = idle_for else {
        return Ok(());
    };
    let now = host.wall_clock();
    match idle_for.verdict(newest, now) {
        Idleness::Idle => Ok(()),
        Idleness::NotIdle => {
            let last = newest.map_or(0, |t| t.secs);
            Err(Skip::new(
                Reason::NotIdle,
                at.to_owned(),
                format!(
                    "newest change at {last} s, {} s before now; idle_for {} s required",
                    now.secs - last,
                    idle_for.0.as_secs()
                ),
            ))
        }
        Idleness::Unknown(unknown) => {
            let reason = match unknown {
                Unknown::Unanswered => Reason::UnsupportedFs,
                Unknown::InFuture => Reason::FutureTime,
            };
            Err(Skip::new(
                reason,
                at.to_owned(),
                format!(
                    "its newest change cannot be told ({unknown:?}): an unknown age is never idle"
                ),
            ))
        }
    }
}

/// The candidate's own name (the last name of its path).
fn file_name(found: &Found) -> Result<&OsStr, Stop> {
    found.path.file_name().ok_or_else(|| Stop::Failed {
        path: found.path.clone(),
        error: HostError::invalid("judge", found.path.display(), "a candidate has no name"),
    })
}

/// A mount at `at` or anywhere below it, by `mountinfo` (`contains_mount`):
/// the candidate is never looked into (a dead FUSE mount would hang the
/// first `statx` that reaches it).
pub(super) fn mounted_at_or_inside(mounts: &[MountEntry], at: &Path) -> Option<Skip> {
    let mount = mounts.iter().find(|m| m.mount_point.starts_with(at))?;
    let (path, detail) = if mount.mount_point == at {
        (at.to_owned(), "a mount point (mountinfo): not touched")
    } else {
        (
            mount.mount_point.clone(),
            "a mount point inside (mountinfo): not looked into",
        )
    };
    Some(Skip::new(Reason::ContainsMount, path, detail))
}

/// Whether a root may be searched, judged on its path and its mount's
/// mountinfo line, before anything there is opened or `statx`ed: inside
/// systemd's PrivateTmp, by a name on its path or by its mount's root (a
/// bind mount of a `systemd-private-*` directory), `Err(Ok(why))`; on a
/// network filesystem, or with no clear mount, `Err(Err(skip))`.
#[allow(clippy::result_large_err)]
pub(super) fn check_root(root: &Path, mounts: &[MountEntry]) -> Result<(), Result<String, Skip>> {
    if path_in_private_tmp(root) {
        return Err(Ok(format!(
            "inside systemd's PrivateTmp ({PRIVATE_TMP_PREFIX}*)"
        )));
    }
    let mount = mount_for(mounts, root).map_err(|why| {
        Err(Skip::new(
            Reason::UnsafePath,
            root.to_owned(),
            format!("unsafe_path: {}: {why}", root.display()),
        ))
    })?;
    if may_be_networked(&mount.fs_type) {
        return Err(Err(Skip::new(
            Reason::UnsupportedFs,
            root.to_owned(),
            format!(
                "on {} ({}, mount {}): never a candidate's filesystem, and not opened (a \
                 dead server would hang it)",
                networked_why(&mount.fs_type),
                mount.fs_type,
                mount.mount_point.display()
            ),
        )));
    }
    if path_in_private_tmp(&mount.fs_root) {
        return Err(Ok(format!(
            "on mount {} whose root is {} in its filesystem: a bind mount of systemd's \
             PrivateTmp ({PRIVATE_TMP_PREFIX}*)",
            mount.mount_point.display(),
            mount.fs_root.display()
        )));
    }
    Ok(())
}

/// A root that cannot be reached: its skip reason (`unsafe_path`,
/// `missing`, `unsupported_fs`, `unreadable`), or an error of the run.
pub(super) fn unreachable_root(root: &Path, e: ResolveError) -> Result<Skip, Stop> {
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
pub(super) fn skip_or_stop(path: PathBuf, what: &str, e: &HostError) -> Result<Skip, Stop> {
    match engine::host_failure(path, what, e) {
        WalkStop::Skipped(skip) => Ok(skip),
        WalkStop::Failed { path, error } => Err(Stop::Failed { path, error }),
        WalkStop::DeadlinePassed { path } => Err(Stop::DeadlinePassed { path }),
    }
}

/// The source's own `has` / `has_not` on the entry `name` (at `at`, of
/// type `file_type`) in `dir`: a regular file of that name directly in it.
/// A file candidate holds nothing. A directory that cannot be read is
/// neither (`Err`: refused as `unreadable`, never taken). The deadline is
/// checked before each predicate's I/O.
pub(super) fn source_where<H: Host>(
    host: &H,
    find: &Find,
    dir: &Resolved<H::Fd>,
    at: &Path,
    name: &OsStr,
    file_type: Option<FileType>,
    deadline: Option<MonoTime>,
) -> Judged<bool> {
    let (has, has_not) = (&find.has, &find.has_not);
    if has.is_none() && has_not.is_none() {
        return Ok(Ok(true));
    }
    if file_type != Some(FileType::Directory) {
        return Ok(Ok(has.is_none()));
    }
    check_deadline(host, deadline, at)?;
    let fd = match open_beneath(
        host,
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
    let holds = |file: &str| -> Judged<bool> {
        check_deadline(host, deadline, &at.join(file))?;
        match host.statx(Base::Dir(&fd), Path::new(file)) {
            Ok(stat) => Ok(Ok(stat.file_type == Some(FileType::Regular))),
            Err(e) if e.is(Errno::ENOENT) => Ok(Ok(false)),
            Err(e) => Ok(Err(skip_or_stop(at.join(file), "stat", &e)?)),
        }
    };
    if let Some(file) = has
        && !judged!(holds(file))
    {
        return Ok(Ok(false));
    }
    if let Some(file) = has_not {
        return Ok(Ok(!judged!(holds(file))));
    }
    Ok(Ok(true))
}

/// A candidate reached again from its root ([`reach`]): the directories
/// from the root down to its parent, each held open (`O_PATH`), so that the
/// keep markers on the way can be judged again ([`no_keep_markers`]) on the
/// very directories the candidate was reached through.
#[derive(Debug)]
pub struct Reached<F> {
    /// (path, directory), the root first, the candidate's parent last.
    dirs: Vec<(PathBuf, Resolved<F>)>,
}

impl<F> Reached<F> {
    /// The candidate's parent directory.
    pub fn parent(&self) -> &Resolved<F> {
        &self.dirs.last().expect("the root at least").1
    }

    /// The candidate `found` as the engine reaches it: by its name in its
    /// parent directory, reached.
    pub fn candidate<'a>(&'a self, found: &'a Found) -> Result<Candidate<'a, F>, Stop> {
        Candidate::new(self.parent(), &found.path).ok_or_else(|| Stop::Failed {
            path: found.path.clone(),
            error: HostError::invalid("judge", found.path.display(), "a candidate has no name"),
        })
    }
}

/// The mount table, read now (an error of the run when it cannot be).
pub(crate) fn mountinfo<H: Host>(host: &H) -> Result<Vec<MountEntry>, Stop> {
    host.mountinfo().map_err(|error| Stop::Failed {
        path: PathBuf::from("/proc/self/mountinfo"),
        error,
    })
}

/// Whether `now` (an identity just read, `None`: not reported) is the
/// object discovery found (device, inode, owner, type, birth time): the
/// one comparison every step that reaches the candidate again makes.
/// `what` names the step for the message (`now`, `locked`, `walked`).
pub(crate) fn same_object(
    found: &Found,
    now: Option<engine::Identity>,
    what: &str,
) -> Result<(), Skip> {
    if now == Some(found.identity) {
        return Ok(());
    }
    Err(Skip::new(
        Reason::Changed,
        found.path.clone(),
        format!(
            "not the entry found (found {:?}, {what} {now:?})",
            found.identity
        ),
    ))
}

/// Step 2 (see the module docs): `found` reached again from its root under
/// the mount table `mounts`, one directory at a time, below the root's
/// mount only. A directory on the way that is a mount point now is
/// `contains_mount` (never opened), one gone `missing`, one no longer
/// reachable as found (a symlink now) `changed`; the candidate itself gone
/// is `missing`, and not the object found `changed` ([`same_object`]).
/// The keep markers on the way are [`no_keep_markers`]'s.
pub(crate) fn reach<H: Host>(
    host: &H,
    found: &Found,
    mounts: &[MountEntry],
    deadline: Option<MonoTime>,
) -> Judged<Reached<H::Fd>> {
    let root = &found.root;
    check_deadline(host, deadline, root)?;
    match check_root(root, mounts) {
        Ok(()) => {}
        Err(Ok(why)) => return Ok(Err(super::discover::private_root(root, &why))),
        Err(Err(skip)) => return Ok(Err(skip)),
    }
    let top = match resolve_in(host, mounts, root, Want::Directory) {
        Ok(dir) => dir,
        Err(e) => return Ok(Err(unreachable_root(root, e)?)),
    };
    let rel = found.path.strip_prefix(root).map_err(|_| Stop::Failed {
        path: found.path.clone(),
        error: HostError::invalid(
            "reach",
            found.path.display(),
            format!("not below its root {}", root.display()),
        ),
    })?;
    let names: Vec<&OsStr> = rel
        .components()
        .map(|c| match c {
            Component::Normal(name) => Ok(name),
            other => Err(Stop::Failed {
                path: found.path.clone(),
                error: HostError::invalid(
                    "reach",
                    found.path.display(),
                    format!("component {other:?} below the root is not a name"),
                ),
            }),
        })
        .collect::<Result<_, _>>()?;
    let Some((name, on_the_way)) = names.split_last() else {
        return Err(Stop::Failed {
            path: found.path.clone(),
            error: HostError::invalid("reach", found.path.display(), "a candidate is its root"),
        });
    };
    let mut dirs = vec![(root.clone(), top)];
    for step in on_the_way {
        let (at, dir) = dirs.last().expect("the root at least");
        let at = at.join(step);
        if mounts.iter().any(|m| m.mount_point == at) {
            return Ok(Err(Skip::new(
                Reason::ContainsMount,
                at,
                "a mount point on the way to the candidate (mountinfo): not entered",
            )));
        }
        check_deadline(host, deadline, &at)?;
        let (fd, stat) = match open_beneath(
            host,
            &dir.fd,
            dir.mount.id,
            Path::new(step),
            Access::Path,
            Want::Directory,
        ) {
            Ok(opened) => opened,
            Err(ResolveError::Host(e)) => return Ok(Err(skip_or_stop(at, "open", &e)?)),
            Err(ResolveError::Missing { .. }) => {
                return Ok(Err(Skip::new(Reason::Missing, at, "gone since discovery")));
            }
            Err(e) => {
                return Ok(Err(Skip::new(
                    Reason::Changed,
                    at,
                    format!("not reachable as found: {e}"),
                )));
            }
        };
        let mount = dir.mount.clone();
        dirs.push((at, Resolved { fd, mount, stat }));
    }
    let parent = &dirs.last().expect("the root at least").1;
    check_deadline(host, deadline, &found.path)?;
    let stat = match host.statx(Base::Dir(&parent.fd), Path::new(name)) {
        Ok(stat) => stat,
        Err(e) if e.is(Errno::ENOENT) => {
            return Ok(Err(Skip::new(
                Reason::Missing,
                found.path.clone(),
                "gone since discovery",
            )));
        }
        Err(e) => return Ok(Err(skip_or_stop(found.path.clone(), "stat", &e)?)),
    };
    if let Err(skip) = same_object(found, engine::Identity::of(&stat), "now") {
        return Ok(Err(skip));
    }
    Ok(Ok(Reached { dirs }))
}

/// No keep marker in the root or any directory on the way to the
/// candidate (`kept`), judged on the directories [`reach`] went through: a
/// `statx` of the name in each, not followed, whatever it is (a marker is
/// a name, never a listing), and a mount point of that name counts without
/// being touched.
pub(crate) fn no_keep_markers<H: Host>(
    host: &H,
    reached: &Reached<H::Fd>,
    mounts: &[MountEntry],
    deadline: Option<MonoTime>,
) -> Judged<()> {
    for (path, dir) in &reached.dirs {
        let marker = path.join(KEEP_MARKER);
        let kept = || {
            Ok(Err(Skip::new(
                Reason::Kept,
                marker.clone(),
                "a keep marker between the root and the candidate",
            )))
        };
        if mounts.iter().any(|m| m.mount_point == marker) {
            return kept();
        }
        check_deadline(host, deadline, &marker)?;
        match host.statx(Base::Dir(&dir.fd), Path::new(KEEP_MARKER)) {
            Ok(_) => return kept(),
            Err(e) if e.is(Errno::ENOENT) => {}
            Err(e) => return Ok(Err(skip_or_stop(marker.clone(), "stat", &e)?)),
        }
    }
    Ok(Ok(()))
}
