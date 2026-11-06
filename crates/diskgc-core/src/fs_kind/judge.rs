//! The judgement of one `fs` candidate that discovery found
//! ([`super::Found`]): the one set of checks that the dry run
//! ([`super::plan`]), the tally and the reclaim ([`super::reclaim`]) apply,
//! so that a plan predicts what a reclaim does (docs/safety.md, Filesystem
//! candidates). The sequence has two halves, so the reclaim can put the
//! candidate's `flock` between them and repeat the first under it:
//! [`Judge::approach`] (steps 1 and 2; again under the lock:
//! [`Judge::again`], which first shows that the path still leads to what
//! was reached, [`still_reached`], so the path rules judge where the
//! candidate is now) and [`Judge::contents`] (steps 3 and 4):
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
//!    in the root or any directory on the way (`kept`); then
//!    [`outside_container_storage`]: the root or a directory on the way is
//!    a container storage root (`contains_special`);
//! 3. [`Judge::source_where`]: the source's own entry predicates again; a
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
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use super::discover::{Found, check_deadline};
use super::pattern::path_matches;
use super::source_anchor::{self, SourceEvidence};
use super::{Find, PRIVATE_TMP_PREFIX, path_in_private_tmp};
use crate::engine::container_storage::Above;
use crate::engine::{
    self, Candidate, KEEP_MARKER, Reason, Rules, Skip, WalkRecord, WalkStop,
    checkout_metadata_below, container_storage, networked_why,
};
use crate::host::{
    Access, Base, DirListing, Errno, FileType, Host, HostError, MonoTime, MountEntry, Timestamp,
    is_single_name,
};
use crate::outcome::{Judged, Stop};
use crate::places;
use crate::profile::{IdleWindow, Idleness, NewestGroup, Unknown, age_secs};
use crate::resolve::{
    MountScreenError, ResolveError, Resolved, Want, is_mount_point, name_present, open_beneath,
    resolve_in,
};
use crate::units::{format_age, format_duration};

/// What a candidate of one tier is judged by: the source's `find` (its own
/// `where`), the walk's rules (owners, IPC policy, entry budget, checkout
/// setting), the term's idle requirement, and the space of `PATH`'s
/// filesystem ([`crate::places::space_of`]).
#[derive(Debug, Clone, Copy)]
pub struct Judge<'r> {
    pub find: &'r Find,
    pub rules: &'r Rules<'r>,
    pub idle: IdleWindow,
    pub space: crate::places::Space,
}

/// What a walked candidate showed: the walk's record and its newest
/// change (the `fs` idle measure; `None` when some time was not reported).
#[derive(Debug)]
pub struct Walked {
    pub record: WalkRecord,
    pub newest_change: Option<Timestamp>,
    /// Whether the record holds a checkout marker the judged source does
    /// not tolerate ([`WalkRecord::contains_checkout`] with its `find`'s
    /// `empty_checkout_markers`): what a union's refusing term refuses.
    pub contains_checkout: bool,
    /// The build source checked under this judgement, if required.
    pub source_evidence: Option<SourceEvidence>,
}

/// What a judgement measured of a candidate: the tier's order key and its
/// size (the tally's result, and an eligible plan entry's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tally {
    /// The newest mtime or ctime in its tree; `None` when some time was not
    /// reported (ordered last, never idle).
    pub newest_change: Option<Timestamp>,
    /// Entries the walk checked, the candidate included.
    pub entries: usize,
    /// Their allocated bytes, each object once ([`Walked::bytes`]).
    pub bytes: u64,
    /// Checkout markers in the checked tree, used when a union combines
    /// only the policies of terms that actually admitted this candidate.
    pub contains_checkout: bool,
    pub source_evidence: Option<SourceEvidence>,
}

/// The reached candidate and, during reclaim, its locked directory fd.
/// Source checks must read through the locked fd when one exists.
pub(crate) struct CandidateView<'a, F> {
    pub candidate: &'a Candidate<'a, F>,
    pub locked_fd: Option<&'a F>,
}

impl Walked {
    /// Its order key and size.
    pub fn tally(&self) -> Tally {
        Tally {
            newest_change: self.newest_change,
            entries: self.record.entries(),
            bytes: self.bytes(),
            contains_checkout: self.contains_checkout,
            source_evidence: self.source_evidence.clone(),
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
    /// Check only the positive retention rules of this source. A union
    /// checks every source that discovered the inode, even when that term's
    /// evidence, idle filter, negation or checkout policy does not admit it.
    /// The path is reached afresh so an unclassifiable guard retains data.
    pub(crate) fn retention_guard<H: Host>(
        &self,
        host: &H,
        found: &Found,
        mounts: &[MountEntry],
        deadline: Option<MonoTime>,
    ) -> Judged<()> {
        if !self.find.has_retention_guard() {
            return Ok(Ok(()));
        }
        let reached = judged!(self.approach(host, found, mounts, deadline));
        self.version_where(host, reached.parent(), found, deadline)
    }

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
        judged!(outside_container_storage(
            host, &reached, found, mounts, deadline
        ));
        Ok(Ok(reached))
    }

    /// Steps 1 and 2 again on a candidate already reached (the reclaim,
    /// under its lock, and once more right before it acts): the path must
    /// still lead to the very directories it was reached through and to the
    /// object found ([`still_reached`], `changed` otherwise), so that its
    /// place under the mount table `mounts`, read again, and the keep
    /// markers on those directories are judged on where it is now.
    pub(crate) fn again<H: Host>(
        &self,
        host: &H,
        found: &Found,
        reached: &Reached<H::Fd>,
        mounts: &[MountEntry],
        deadline: Option<MonoTime>,
    ) -> Judged<()> {
        judged!(still_reached(host, found, reached, mounts, deadline));
        if let Err(skip) = self.place(found, mounts) {
            return Ok(Err(skip));
        }
        Ok(Ok(()))
    }

    /// Steps 3 and 4, what the candidate holds: the source's own `where`
    /// and the eligibility walk of `candidate` (the object `found`, reached
    /// as `reached`), with the idle requirement when `idle`. Single-source
    /// ordering tallies omit idle; union pre-queue tallies include it so an
    /// ineligible term cannot impose its checkout policy.
    pub(crate) fn contents<H: Host>(
        &self,
        host: &H,
        reached: &Reached<H::Fd>,
        candidate_view: CandidateView<'_, H::Fd>,
        found: &Found,
        idle: bool,
        deadline: Option<MonoTime>,
    ) -> Judged<Walked> {
        judged!(self.source_where(host, reached.parent(), found, deadline));
        judged!(self.entry_filters(
            host,
            reached.parent(),
            candidate_view.locked_fd,
            found,
            deadline
        ));
        let source_evidence = judged!(self.source_anchor(
            host,
            reached.parent(),
            candidate_view.locked_fd,
            found,
            deadline
        ));
        if let Some(lock) = &self.find.app_not_running {
            judged!(super::app::check(host, &found.root, lock, deadline));
        }
        if let Err(skip) = super::liveness::name_filter(self.find, found) {
            return Ok(Err(skip));
        }
        // A project contract's instance check (a manifest beside it, no
        // dependency directory around it): this source's own, so a
        // non-instance is never its to delete, also in a union.
        if let Some(contract) = self.find.liveness_contract.filter(|c| c.is_project()) {
            judged!(
                super::project::locate(host, contract, found, &reached.parent().fd, deadline)
                    .map(|located| located.map(drop))
            );
        }
        judged!(self.version_where(host, reached.parent(), found, deadline));
        let mut walked = judged!(self.walk(host, candidate_view.candidate, found, idle, deadline));
        if let Err(skip) = self.negated_mixed_filters(
            host,
            reached.parent(),
            candidate_view.locked_fd,
            found,
            walked.newest_change,
            deadline,
        )? {
            return Ok(Err(skip.measured(walked.bytes())));
        }
        walked.source_evidence = source_evidence;
        Ok(Ok(walked))
    }

    /// A sibling rank and configured current links are read afresh for each
    /// judgement, including the reclaim's locked judgement. Uncertain rank
    /// or link state retains the candidate.
    fn version_where<H: Host>(
        &self,
        host: &H,
        parent: &Resolved<H::Fd>,
        found: &Found,
        deadline: Option<MonoTime>,
    ) -> Judged<()> {
        for link in &self.find.protect_links {
            judged!(protect_link(
                host,
                found,
                &found.root.join(link),
                true,
                deadline
            ));
        }
        for dir in &self.find.protect_link_dirs {
            judged!(protect_link_dir_pattern(host, found, dir, deadline));
        }
        if let Some(name) = &self.find.protect_parent_link {
            let Some(parent) = found.path.parent().and_then(Path::parent) else {
                return Ok(Err(Skip::new(
                    Reason::LinkedCurrent,
                    found.path.clone(),
                    "candidate has no version parent",
                )));
            };
            judged!(protect_link(
                host,
                found,
                &parent.join(name),
                true,
                deadline
            ));
        }
        if let Some(manifest) = &self.find.protect_cursor_extensions {
            judged!(protect_cursor_extension(
                host,
                found,
                &found.root.join(manifest),
                deadline
            ));
        }
        if let Some(manager) = self.find.protect_toolchains {
            judged!(super::toolchain::protect(
                host,
                found,
                manager,
                &self.find.bound_roots,
                deadline
            ));
        }
        if let Some(count) = self.find.keep_newest {
            judged!(keep_newest(host, parent, found, self.find, count, deadline));
        }
        Ok(Ok(()))
    }

    /// Step 1: what can be judged from the path, the identity found and the
    /// mount table, before anything is opened.
    fn place(&self, found: &Found, mounts: &[MountEntry]) -> Result<(), Skip> {
        if let Some(refused) =
            checkout_metadata_below(self.rules.checkouts, &found.root, &found.path)
        {
            return Err(refused);
        }
        if !found.space.same_as(&self.space) {
            return Err(Skip::new(
                Reason::OtherFs,
                found.path.clone(),
                format!(
                    "on device {}, not on PATH's filesystem (device {})",
                    found.space, self.space
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
                "no longer passes its source profile's own `where` (has / has_dir / has_not / lacks_entry), which it \
                 passed when found",
            ))),
            Err(skip) => Ok(Err(skip)),
        }
    }

    /// Evaluate each selection entry predicate on the candidate, not at
    /// source discovery. A failed observation is never treated as the
    /// complement of a filter. The reclaim uses its locked directory fd.
    fn entry_filters<H: Host>(
        &self,
        host: &H,
        parent: &Resolved<H::Fd>,
        locked_fd: Option<&H::Fd>,
        found: &Found,
        deadline: Option<MonoTime>,
    ) -> Judged<()> {
        if self.find.entry_filters.is_empty() {
            return Ok(Ok(()));
        }
        // A file has no child entries. These are known predicate verdicts,
        // matching the source `has` / `has_not` semantics.
        let opened = judged!(open_selection_dir(host, parent, locked_fd, found, deadline));
        let candidate_fd = if found.identity.file_type == FileType::Directory {
            locked_fd.or(opened.as_ref())
        } else {
            None
        };
        for filter in &self.find.entry_filters {
            let regular = judged!(selection_regular_child(
                host,
                candidate_fd,
                found,
                &filter.entry,
                &filter.name,
                deadline
            ));
            let matches = if filter.has { regular } else { !regular };
            if matches == filter.negated {
                return Ok(Err(Skip::new(
                    Reason::Filtered,
                    found.path.clone(),
                    format!(
                        "selection filter {}filter:{} does not admit candidate",
                        if filter.negated { "!" } else { "" },
                        filter.name
                    ),
                )));
            }
        }
        Ok(Ok(()))
    }

    /// Complement each complete filter conjunction. Every predicate must
    /// have a known verdict before a mismatch can admit the candidate.
    /// This also runs in the single-source ordering tally: unlike the tier's
    /// positive idle bound, a mixed complement defines membership itself.
    fn negated_mixed_filters<H: Host>(
        &self,
        host: &H,
        parent: &Resolved<H::Fd>,
        locked_fd: Option<&H::Fd>,
        found: &Found,
        newest: Option<Timestamp>,
        deadline: Option<MonoTime>,
    ) -> Judged<()> {
        if self.find.negated_mixed_filters.is_empty() {
            return Ok(Ok(()));
        }
        let opened = judged!(open_selection_dir(host, parent, locked_fd, found, deadline));
        let candidate_fd = if found.identity.file_type == FileType::Directory {
            locked_fd.or(opened.as_ref())
        } else {
            None
        };
        for filter in &self.find.negated_mixed_filters {
            let mut matches = true;
            for (entry, requires_regular) in [(&filter.has, true), (&filter.has_not, false)] {
                let Some(entry) = entry else { continue };
                let regular = judged!(selection_regular_child(
                    host,
                    candidate_fd,
                    found,
                    entry,
                    &filter.name,
                    deadline
                ));
                matches &= regular == requires_regular;
            }
            if let Some(bound) = filter.idle_for {
                match bound.verdict(newest, host.wall_clock()) {
                    Idleness::Idle => {}
                    Idleness::NotIdle => matches = false,
                    Idleness::Unknown(unknown) => {
                        let reason = match unknown {
                            Unknown::Unanswered => Reason::UnsupportedFs,
                            Unknown::InFuture => Reason::FutureTime,
                        };
                        return Ok(Err(Skip::new(
                            reason,
                            found.path.clone(),
                            format!(
                                "!filter:{}: unknown idle age ({unknown}) passes neither polarity",
                                filter.name
                            ),
                        )));
                    }
                }
            }
            if matches {
                return Ok(Err(Skip::new(
                    Reason::Filtered,
                    found.path.clone(),
                    format!("selection filter !filter:{} matches candidate", filter.name),
                )));
            }
        }
        Ok(Ok(()))
    }

    fn source_anchor<H: Host>(
        &self,
        host: &H,
        parent: &Resolved<H::Fd>,
        opened_candidate: Option<&H::Fd>,
        found: &Found,
        deadline: Option<MonoTime>,
    ) -> Judged<Option<SourceEvidence>> {
        let Some(kind) = self.find.source_anchor else {
            return Ok(Ok(None));
        };
        let name = file_name(found)?;
        let evidence = if let Some(fd) = opened_candidate {
            judged!(source_anchor::check_opened(
                host,
                source_anchor::AnchorCheck::new(
                    kind,
                    &found.root,
                    &found.path,
                    found.identity,
                    deadline
                ),
                fd,
                parent.mount.id,
            ))
        } else {
            judged!(source_anchor::check(
                host,
                source_anchor::AnchorCheck::new(
                    kind,
                    &found.root,
                    &found.path,
                    found.identity,
                    deadline
                ),
                parent,
                name,
            ))
        };
        if evidence.as_ref() != found.source_evidence.as_ref() {
            return Ok(Err(Skip::new(
                Reason::Changed,
                found.path.clone(),
                "the recorded build source or source file identity changed since discovery",
            )));
        }
        Ok(Ok(evidence))
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
        let rules = Rules {
            empty_checkout_markers: &self.find.empty_checkout_markers,
            ..*self.rules
        };
        let record = match engine::walk(host, candidate, &rules, deadline) {
            Ok(record) => record,
            Err(WalkStop::Skipped(skip)) => return Ok(Err(skip)),
            Err(WalkStop::DeadlinePassed { path }) => return Err(Stop::DeadlinePassed { path }),
            Err(WalkStop::Failed { path, error }) => return Err(Stop::Failed { path, error }),
        };
        if let Err(skip) = same_object(found, Some(record.root().identity), "walked") {
            return Ok(Err(skip));
        }
        let walked = Walked {
            newest_change: record.newest_change(),
            contains_checkout: record.contains_checkout(&self.find.empty_checkout_markers),
            record,
            source_evidence: None,
        };
        if idle && let Err(skip) = judge_idle(host, &found.path, walked.newest_change, self.idle) {
            return Ok(Err(skip.measured(walked.bytes())));
        }
        Ok(Ok(walked))
    }
}

/// Open an unlocked directory candidate once for all of a filter pass. A
/// file candidate has no entries; a reclaim uses the already-held lock fd.
fn open_selection_dir<H: Host>(
    host: &H,
    parent: &Resolved<H::Fd>,
    locked_fd: Option<&H::Fd>,
    found: &Found,
    deadline: Option<MonoTime>,
) -> Judged<Option<H::Fd>> {
    if found.identity.file_type != FileType::Directory || locked_fd.is_some() {
        return Ok(Ok(None));
    }
    check_deadline(host, deadline, &found.path)?;
    let name = file_name(found)?;
    match open_beneath(
        host,
        &parent.fd,
        parent.mount.id,
        Path::new(name),
        Access::Path,
        Want::Directory,
    ) {
        Ok((fd, _)) => Ok(Ok(Some(fd))),
        Err(ResolveError::Host(error)) => {
            Ok(Err(skip_or_stop(found.path.clone(), "open", &error)?))
        }
        Err(error) => Ok(Err(Skip::new(
            Reason::Changed,
            found.path.clone(),
            format!("cannot inspect selection filters: {error}"),
        ))),
    }
}

/// A missing regular child is a known false; an unanswered type or failed
/// stat is unknown and cannot satisfy either polarity of `!filter:`.
fn selection_regular_child<H: Host>(
    host: &H,
    candidate_fd: Option<&H::Fd>,
    found: &Found,
    entry: &str,
    filter_name: &str,
    deadline: Option<MonoTime>,
) -> Judged<bool> {
    let Some(fd) = candidate_fd else {
        return Ok(Ok(false));
    };
    let path = found.path.join(entry);
    check_deadline(host, deadline, &path)?;
    match host.statx(Base::Dir(fd), Path::new(entry)) {
        Ok(stat) => match stat.file_type {
            Some(file_type) => Ok(Ok(file_type == FileType::Regular)),
            None => Ok(Err(Skip::new(
                Reason::UnsupportedFs,
                path,
                format!("filter:{filter_name}: kernel did not report entry type"),
            ))),
        },
        Err(error) if error.is(Errno::ENOENT) => Ok(Ok(false)),
        Err(error) => Ok(Err(skip_or_stop(path, "stat", &error)?)),
    }
}

/// The idle requirement `idle_for` (none: anything passes) on a candidate
/// whose newest change is `newest`: an unknown age is never idle
/// (docs/reference.md, Predicates).
pub(crate) fn judge_idle<H: Host>(
    host: &H,
    at: &Path,
    newest: Option<Timestamp>,
    idle: IdleWindow,
) -> Result<(), Skip> {
    let now = host.wall_clock();
    if let Some(idle_for) = idle.at_least {
        match idle_for.verdict(newest, now) {
            Idleness::Idle => Ok(()),
            Idleness::NotIdle => Err(Skip::new(
                Reason::NotIdle,
                at.to_owned(),
                format!(
                    "newest change {} ago; idle_for {} required",
                    format_age(age_secs(now, newest)),
                    format_duration(idle_for.0)
                ),
            )),
            Idleness::Unknown(unknown) => {
                let reason = match unknown {
                    Unknown::Unanswered => Reason::UnsupportedFs,
                    Unknown::InFuture => Reason::FutureTime,
                };
                Err(Skip::new(
                    reason,
                    at.to_owned(),
                    format!(
                        "its newest change cannot be told ({unknown}): an unknown age is never idle"
                    ),
                ))
            }
        }?;
    }
    if let Some(bound) = idle.before {
        match bound.verdict(newest, now) {
            Idleness::NotIdle => Ok(()),
            Idleness::Idle => Err(Skip::new(
                Reason::TooIdle,
                at.to_owned(),
                format!(
                    "newest change {} ago, at least {}; !filter:idle-{} excludes it",
                    format_age(age_secs(now, newest)),
                    format_duration(bound.0),
                    format_duration(bound.0)
                ),
            )),
            Idleness::Unknown(Unknown::Unanswered) => Err(Skip::new(
                Reason::UnsupportedFs,
                at.to_owned(),
                "newest change cannot be told: an unknown age passes neither idle filter polarity",
            )),
            Idleness::Unknown(Unknown::InFuture) => Err(Skip::new(
                Reason::FutureTime,
                at.to_owned(),
                "newest change lies in the future: an unknown age passes neither idle filter polarity",
            )),
        }
    } else {
        Ok(())
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
/// first `statx` that reaches it). By path, or by place
/// ([`places::mount_within`], TASK-290): a mount point reached through a
/// non-recursive bind is an uncovered directory there, which Linux refuses
/// to remove (`EBUSY`) after everything around it was deleted. A candidate
/// whose place the table cannot tell is `unsafe_path`, never looked into.
pub(super) fn mounted_at_or_inside(mounts: &[MountEntry], at: &Path) -> Option<Skip> {
    let (mount, path) = match places::mount_within(mounts, at) {
        Ok(found) => found?,
        Err(why) => {
            return Some(Skip::new(
                Reason::UnsafePath,
                at.to_owned(),
                format!("cannot be placed in its filesystem (mountinfo): {why}"),
            ));
        }
    };
    let detail = if mount.mount_point == at {
        "a mount point (mountinfo): not touched".to_owned()
    } else if mount.mount_point == path {
        "a mount point inside (mountinfo): not looked into".to_owned()
    } else {
        format!(
            "a mount point inside by its place in the filesystem (mount {} is attached there, \
             mountinfo): not looked into",
            mount.mount_point.display()
        )
    };
    Some(Skip::new(Reason::ContainsMount, path, detail))
}

/// Why a configured root is not searched or reached.
pub(super) enum RootRefused {
    /// A misconfiguration, an error that excludes the tier: inside systemd's
    /// PrivateTmp, by a name on its path or by its mount's root (a bind
    /// mount of a `systemd-private-*` directory).
    PrivateTmp(String),
    /// A skip: on a network filesystem, no clear mount, or not reachable
    /// ([`unreachable_root`]), including a directory above it that could
    /// not be asked whether it is container storage.
    Skip(Skip),
    /// Inside container storage (`contains_special`, the storage root as
    /// its path): excluded on purpose, as a storage root its discovery
    /// lists is ([`container_storage::storage_above`]).
    Storage(Skip),
}

/// The configured root `root` reached under the mount table `mounts`:
/// judged on its path, then by [`resolve_in`], whose mount screen judges
/// every mount on the way from its mountinfo line before anything there is
/// opened or `statx`ed; then opened with no symlink and no mount crossed.
pub(super) fn resolve_root<H: Host>(
    host: &H,
    root: &Path,
    mounts: &[MountEntry],
    above: Above,
) -> Result<Result<Resolved<H::Fd>, RootRefused>, Stop> {
    if path_in_private_tmp(root) {
        return Ok(Err(RootRefused::PrivateTmp(format!(
            "inside systemd's PrivateTmp ({PRIVATE_TMP_PREFIX}*)"
        ))));
    }
    let why = match resolve_in(host, mounts, root, Want::Directory) {
        // A root inside container storage is refused like a candidate
        // inside it (docs/safety.md, No container storage), at every
        // resolution: discovery, the approach and each recheck.
        Ok(resolved) => {
            if above == Above::Skip {
                return Ok(Ok(resolved));
            }
            return Ok(match container_storage::storage_above(host, mounts, root) {
                Ok(None) => Ok(resolved),
                Ok(Some(inside)) => Err(RootRefused::Storage(inside)),
                Err(e) => Err(RootRefused::Skip(unreachable_root(root, e)?)),
            });
        }
        Err(ResolveError::Screened { why, .. }) => why,
        Err(e) => return Ok(Err(RootRefused::Skip(unreachable_root(root, e)?))),
    };
    Ok(Err(match why {
        MountScreenError::Unsafe { at, why } => RootRefused::Skip(Skip::new(
            Reason::UnsafePath,
            root.to_owned(),
            format!(
                "unsafe_path: {}: mount at {}: {why}",
                root.display(),
                at.display()
            ),
        )),
        MountScreenError::Networked {
            at,
            mount_point,
            fs_type,
        } => RootRefused::Skip(Skip::new(
            Reason::UnsupportedFs,
            root.to_owned(),
            format!(
                "ancestor {} is on {} ({fs_type}, mount {}): never opened (a dead server would hang it)",
                at.display(),
                networked_why(&fs_type),
                mount_point.display()
            ),
        )),
        MountScreenError::PrivateTmp {
            at,
            mount_point,
            fs_root,
        } => RootRefused::PrivateTmp(format!(
            "ancestor {} is on mount {} whose root is {} in its filesystem: a bind mount of systemd's PrivateTmp ({PRIVATE_TMP_PREFIX}*)",
            at.display(),
            mount_point.display(),
            fs_root.display()
        )),
    }))
}

/// A root that cannot be reached: its skip reason (`unsafe_path`,
/// `missing`, `unsupported_fs`, `unreadable`), or an error of the run.
pub(super) fn unreachable_root(root: &Path, e: ResolveError) -> Result<Skip, Stop> {
    let reason = match &e {
        ResolveError::UnsafePath { .. } => Reason::UnsafePath,
        ResolveError::UnsupportedFs { .. } => Reason::UnsupportedFs,
        ResolveError::Missing { .. } => Reason::Missing,
        ResolveError::Screened { why, .. } => why.skip_reason(),
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

/// The source's own entry predicates on the entry `name` (at `at`, of
/// type `file_type`) in `dir`. `has` and `has_not` inspect regular files;
/// `has_dir` requires a direct child directory without following links;
/// `lacks_entry` refuses any directory entry of its name, including a link.
/// A file candidate holds nothing. A directory that cannot be read is
/// neither (`Err`: refused as `unreadable`, never taken). The deadline is
/// checked before each predicate's I/O.
/// Retain a candidate unless at least `count` matching siblings have a
/// strictly newer rank. The name breaks equal-mtime ties deterministically.
/// Missing timestamps and incomplete listings retain every candidate.
fn keep_newest<H: Host>(
    host: &H,
    parent: &Resolved<H::Fd>,
    found: &Found,
    find: &Find,
    count: usize,
    deadline: Option<MonoTime>,
) -> Judged<()> {
    let retain = |detail: String| Skip::new(Reason::NewestVersion, found.path.clone(), detail);
    let name = file_name(found)?;
    let group = match version_group(name, find.newest_group) {
        Some(group) => group,
        None => return Ok(Err(retain("candidate version group is unknown".to_owned()))),
    };
    check_deadline(host, deadline, &found.path)?;
    let names = match host.read_dir(&parent.fd, engine::DEFAULT_MAX_ENTRIES) {
        Ok(crate::host::DirListing::Names(names)) => names,
        Ok(crate::host::DirListing::TooMany) => {
            return Ok(Err(retain(
                "candidate parent has too many entries to rank".to_owned(),
            )));
        }
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot list version siblings: {error}"
            ))));
        }
    };
    let mut ranked = Vec::new();
    let root = [found.root.as_path()];
    for sibling in names {
        let sibling_path = found.path.with_file_name(&sibling);
        if !path_matches(Some(&root), &find.patterns, &sibling_path) {
            continue;
        }
        let Some(sibling_group) = version_group(&sibling, find.newest_group) else {
            if sibling.as_bytes().starts_with(&group) {
                return Ok(Err(retain(format!(
                    "cannot parse version group of {}",
                    sibling_path.display()
                ))));
            }
            continue;
        };
        if sibling_group != group {
            continue;
        }
        check_deadline(host, deadline, &sibling_path)?;
        let stat = match host.statx(Base::Dir(&parent.fd), Path::new(&sibling)) {
            Ok(stat) => stat,
            Err(error) => {
                return Ok(Err(retain(format!(
                    "cannot stat version sibling {}: {error}",
                    sibling_path.display()
                ))));
            }
        };
        let same_type = match find.entry_type {
            crate::profile::EntryType::Dir => stat.file_type == Some(FileType::Directory),
            crate::profile::EntryType::File => stat.file_type == Some(FileType::Regular),
            crate::profile::EntryType::Any => matches!(
                stat.file_type,
                Some(FileType::Directory | FileType::Regular)
            ),
        };
        if !same_type {
            continue;
        }
        if stat.dev != found.identity.id.dev || stat.mnt_id != Some(parent.mount.id) {
            return Ok(Err(retain(format!(
                "version sibling {} is on another or unknown mount",
                sibling_path.display()
            ))));
        }
        let Some(mtime) = stat.mtime else {
            return Ok(Err(retain(format!(
                "version sibling {} has no mtime",
                sibling_path.display()
            ))));
        };
        ranked.push((mtime, sibling));
    }
    ranked.sort();
    ranked.reverse();
    let Some(rank) = ranked.iter().position(|(_, sibling)| sibling == name) else {
        return Ok(Err(retain(
            "candidate disappeared from its version siblings".to_owned(),
        )));
    };
    if rank < count {
        return Ok(Err(retain(format!(
            "version rank {} of {}; keeping newest {count}",
            rank + 1,
            ranked.len()
        ))));
    }
    Ok(Ok(()))
}

/// Extension directory names end in `-<major>.<minor>.<patch>` and an
/// optional known target platform. The prefix is the extension id. A name
/// with multiple version-shaped segments is ambiguous and retained.
fn version_group(name: &OsStr, mode: Option<NewestGroup>) -> Option<Vec<u8>> {
    match mode.unwrap_or(NewestGroup::Siblings) {
        NewestGroup::Siblings => Some(Vec::new()),
        NewestGroup::CursorExtension => {
            let bytes = name.as_bytes();
            let mut group = None;
            let mut seen_version = false;
            for (split, byte) in bytes.iter().enumerate() {
                if *byte != b'-' || split == 0 {
                    continue;
                }
                let suffix = &bytes[split + 1..];
                let number = suffix
                    .split(|byte| *byte == b'-')
                    .next()
                    .unwrap_or_default();
                if !cursor_version_number(number) {
                    continue;
                }
                if seen_version {
                    return None;
                }
                seen_version = true;
                if cursor_version_suffix(suffix) {
                    group = Some(bytes[..split].to_vec());
                }
            }
            group
        }
    }
}

fn cursor_version_suffix(suffix: &[u8]) -> bool {
    let (version, platform) = match suffix.iter().position(|byte| *byte == b'-') {
        Some(split) => (&suffix[..split], Some(&suffix[split + 1..])),
        None => (suffix, None),
    };
    if !cursor_version_number(version) {
        return false;
    }
    match platform {
        None => true,
        Some(platform) => [
            b"universal".as_slice(),
            b"web",
            b"linux-x64",
            b"linux-arm64",
            b"linux-armhf",
            b"linux-x64-musl",
            b"linux-arm64-musl",
            b"darwin-x64",
            b"darwin-arm64",
            b"win32-x64",
            b"win32-arm64",
            b"win32-ia32",
        ]
        .contains(&platform),
    }
}

fn cursor_version_number(version: &[u8]) -> bool {
    let numbers: Vec<_> = version.split(|byte| *byte == b'.').collect();
    if numbers.len() != 3
        || numbers
            .iter()
            .any(|part| part.is_empty() || !part.iter().all(u8::is_ascii_digit))
    {
        return false;
    }
    true
}

/// Cursor records installed extension directories as `relativeLocation`
/// entries in `extensions.json`. The whole registry must be readable and
/// internally consistent before any version absent from it can be selected.
fn protect_cursor_extension<H: Host>(
    host: &H,
    found: &Found,
    path: &Path,
    deadline: Option<MonoTime>,
) -> Judged<()> {
    const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
    let retain = |detail: String| Skip::new(Reason::InstalledExtension, found.path.clone(), detail);
    if path.parent() != found.path.parent() {
        return Ok(Err(retain(format!(
            "Cursor registry {} is not beside candidate {}",
            path.display(),
            found.path.display()
        ))));
    }
    check_deadline(host, deadline, path)?;
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot check Cursor registry mount: {error}"
            ))));
        }
    };
    let bytes = match read_small_stable_file(host, &mounts, path, MAX_MANIFEST_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return Ok(Err(retain(format!(
                "Cursor registry {} is missing",
                path.display()
            ))));
        }
        Err(why) => return Ok(Err(retain(format!("Cursor registry: {why}")))),
    };
    let entries: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(entries) => entries,
        Err(error) => {
            return Ok(Err(retain(format!(
                "Cursor registry {} is malformed JSON: {error}",
                path.display()
            ))));
        }
    };
    let Some(entries) = entries.as_array() else {
        return Ok(Err(retain(format!(
            "Cursor registry {} is not an array",
            path.display()
        ))));
    };
    let Some(parent) = path.parent() else {
        return Ok(Err(retain(format!(
            "Cursor registry {} has no parent",
            path.display()
        ))));
    };
    for entry in entries {
        let Some(relative) = entry
            .get("relativeLocation")
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(Err(retain(format!(
                "Cursor registry {} has an entry without relativeLocation",
                path.display()
            ))));
        };
        let mut components = Path::new(relative).components();
        let Some(Component::Normal(name)) = components.next() else {
            return Ok(Err(retain(format!(
                "Cursor registry {} has unsafe relativeLocation {relative:?}",
                path.display()
            ))));
        };
        if components.next().is_some() {
            return Ok(Err(retain(format!(
                "Cursor registry {} has unsafe relativeLocation {relative:?}",
                path.display()
            ))));
        }
        let installed = parent.join(name);
        let Some(location) = entry
            .get("location")
            .and_then(|v| v.get("path"))
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(Err(retain(format!(
                "Cursor registry {} has an entry without location.path",
                path.display()
            ))));
        };
        if Path::new(location) != installed {
            return Ok(Err(retain(format!(
                "Cursor registry {} disagrees about installed path {relative:?}",
                path.display()
            ))));
        }
        if installed == found.path {
            return Ok(Err(retain(format!(
                "Cursor registry {} lists this installed extension",
                path.display()
            ))));
        }
    }
    Ok(Ok(()))
}

/// The bytes of a small regular file reached from `/` without a symlink
/// or mount crossing (network and FUSE mounts are screened before anything
/// there is opened), unchanged while read: `Ok(None)` when it, or a
/// directory on its way, does not exist (`ENOENT`, `ENOTDIR`); `Err` with
/// the cause otherwise (not a regular file, over `limit` bytes, changed,
/// unreadable).
pub(super) fn read_small_stable_file<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    path: &Path,
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    let opened = match resolve_in(host, mounts, path, Want::Any) {
        Ok(opened) => opened,
        Err(ResolveError::Missing { .. }) => return Ok(None),
        Err(error) => return Err(format!("cannot reach {}: {error}", path.display())),
    };
    if opened.stat.file_type != Some(FileType::Regular) {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let bytes = host
        .read_head(&opened.fd, limit + 1)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!("{} exceeds the {limit} byte limit", path.display()));
    }
    let after = resolve_in(host, mounts, path, Want::Any)
        .map_err(|error| format!("cannot recheck {}: {error}", path.display()))?;
    if after.stat != opened.stat {
        return Err(format!("{} changed while read", path.display()));
    }
    Ok(Some(bytes))
}

/// A `protect_link_dirs` entry, relative to the bound root: a literal
/// directory, or one with a single `*` segment (`.kani/*`), which stands
/// for every directory of that level (listed afresh at each judgement; a
/// non-directory there is passed over). Each directory is a launcher
/// directory ([`protect_link_directory`]). A level that cannot be listed
/// retains the candidate; one that does not exist holds no launchers.
fn protect_link_dir_pattern<H: Host>(
    host: &H,
    found: &Found,
    dir: &str,
    deadline: Option<MonoTime>,
) -> Judged<()> {
    let Some((above, below)) = split_star(dir) else {
        return protect_link_directory(host, found, &found.root.join(dir), deadline);
    };
    let retain = |detail: String| Skip::new(Reason::LinkedCurrent, found.path.clone(), detail);
    let parent_path = found.root.join(above);
    check_deadline(host, deadline, &parent_path)?;
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot check launcher mounts: {error}"
            ))));
        }
    };
    let parent = match resolve_in(host, &mounts, &parent_path, Want::Directory) {
        Ok(parent) => parent,
        Err(ResolveError::Missing { .. }) => return Ok(Ok(())),
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot reach launcher directories {}: {error}",
                parent_path.display()
            ))));
        }
    };
    let names = match host.read_dir(&parent.fd, engine::DEFAULT_MAX_ENTRIES) {
        Ok(DirListing::Names(names)) => names,
        Ok(DirListing::TooMany) => {
            return Ok(Err(retain(format!(
                "{} has too many entries to check for launchers",
                parent_path.display()
            ))));
        }
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot list {}: {error}",
                parent_path.display()
            ))));
        }
    };
    for name in names {
        let at = parent_path.join(&name);
        check_deadline(host, deadline, &at)?;
        if is_mount_point(&mounts, &at) {
            return Ok(Err(retain(format!(
                "{} is a mount point: its launchers are not checked",
                at.display()
            ))));
        }
        let stat = match host.statx(Base::Dir(&parent.fd), Path::new(&name)) {
            Ok(stat) => stat,
            Err(error) => {
                return Ok(Err(retain(format!(
                    "cannot stat {}: {error}",
                    at.display()
                ))));
            }
        };
        if stat.file_type == Some(FileType::Directory) {
            judged!(protect_link_directory(
                host,
                found,
                &at.join(below),
                deadline
            ));
        }
    }
    Ok(Ok(()))
}

/// A path with one whole `*` segment, split around it (`a/*/b`: `a`,
/// `b`); `None` when it has none.
pub(crate) fn split_star(dir: &str) -> Option<(&str, &str)> {
    let segments: Vec<&str> = dir.split('/').collect();
    let star = segments.iter().position(|segment| *segment == "*")?;
    let above_len: usize = segments[..star].iter().map(|s| s.len() + 1).sum();
    let above = dir.get(..above_len.saturating_sub(1)).unwrap_or_default();
    let below = dir.get(above_len + 2..).unwrap_or_default();
    Some((above, below))
}

/// Every entry in a launcher directory must be listed and statted. The
/// directory itself must be safely reachable when present. An absent
/// directory contains no launchers; a partial listing, an unsafe path or an
/// unreadable entry retains the candidate: an unseen symlink might point
/// into it. This is repeated during reclaim under its lock.
pub(super) fn protect_link_directory<H: Host>(
    host: &H,
    found: &Found,
    path: &Path,
    deadline: Option<MonoTime>,
) -> Judged<()> {
    const MAX_LAUNCHER_ENTRIES: usize = 16_384;
    let retain = |detail: String| Skip::new(Reason::LinkedCurrent, found.path.clone(), detail);
    check_deadline(host, deadline, path)?;
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot check launcher mounts: {error}"
            ))));
        }
    };
    let directory = match resolve_in(host, &mounts, path, Want::Directory) {
        Ok(directory) => directory,
        Err(ResolveError::Missing {
            errno: Errno::ENOENT,
            ..
        }) => return Ok(Ok(())),
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot reach launcher directory {}: {error}",
                path.display()
            ))));
        }
    };
    let names = match host.read_dir(&directory.fd, MAX_LAUNCHER_ENTRIES) {
        Ok(DirListing::Names(names)) => names,
        Ok(DirListing::TooMany) => {
            return Ok(Err(retain(format!(
                "launcher directory {} has more than {MAX_LAUNCHER_ENTRIES} entries",
                path.display()
            ))));
        }
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot list launcher directory {}: {error}",
                path.display()
            ))));
        }
    };
    for name in names {
        if !is_single_name(&name) {
            return Ok(Err(retain(format!(
                "launcher directory {} listed an invalid name",
                path.display()
            ))));
        }
        let link = path.join(&name);
        check_deadline(host, deadline, &link)?;
        let stat = match host.statx(Base::Dir(&directory.fd), Path::new(&name)) {
            Ok(stat) => stat,
            Err(error) => {
                return Ok(Err(retain(format!(
                    "cannot stat launcher entry {}: {error}",
                    link.display()
                ))));
            }
        };
        if stat.file_type.is_none() {
            return Ok(Err(retain(format!(
                "launcher entry {} has no reported file type",
                link.display()
            ))));
        }
        if stat.file_type == Some(FileType::Symlink) {
            judged!(protect_link(host, found, &link, false, deadline));
        }
    }
    let after = match host.statx(Base::Dir(&directory.fd), Path::new("")) {
        Ok(stat) => stat,
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot recheck launcher directory {}: {error}",
                path.display()
            ))));
        }
    };
    if after != directory.stat {
        return Ok(Err(retain(format!(
            "launcher directory {} changed while scanned",
            path.display()
        ))));
    }
    Ok(Ok(()))
}

/// A named link must exist, remain stable while read, and resolve to an
/// object inside the bound root without a symlink or mount crossing on its
/// way. Any uncertainty retains every candidate of this source.
fn protect_link<H: Host>(
    host: &H,
    found: &Found,
    path: &Path,
    required: bool,
    deadline: Option<MonoTime>,
) -> Judged<()> {
    let retain = |detail: String| Skip::new(Reason::LinkedCurrent, found.path.clone(), detail);
    let Some((parent_path, name)) = path.parent().zip(path.file_name()) else {
        return Ok(Err(retain(format!(
            "invalid configured link: {}",
            path.display()
        ))));
    };
    check_deadline(host, deadline, path)?;
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot check current link mount: {error}"
            ))));
        }
    };
    let parent = match resolve_in(host, &mounts, parent_path, Want::Directory) {
        Ok(parent) => parent,
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot reach current link {}: {error}",
                path.display()
            ))));
        }
    };
    let before = match host.statx(Base::Dir(&parent.fd), Path::new(name)) {
        Ok(stat) => stat,
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot stat current link {}: {error}",
                path.display()
            ))));
        }
    };
    if before.file_type != Some(FileType::Symlink) {
        return Ok(Err(retain(format!(
            "current link {} is not a symlink",
            path.display()
        ))));
    }
    let text = match host.read_link(&parent.fd, name) {
        Ok(text) => text,
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot read current link {}: {error}",
                path.display()
            ))));
        }
    };
    let after = match host.statx(Base::Dir(&parent.fd), Path::new(name)) {
        Ok(stat) => stat,
        Err(error) => {
            return Ok(Err(retain(format!(
                "cannot recheck current link {}: {error}",
                path.display()
            ))));
        }
    };
    if before != after {
        return Ok(Err(retain(format!(
            "current link {} changed while read",
            path.display()
        ))));
    }
    let Some(target) = normalize_link_target(parent_path, &text) else {
        return Ok(Err(retain(format!(
            "current link {} has an invalid target",
            path.display()
        ))));
    };
    let target = match resolve_link_chain(host, &mounts, &target, &found.root, required) {
        Ok(target) => target,
        Err(detail) => {
            return Ok(Err(retain(format!(
                "current link {} has an unresolved target: {detail}",
                path.display()
            ))));
        }
    };
    if !target.starts_with(&found.root) {
        if !required {
            // An unrelated launcher may point to another installed tool.
            // Its complete chain was resolved without ambiguity above.
            return Ok(Ok(()));
        }
        return Ok(Err(retain(format!(
            "current link {} resolves outside bound root: {}",
            path.display(),
            target.display()
        ))));
    }
    check_deadline(host, deadline, &target)?;
    if target.starts_with(&found.path) {
        return Ok(Err(retain(format!(
            "current link {} points into this version ({})",
            path.display(),
            target.display()
        ))));
    }
    Ok(Ok(()))
}

/// Follow a bounded chain one component at a time. Every symlink is read
/// below a safely reached parent, and the final object is reached again
/// without symlinks. This is needed for launcher aliases through another
/// tool's `current` link; a lexical path alone can name the wrong version.
fn resolve_link_chain<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    initial: &Path,
    root: &Path,
    required: bool,
) -> Result<PathBuf, String> {
    match follow_links(host, mounts, initial)? {
        Followed::Reached(path) => Ok(path),
        // A dangling launcher elsewhere (for example an old Nix profile)
        // cannot name a candidate below this root. Every existing prefix
        // was checked for symlinks first.
        Followed::Dangling { path, missing } if !required && !missing.starts_with(root) => Ok(path),
        Followed::Dangling { missing, .. } => Err(format!(
            "stat {}: no such file or directory",
            missing.display()
        )),
    }
}

/// Where [`follow_links`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Followed {
    /// The object every symlink on the way leads to, reached again without
    /// a symlink.
    Reached(PathBuf),
    /// A name on the way does not exist (`ENOENT`, `ENOTDIR`): `path` is
    /// the target as far as it was followed, `missing` the first prefix of
    /// it that is not there.
    Dangling { path: PathBuf, missing: PathBuf },
}

/// Follow a bounded chain one component at a time, as the kernel would
/// resolve `initial`: every symlink is read below a safely reached parent
/// (mount-screened, no crossing), its text normalised without folding a
/// `..` after a named component, and the final object reached again
/// without symlinks. `Err` for anything unsafe or unreadable on the way.
pub(super) fn follow_links<H: Host>(
    host: &H,
    mounts: &[MountEntry],
    initial: &Path,
) -> Result<Followed, String> {
    let mut path = initial.to_owned();
    for _ in 0..40 {
        let mut prefix = PathBuf::from("/");
        let mut next = None;
        for component in path.components() {
            let Component::Normal(name) = component else {
                if component == Component::RootDir {
                    continue;
                }
                return Err(format!("non-normal target {}", path.display()));
            };
            let parent_path = prefix.clone();
            prefix.push(name);
            let parent = resolve_in(host, mounts, &parent_path, Want::Directory)
                .map_err(|error| format!("reach {}: {error}", parent_path.display()))?;
            let before = match host.statx(Base::Dir(&parent.fd), Path::new(name)) {
                Ok(stat) => stat,
                Err(error) if error.is(Errno::ENOENT) || error.is(Errno::ENOTDIR) => {
                    return Ok(Followed::Dangling {
                        path,
                        missing: prefix,
                    });
                }
                Err(error) => return Err(format!("stat {}: {error}", prefix.display())),
            };
            if before.file_type != Some(FileType::Symlink) {
                continue;
            }
            let text = host
                .read_link(&parent.fd, name)
                .map_err(|error| format!("read {}: {error}", prefix.display()))?;
            let after = host
                .statx(Base::Dir(&parent.fd), Path::new(name))
                .map_err(|error| format!("recheck {}: {error}", prefix.display()))?;
            if before != after {
                return Err(format!("{} changed while read", prefix.display()));
            }
            let mut target = normalize_link_target(&parent_path, &text)
                .ok_or_else(|| format!("{} has an ambiguous target", prefix.display()))?;
            target.push(path.strip_prefix(&prefix).expect("prefix of target"));
            next = Some(target);
            break;
        }
        if let Some(target) = next {
            path = target;
            continue;
        }
        resolve_in(host, mounts, &path, Want::Any)
            .map_err(|error| format!("reach {}: {error}", path.display()))?;
        return Ok(Followed::Reached(path));
    }
    Err(format!(
        "target {} has more than 40 symlink steps",
        initial.display()
    ))
}

pub(super) fn normalize_link_target(parent: &Path, text: &Path) -> Option<PathBuf> {
    let mut path = if text.is_absolute() {
        PathBuf::from("/")
    } else {
        parent.to_owned()
    };
    let mut saw_normal = false;
    for component in text.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                saw_normal = true;
                path.push(name);
            }
            Component::ParentDir => {
                // After a named component the kernel might follow a symlink
                // before applying `..`; lexical folding would name a
                // different object. Leading `..` traverses only ancestors
                // of the already safely reached link directory.
                if text.is_absolute() || saw_normal {
                    return None;
                }
                if !path.pop() {
                    return None;
                }
            }
            Component::Prefix(_) => return None,
        }
    }
    Some(path)
}

pub(super) fn source_where<H: Host>(
    host: &H,
    find: &Find,
    dir: &Resolved<H::Fd>,
    at: &Path,
    name: &OsStr,
    file_type: Option<FileType>,
    deadline: Option<MonoTime>,
) -> Judged<bool> {
    let (has, has_dir, has_not, lacks_entry) =
        (&find.has, &find.has_dir, &find.has_not, &find.lacks_entry);
    if has.is_none() && has_dir.is_none() && has_not.is_none() && lacks_entry.is_none() {
        return Ok(Ok(true));
    }
    if file_type != Some(FileType::Directory) {
        return Ok(Ok(has.is_none() && has_dir.is_none()));
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
            Ok(stat) => match stat.file_type {
                Some(file_type) => Ok(Ok(file_type == FileType::Regular)),
                None => Ok(Err(Skip::new(
                    Reason::UnsupportedFs,
                    at.join(file),
                    "source where: kernel did not report entry type",
                ))),
            },
            Err(e) if e.is(Errno::ENOENT) => Ok(Ok(false)),
            Err(e) => Ok(Err(skip_or_stop(at.join(file), "stat", &e)?)),
        }
    };
    if let Some(file) = has
        && !judged!(holds(file))
    {
        return Ok(Ok(false));
    }
    if let Some(name) = has_dir {
        check_deadline(host, deadline, &at.join(name))?;
        match host.statx(Base::Dir(&fd), Path::new(name)) {
            Ok(stat) if stat.file_type == Some(FileType::Directory) => {}
            Ok(_) => return Ok(Ok(false)),
            Err(e) if e.is(Errno::ENOENT) => return Ok(Ok(false)),
            Err(e) => return Ok(Err(skip_or_stop(at.join(name), "stat", &e)?)),
        }
    }
    if let Some(file) = has_not
        && judged!(holds(file))
    {
        return Ok(Ok(false));
    }
    if let Some(name) = lacks_entry {
        check_deadline(host, deadline, &at.join(name))?;
        match host.statx(Base::Dir(&fd), Path::new(name)) {
            // Any successful stat means the name exists. In particular, do
            // not follow a symlink or interpret an unknown type as absence.
            Ok(_) => return Ok(Ok(false)),
            Err(e) if e.is(Errno::ENOENT) => {}
            Err(e) => return Ok(Err(skip_or_stop(at.join(name), "stat", &e)?)),
        }
    }
    Ok(Ok(true))
}

/// A candidate reached again from its root ([`reach`]): the directories
/// from the root down to its parent, each held open (`O_PATH`), so that
/// the path can be shown to still lead through them ([`still_reached`])
/// and the keep markers on the way judged again ([`no_keep_markers`]).
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
            "not the entry found (found {}, {what} {})",
            found.identity,
            engine::Identity::shown(now)
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
    let top = judged!(reach_root(host, root, mounts, deadline));
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
        if is_mount_point(mounts, &at) {
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

/// The configured root `root` resolved now under the mount table `mounts`:
/// its mount's line judged first (a `systemd-private-*` bind mount, a
/// network filesystem), then opened with no symlink and no mount crossed.
fn reach_root<H: Host>(
    host: &H,
    root: &Path,
    mounts: &[MountEntry],
    deadline: Option<MonoTime>,
) -> Judged<Resolved<H::Fd>> {
    check_deadline(host, deadline, root)?;
    Ok(match resolve_root(host, root, mounts, Above::Ask)? {
        Ok(dir) => Ok(dir),
        Err(RootRefused::PrivateTmp(why)) => Err(super::discover::private_root(root, &why)),
        Err(RootRefused::Skip(skip) | RootRefused::Storage(skip)) => Err(skip),
    })
}

/// Whether the path `found.path` reaches, now, exactly what [`reach`]
/// reached (TASK-165): its root resolved again, then, one level at a time,
/// each name `statx`ed (not followed) in the directory `reached` holds for
/// the level above, and each result must be the directory held for that
/// level, and the candidate's name in the parent held the object `found`
/// (identity: device, inode, owner, type, birth time). Every check of the
/// path text (the checkout path rule, `protect`, the keep markers held) is
/// sound only after this: a directory on the way renamed since the reach
/// (into a kept directory, a checkout's metadata, out of the root) leaves
/// the held fds valid but the path pointing elsewhere, and is `changed`.
/// Then [`no_keep_markers`] on the held directories, which are now the
/// directories on the path.
///
/// Costs one transient resolution of the root and one `statx` per level;
/// it opens nothing that stays open, so the reclaim's fd budget is that of
/// [`reach`]. Each level is checked at its own instant; a rename after a
/// level's check is the window after the check (docs/safety.md, Deletion:
/// accepted limits, check to delete).
pub(crate) fn still_reached<H: Host>(
    host: &H,
    found: &Found,
    reached: &Reached<H::Fd>,
    mounts: &[MountEntry],
    deadline: Option<MonoTime>,
) -> Judged<()> {
    let changed = |at: &Path, now: &str| {
        Ok(Err(Skip::new(
            Reason::Changed,
            found.path.clone(),
            format!(
                "its path no longer leads to it: {} {now} since it was reached",
                at.display()
            ),
        )))
    };
    let (root_path, root) = reached.dirs.first().expect("the root at least");
    assert_eq!(root_path, &found.root, "reached from the candidate's root");
    let fresh = match reach_root(host, root_path, mounts, deadline)? {
        Ok(fresh) => fresh,
        // The root reached before is gone from its path: the path no longer
        // leads to the candidate, like any other level gone.
        Err(skip) if skip.reason == Reason::Missing => return changed(root_path, "is gone"),
        Err(skip) => return Ok(Err(skip)),
    };
    if engine::Identity::of(&fresh.stat).is_none()
        || engine::Identity::of(&fresh.stat) != engine::Identity::of(&root.stat)
    {
        return changed(root_path, "is another directory");
    }
    drop(fresh);
    for pair in reached.dirs.windows(2) {
        let ((_, above), (at, held)) = (&pair[0], &pair[1]);
        let name = at
            .file_name()
            .expect("a directory below the root has a name");
        if is_mount_point(mounts, at) {
            return Ok(Err(Skip::new(
                Reason::ContainsMount,
                at.clone(),
                "a mount point on the way to the candidate (mountinfo): not entered",
            )));
        }
        check_deadline(host, deadline, at)?;
        match host.statx(Base::Dir(&above.fd), Path::new(name)) {
            Ok(stat) => {
                let now = engine::Identity::of(&stat);
                if now.is_none() || now != engine::Identity::of(&held.stat) {
                    return changed(at, "is another entry");
                }
            }
            Err(e) if e.is(Errno::ENOENT) => return changed(at, "is gone"),
            Err(e) => return Ok(Err(skip_or_stop(at.clone(), "stat", &e)?)),
        }
    }
    let name = file_name(found)?;
    check_deadline(host, deadline, &found.path)?;
    match host.statx(Base::Dir(&reached.parent().fd), Path::new(name)) {
        Ok(stat) => judged!(Ok(same_object(
            found,
            engine::Identity::of(&stat),
            "on its path now"
        ))),
        Err(e) if e.is(Errno::ENOENT) => return changed(&found.path, "is gone"),
        Err(e) => return Ok(Err(skip_or_stop(found.path.clone(), "stat", &e)?)),
    }
    judged!(no_keep_markers(host, reached, mounts, deadline));
    outside_container_storage(host, reached, found, mounts, deadline)
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
        check_deadline(host, deadline, &marker)?;
        match name_present(host, &dir.fd, path, OsStr::new(KEEP_MARKER), mounts) {
            Ok(false) => {}
            Ok(true) => {
                return Ok(Err(Skip::new(
                    Reason::KeepMarker,
                    marker,
                    "between the root and the candidate",
                )));
            }
            Err(e) => return Ok(Err(skip_or_stop(marker, "stat", &e)?)),
        }
    }
    Ok(Ok(()))
}

/// The candidate `found` lies inside no container storage
/// ([`engine::container_storage`], `contains_special`): the root and every
/// directory on the way to the candidate, the directories [`reach`] went
/// through, is asked for the recogniser's marker names
/// ([`container_storage::storage_in`]: a `statx` of each, a rule's later
/// names only where its first held). Fail closed as [`no_keep_markers`]:
/// a marker that cannot be told refuses the candidate. Above the root,
/// [`resolve_root`] asks.
pub(crate) fn outside_container_storage<H: Host>(
    host: &H,
    reached: &Reached<H::Fd>,
    found: &Found,
    mounts: &[MountEntry],
    deadline: Option<MonoTime>,
) -> Judged<()> {
    for (path, dir) in &reached.dirs {
        check_deadline(host, deadline, path)?;
        match container_storage::storage_in(host, &dir.fd, path, mounts) {
            Ok(None) => {}
            Ok(Some(storage)) => return Ok(Err(storage.inside(path, &found.path))),
            Err((marker, e)) => return Ok(Err(skip_or_stop(marker, "stat", &e)?)),
        }
    }
    Ok(Ok(()))
}
