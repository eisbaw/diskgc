//! Eligibility of one found target (docs/safety.md, `cargo-target`:
//! Discovery, idleness, eligibility; In use), and the parts the reclaim
//! protocol reuses: [`judge_layout`], [`walk_profiles`], [`last_build`].

use std::path::{Path, PathBuf};

use super::discover::{check_deadline, open_dir};
use super::{
    Inspected, Judged, LockSet, ProfileDir, Project, Stop, Target, cargo_evidence, inspect, reach,
};
use crate::engine::{
    self, CARGO_LOCK_NAMES, Candidate, Identity, Reason, Rules, Skip, WalkRecord, WalkStop,
};
use crate::host::{FileType, Host, MonoTime, Timestamp};
use crate::inuse::{Checker, Scan};
use crate::profile::{IdleFor, Idleness, Unknown};
use crate::resolve::Resolved;

/// A target that passed every check at the time of the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eligible {
    /// Its last build ([`last_build`]); `None` only when no idle
    /// requirement asked and the kernel did not report a time.
    pub last_build: Option<Timestamp>,
    /// Entries the walks checked (each profile directory's entries and
    /// everything below them; lock files and the directories themselves
    /// not counted).
    pub entries: usize,
    /// Their allocated bytes: an estimate of what reclaiming frees.
    pub bytes: u64,
}

/// One profile directory's entries, each walked as a candidate of its own
/// (the reclaim protocol deletes them one by one, never the directory or
/// its lock files).
#[derive(Debug)]
pub struct ProfileWalk<F> {
    /// The profile directory, opened below the target and checked to be the
    /// one inspected.
    pub dir: Resolved<F>,
    /// Its path.
    pub path: PathBuf,
    /// Every entry but the lock files: its path and its walk record.
    pub entries: Vec<(PathBuf, WalkRecord)>,
}

impl<F> ProfileWalk<F> {
    /// Each entry as the engine's candidate, with its record.
    pub fn candidates(&self) -> impl Iterator<Item = (Candidate<'_, F>, &WalkRecord)> {
        self.entries.iter().map(|(path, record)| {
            let candidate =
                Candidate::new(&self.dir, path).expect("an entry's path ends in its name");
            (candidate, record)
        })
    }
}

/// Whether diskgc knows how to reclaim the target `inspected` (at `path`),
/// in this order: every lock file is a regular file (`statx`, not
/// followed: a directory, symlink, FIFO, socket or device with a lock-like
/// name is no Cargo's, `unsupported_layout`); every lock file name is
/// known (an unknown one is a Cargo whose locks were not measured:
/// `unsupported_layout`); every profile
/// directory holds a whole lock set ([`LockSet::Whole`]; a half of a split
/// build directory, or an unmeasured combination, is `unsupported_layout`,
/// since both halves of a pair would have to be locked together and the
/// other half is not known); some profile directory holds more than lock
/// files (otherwise `empty`); and every such one is in a known layout
/// ([`super::Layout`]; neither `.fingerprint/`, `deps/` nor `build/` is
/// `unsupported_layout`: the protocol would not know what to delete first).
pub fn judge_layout(path: &Path, inspected: &Inspected) -> Result<(), Skip> {
    let unsupported =
        |at: PathBuf, detail: String| Err(Skip::new(Reason::UnsupportedLayout, at, detail));
    for profile in &inspected.profiles {
        if let Some(lock) = profile
            .locks
            .iter()
            .find(|l| l.identity.file_type != FileType::Regular)
        {
            return unsupported(
                path.join(&profile.rel).join(&lock.name),
                format!(
                    "a Cargo lock file name on a {:?}, not a regular file: no measured Cargo \
                     makes that",
                    lock.identity.file_type
                ),
            );
        }
        if let Some(lock) = profile.unknown_locks().next() {
            return unsupported(
                path.join(&profile.rel).join(&lock.name),
                format!(
                    "an unknown Cargo lock file name (known: {CARGO_LOCK_NAMES:?}): a Cargo \
                     whose locks diskgc has not measured"
                ),
            );
        }
        let set = profile.lock_set();
        if set != LockSet::Whole {
            return unsupported(path.join(&profile.rel), set.describe().to_owned());
        }
    }
    if inspected.profiles.iter().all(|p| p.is_empty()) {
        return Err(Skip::new(
            Reason::Empty,
            path.to_owned(),
            "its profile directories hold nothing but lock files",
        ));
    }
    for profile in inspected.profiles.iter().filter(|p| !p.is_empty()) {
        if profile.layout().is_none() {
            return unsupported(
                path.join(&profile.rel),
                "neither .fingerprint/, deps/ nor build/ in the profile directory: an unknown \
                 layout"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// What a new inspection of `target` (`inspected`) must show before its
/// entries are walked: the same profile directories and lock files as
/// when found (one created, removed or replaced since is `changed`), no
/// keep marker in the target or a directory directly in it (`kept`),
/// ownership of what the kind relies on ([`judge_owners`]) and a layout
/// diskgc knows how to reclaim ([`judge_layout`]).
pub(super) fn judge_inspected(
    target: &Target,
    inspected: &Inspected,
    owners: &[u32],
) -> Result<(), Skip> {
    let path = &target.path;
    if !inspected.same_locks(&target.inspected) {
        return Err(Skip::new(
            Reason::Changed,
            path.clone(),
            "a profile directory or lock file was created, removed or replaced since discovery",
        ));
    }
    if let Some(marker) = &inspected.keep_marker {
        return Err(Skip::new(
            Reason::Kept,
            path.join(marker),
            "a keep marker in the target",
        ));
    }
    judge_owners(path, &target.identity, inspected, owners)?;
    judge_layout(path, inspected)
}

/// Every object the kind relies on but does not delete must be the user's
/// too: the target directory, the directories holding profile directories
/// two levels down, the profile directories and their lock files
/// (`not_owner`). What it deletes is checked by the walks.
fn judge_owners(
    path: &Path,
    identity: &Identity,
    inspected: &Inspected,
    owners: &[u32],
) -> Result<(), Skip> {
    let mut objects: Vec<(PathBuf, &Identity)> = vec![(path.to_owned(), identity)];
    for profile in &inspected.profiles {
        if let Some(parent) = profile.rel.parent().filter(|p| !p.as_os_str().is_empty())
            && let Some(dir) = inspected.dirs.iter().find(|d| d.rel == parent)
        {
            objects.push((path.join(&dir.rel), &dir.identity));
        }
        objects.push((path.join(&profile.rel), &profile.identity));
        for lock in &profile.locks {
            objects.push((path.join(&profile.rel).join(&lock.name), &lock.identity));
        }
    }
    match objects
        .into_iter()
        .find(|(_, id)| !owners.contains(&id.uid))
    {
        None => Ok(()),
        Some((at, id)) => Err(Skip::new(
            Reason::NotOwner,
            at,
            format!("owned by uid {}, not by {owners:?}", id.uid),
        )),
    }
}

/// Walks every entry of every profile directory of `inspected` but the
/// lock files, each with the deletion engine's eligibility walk
/// ([`engine::walk`], `rules`): ownership of every entry, keep markers,
/// mounts, sockets and FIFOs, Cargo lock names and nested targets, the
/// entry budget, which is one for the whole target (`rules.max_entries`
/// over all its entries, not per entry). The first refusal is the
/// target's. `target` is the target directory reached at `path`; each
/// profile directory is opened below it and must be the one inspected
/// (`changed` otherwise).
pub fn walk_profiles<H: Host>(
    host: &H,
    target: &Resolved<H::Fd>,
    path: &Path,
    inspected: &Inspected,
    rules: &Rules<'_>,
    deadline: Option<MonoTime>,
) -> Judged<Vec<ProfileWalk<H::Fd>>> {
    let mut walks = Vec::new();
    let mut budget = Budget::new(rules.max_entries);
    for profile in inspected.profiles.iter().filter(|p| !p.is_empty()) {
        let at = path.join(&profile.rel);
        let dir = judged!(open_profile(host, target, &at, profile, deadline));
        let entries = judged!(walk_entries(
            host,
            &dir,
            &at,
            profile,
            rules,
            &mut budget,
            deadline
        ));
        walks.push(ProfileWalk {
            dir,
            path: at,
            entries,
        });
    }
    Ok(Ok(walks))
}

/// Opens the profile directory `profile` (at `path`) below the target
/// directory `target`; it must be the one inspected (`changed`
/// otherwise).
pub(super) fn open_profile<H: Host>(
    host: &H,
    target: &Resolved<H::Fd>,
    path: &Path,
    profile: &ProfileDir,
    deadline: Option<MonoTime>,
) -> Judged<Resolved<H::Fd>> {
    check_deadline(host, deadline, path)?;
    let (fd, stat) = judged!(open_dir(
        host,
        &target.fd,
        target.mount.id,
        &profile.rel,
        path
    ));
    if Identity::of(&stat) != Some(profile.identity) {
        return Ok(Err(Skip::new(
            Reason::Changed,
            path.to_owned(),
            "not the profile directory inspected (replaced in between)",
        )));
    }
    Ok(Ok(Resolved {
        fd,
        mount: target.mount.clone(),
        stat,
    }))
}

/// What is left of a target's entry budget ([`Rules::max_entries`]): one
/// for all the entries of all its profile directories, so a target cannot
/// hold more records in memory at once than one candidate may (task-9
/// review, H4).
#[derive(Debug)]
pub(super) struct Budget {
    max: usize,
    used: usize,
}

impl Budget {
    pub(super) fn new(max: usize) -> Self {
        Self { max, used: 0 }
    }

    /// The skip of a target whose entries exceed the budget, at `path`.
    fn exceeded(&self, path: PathBuf) -> Skip {
        Skip::new(
            Reason::TooManyEntries,
            path,
            format!(
                "the target's profile directories hold more than {} entries (one budget for \
                 the whole target)",
                self.max
            ),
        )
    }
}

/// Walks every entry of the profile directory `profile`, open as `dir` at
/// `path`, but its lock files, each as a candidate of its own, within
/// what is left of `budget` (see [`walk_profiles`]).
pub(super) fn walk_entries<H: Host>(
    host: &H,
    dir: &Resolved<H::Fd>,
    path: &Path,
    profile: &ProfileDir,
    rules: &Rules<'_>,
    budget: &mut Budget,
    deadline: Option<MonoTime>,
) -> Judged<Vec<(PathBuf, WalkRecord)>> {
    let mut entries = Vec::new();
    for inspected_entry in &profile.entries {
        let entry = path.join(&inspected_entry.name);
        let candidate = Candidate::new(dir, &entry).expect("an entry's path ends in its name");
        let left = Rules {
            max_entries: budget.max.saturating_sub(budget.used),
            ..*rules
        };
        match engine::walk(host, &candidate, &left, deadline) {
            // The layout was judged by the types inspected: an entry of
            // another type now (a directory replaced by a file or a
            // symlink) is a change.
            Ok(record) if record.root().identity.file_type != inspected_entry.file_type => {
                return Ok(Err(Skip::new(
                    Reason::Changed,
                    entry,
                    "not of the type inspected (replaced in between)",
                )));
            }
            // The walk bounds its listings by what is left; an entry that
            // is a file needs no listing, so the sum is checked here too.
            Ok(record) if budget.used + record.entries() > budget.max => {
                return Ok(Err(budget.exceeded(entry)));
            }
            Ok(record) => {
                budget.used += record.entries();
                entries.push((entry, record));
            }
            Err(WalkStop::Skipped(skip)) if skip.reason == Reason::TooManyEntries => {
                return Ok(Err(budget.exceeded(skip.path)));
            }
            Err(WalkStop::Skipped(skip)) => return Ok(Err(skip)),
            Err(WalkStop::DeadlinePassed { path }) => {
                return Err(Stop::DeadlinePassed { path });
            }
            Err(WalkStop::Failed { path, error }) => return Err(Stop::Failed { path, error }),
        }
    }
    Ok(Ok(entries))
}

/// The last build (docs/safety.md, `cargo-target`: Last build): the newest
/// mtime or ctime of the profile directories, their lock files and
/// everything the walks recorded in them. The same measure on every
/// layout: it reads no layout-specific directory, so a layout that keeps
/// its fingerprints elsewhere changes nothing about it. `None` when any of
/// those times was not reported: the age is then unknown, never assumed.
pub fn last_build<F>(inspected: &Inspected, walks: &[ProfileWalk<F>]) -> Option<Timestamp> {
    let mut newest: Option<Timestamp> = None;
    for profile in &inspected.profiles {
        newest = newest.max(Some(profile.changed?));
        for lock in &profile.locks {
            newest = newest.max(Some(lock.changed?));
        }
    }
    for walk in walks {
        for (_, record) in &walk.entries {
            newest = newest.max(Some(record.newest_change()?));
        }
    }
    newest
}

/// Judges `target` (found by [`super::discover`]) now, without locking or
/// deleting anything, for a plan or a survey: reached again from its root
/// ([`reach`]: `kept`, `changed`, `missing`, `unsupported_fs`), inspected
/// again (a profile or lock file other than when found is `changed`), the
/// keep marker in it, ownership, [`judge_layout`], [`walk_profiles`] with
/// `rules`, the idle requirement on [`last_build`] (an unknown age passes
/// no idle filter: `unsupported_fs` or `future_time`), then the in-use
/// check: a scan of every visible process (from `checker`, reused while
/// young), against which every entry's record is checked
/// ([`crate::inuse::Scan::check`]) and the Cargo heuristics run
/// ([`cargo_evidence`]). An in-use target names every piece of evidence,
/// `; `-separated.
pub fn eligibility<H: Host>(
    host: &H,
    target: &Target,
    rules: &Rules<'_>,
    idle_for: Option<IdleFor>,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
) -> Judged<Eligible> {
    let path = &target.path;
    let reached = judged!(reach(host, target, deadline));
    let inspected = judged!(inspect(
        host,
        &reached.target.fd,
        reached.target.mount.id,
        path,
        deadline
    ));
    if let Err(skip) = judge_inspected(target, &inspected, rules.owners) {
        return Ok(Err(skip));
    }
    let walks = judged!(walk_profiles(
        host,
        &reached.target,
        path,
        &inspected,
        rules,
        deadline
    ));
    let last = last_build(&inspected, &walks);
    if let Err(skip) = judge_idle(host, path, last, idle_for) {
        return Ok(Err(skip));
    }
    let scan = checker.current(host).map_err(|error| Stop::Failed {
        path: path.clone(),
        error,
    })?;
    if let Err(skip) = judge_in_use(scan, &walks, target, &inspected, reached.project) {
        return Ok(Err(skip));
    }
    let (entries, bytes) = walks
        .iter()
        .flat_map(|w| &w.entries)
        .fold((0, 0u64), |(n, b), (_, r)| {
            (n + r.entries(), b.saturating_add(r.bytes()))
        });
    Ok(Ok(Eligible {
        last_build: last,
        entries,
        bytes,
    }))
}

/// The idle requirement on the last build `last` of the target at `path`
/// (none: anything passes): `not_idle`, or, for an unknown age, which is
/// never idle, `unsupported_fs` (a time not reported) or `future_time`.
pub(super) fn judge_idle<H: Host>(
    host: &H,
    path: &Path,
    last: Option<Timestamp>,
    idle_for: Option<IdleFor>,
) -> Result<(), Skip> {
    let Some(idle_for) = idle_for else {
        return Ok(());
    };
    let now = host.wall_clock();
    let skip = |reason, detail: String| Err(Skip::new(reason, path.to_owned(), detail));
    match idle_for.verdict(last, now) {
        Idleness::Idle => Ok(()),
        Idleness::NotIdle => skip(
            Reason::NotIdle,
            format!(
                "last build at {} s, {} s before now; idle_for {} s required",
                last.map_or(0, |t| t.secs),
                now.secs - last.map_or(0, |t| t.secs),
                idle_for.0.as_secs()
            ),
        ),
        Idleness::Unknown(Unknown::Unanswered) => skip(
            Reason::UnsupportedFs,
            "its last build cannot be told: the kernel did not report a time, and an unknown \
             age is never idle"
                .to_owned(),
        ),
        Idleness::Unknown(Unknown::InFuture) => skip(
            Reason::FutureTime,
            format!(
                "its last build at {} s lies after now ({} s): its age says nothing",
                last.map_or(0, |t| t.secs),
                now.secs
            ),
        ),
    }
}

/// The in-use check of the whole target against `scan`: a scan that
/// cannot clear anything (unsettled, a blocking unreadable process), every
/// entry's record ([`crate::inuse::Scan::check`]) and the Cargo heuristics
/// ([`cargo_evidence`]). An in-use target names every piece of evidence,
/// `; `-separated; another refusal of a record (a bound socket:
/// `contains_ipc`) is returned as it is.
pub(super) fn judge_in_use<F>(
    scan: &Scan,
    walks: &[ProfileWalk<F>],
    target: &Target,
    inspected: &Inspected,
    project: Project,
) -> Result<(), Skip> {
    let path = &target.path;
    if let Some(why) = scan.blocked() {
        return Err(Skip::new(Reason::InUse, path.clone(), why));
    }
    let mut evidence = Vec::new();
    for walk in walks {
        for (candidate, record) in walk.candidates() {
            match scan.check(&candidate, record) {
                Ok(()) => {}
                Err(skip) if skip.reason == Reason::InUse => {
                    evidence.push(format!("{}: {}", skip.path.display(), skip.detail));
                }
                Err(skip) => return Err(skip),
            }
        }
    }
    evidence.extend(cargo_evidence(
        scan,
        path,
        target.identity.id,
        inspected,
        project,
    ));
    if evidence.is_empty() {
        Ok(())
    } else {
        Err(Skip::new(Reason::InUse, path.clone(), evidence.join("; ")))
    }
}
