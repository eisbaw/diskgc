//! The reclaim protocol (docs/safety.md, `cargo-target`: Reclaim
//! protocol): the step that deletes Cargo build output.
//!
//! 1. **Lock.** Check, as the source profile's `checkouts` says, that no
//!    profile directory lies inside a checkout's metadata by its path, its
//!    own name included ([`judge_checkout_paths`]), and that it is in a
//!    layout diskgc can reclaim as found ([`judge_layout`]); reach the
//!    target again from its root ([`reach`]), then take every lock file of
//!    every profile directory, exclusively and without waiting, in Cargo's
//!    own order ([`LOCK_ORDER`]). Each profile directory is opened once,
//!    below the target, and must be the one
//!    found; each lock file is opened `O_PATH` below it, must be the one
//!    found (identity, type included), and is reopened read-only through
//!    its fd ([`crate::host::Host::reopen_regular`]), never by name, for
//!    the `flock`. Any lock held by someone else: `locked`.
//! 2. **Recheck under the locks.** Inspect the target again: the same
//!    profile directories and lock files as when found (`changed`), no keep
//!    marker, ownership, the layout ([`judge_inspected`]); walk every entry
//!    of the locked profile directories below the directories opened in
//!    step 1, within one entry budget for the target; the last build (newer
//!    than the one the caller judged: `changed`), the idle requirement, and
//!    the in-use check of the whole target against a scan made now, under
//!    the locks.
//! 3. **Delete inside the locked profile directories**, each part with the
//!    deletion engine's identity-checked delete, through
//!    [`delete_cleared`] on the in-use check of exactly that part's record
//!    (and the Cargo heuristics on the same scan), in this order
//!    ([`plan`]): the fingerprints of every profile directory first,
//!    whatever its layout or mix of layouts (`.fingerprint/`, and every
//!    `build/<package>/<hash>/fingerprint`, split off the `build/` record),
//!    so that a profile deleted part-way always rebuilds; then every other
//!    entry in name order, `build/` and `deps/` last, so that a run stopped
//!    part-way leaves a profile directory still in a known layout. Before
//!    each part its profile directory's lock files are listed again: one
//!    created, removed or replaced while the locks are held (a Cargo of
//!    another lock set starting) is `changed`, and nothing more is deleted.
//!    That is a detector, not the protection: every lock set diskgc
//!    reclaims holds `.cargo-lock`, which every measured Cargo takes before
//!    it writes into the profile directory, so a Cargo that creates a new
//!    lock file meanwhile waits on the `.cargo-lock` diskgc holds.
//!    Lock files are never in a record (the walks refuse every Cargo lock
//!    name), so they are never unlinked or replaced.
//! 4. **Release** by closing the lock fds (dropping the [`Locked`]
//!    handles) when the protocol returns, whatever it returns.
//!
//! Nothing outside the locked profile directories is touched: not the
//! target directory, its `CACHEDIR.TAG`, the triple directories, nor
//! `doc/`, `tmp/` or `criterion/`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::discover::{check_deadline, failure, list, open_dir, profile_dir};
use super::eligible::{
    Budget, judge_checkout_paths, judge_idle, judge_in_use, judge_inspected, open_profile,
    walk_entries,
};
use super::{
    Inspected, Judged, LockFile, ProfileDir, ProfileWalk, Project, Stop, Target, cargo_evidence,
    inspect, judge_layout, last_build, reach,
};
use crate::engine::{Candidate, DeleteOptions, End, Identity, Reason, Rules, Skip, WalkRecord};
use crate::host::{
    Access, Delete, Host, HostError, LockMode, Locked, MonoTime, Timestamp, TryLock,
};
use crate::inuse::{Checker, NotCleared, Options, delete_cleared};
use crate::profile::IdleFor;
use crate::resolve::{ResolveError, Resolved, Unsafe, Want, open_beneath};

/// The order in which a profile directory's lock files are taken: the
/// order Cargo 1.99.1 and the 2026-10-16 nightly take them (measured,
/// docs/safety.md, Lock files; 1.90 takes `.cargo-lock` alone). The order
/// is not what makes the protocol safe (holding every lock is, and diskgc
/// never waits for one, so it can be in no deadlock): it only spares a
/// Cargo that started first a brief wait, within one profile directory,
/// by making diskgc's first attempt there fail before diskgc holds
/// anything that Cargo will wait for.
pub const LOCK_ORDER: [&str; 3] = [".cargo-build-lock", ".cargo-lock", ".cargo-artifact-lock"];

/// What [`reclaim`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimed {
    /// Entries unlinked (the profile directories' entries and everything
    /// below them; never a lock file or a profile directory).
    pub removed_entries: usize,
    /// Their allocated bytes as the walks recorded them (an estimate; the
    /// measure is the observed change in available space).
    pub removed_bytes: u64,
    /// How long the target's locks were held, from the first one taken to
    /// their release (`None`: none was taken). Every Cargo of this target
    /// waits that long (`Blocking waiting for file lock`).
    pub locks_held: Option<Duration>,
    pub end: ReclaimEnd,
}

/// How [`reclaim`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimEnd {
    /// Every entry of every locked profile directory but the lock files
    /// was deleted.
    Complete,
    /// A skip of the target, with its reason: before anything was deleted
    /// (`locked`, `changed`, `in_use`, `not_idle`, ...), or part-way
    /// (`changed`, `in_use`; `removed_entries` says how far it got, the
    /// fingerprints being gone first).
    Skipped(Skip),
    /// The engine's delete of an entry failed (`ENOTEMPTY`: something was
    /// created in it after its walk; `EACCES`, ...): an error for this
    /// target, part-way.
    Failed { path: PathBuf, error: HostError },
    /// The pre-release sandbox gate refused (docs/safety.md): an error of
    /// the whole run, never a skip; every later deletion is refused too.
    GateRefused { path: PathBuf, error: HostError },
    /// An error of the run: the deadline passed, or a host failure that
    /// says nothing about this target (`EIO`, `EMFILE`, a scan slower than
    /// the freshness bound, ...).
    Stopped(Stop),
}

/// Runs the reclaim protocol (see the module docs) on `target`, found by
/// [`super::discover`], for the walk `rules` (their owners and IPC policy;
/// `max_entries` is the budget for the whole target) and the term's idle
/// requirement `idle_for`. `judged_last_build` is the last build the
/// caller chose the target by (its [`super::eligibility`], a plan), if
/// any: a newer one under the locks is `changed`. `checker` must have been
/// built from `rules` ([`Checker::new`]); its scans serve the in-use checks
/// (a fresh one is made under the locks). Starts no operation after
/// `deadline`.
pub fn reclaim<H: Delete>(
    host: &H,
    target: &Target,
    judged_last_build: Option<Timestamp>,
    rules: &Rules<'_>,
    idle_for: Option<IdleFor>,
    checker: &mut Checker<'_>,
    deadline: Option<MonoTime>,
) -> Reclaimed {
    let mut run = Run {
        host,
        target,
        checker,
        deadline,
        removed_entries: 0,
        removed_bytes: 0,
        locked_at: None,
    };
    let end = match run.protocol(judged_last_build, rules, idle_for) {
        Ok(()) => ReclaimEnd::Complete,
        Err(end) => end,
    };
    // `protocol` has returned, so every lock it took is closed by now.
    let locks_held = run
        .locked_at
        .map(|first| host.now().saturating_since(first));
    Reclaimed {
        removed_entries: run.removed_entries,
        removed_bytes: run.removed_bytes,
        locks_held,
        end,
    }
}

/// One run of the protocol on one target.
struct Run<'r, 'c, H: Delete> {
    host: &'r H,
    target: &'r Target,
    checker: &'r mut Checker<'c>,
    deadline: Option<MonoTime>,
    removed_entries: usize,
    removed_bytes: u64,
    /// When the first lock was taken.
    locked_at: Option<MonoTime>,
}

/// A judgement as the end of the protocol.
fn step<T>(judged: Judged<T>) -> Result<T, ReclaimEnd> {
    match judged {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(skip)) => Err(ReclaimEnd::Skipped(skip)),
        Err(stop) => Err(ReclaimEnd::Stopped(stop)),
    }
}

/// One part of the deletion: an entry of a profile directory (or, for the
/// per-unit fingerprints, a part split off one) with its own record.
#[derive(Debug)]
struct Part {
    /// Its profile directory's index in the walks.
    walk: usize,
    /// Where its parent directory is below the profile directory, when it
    /// is not the profile directory itself, with the identity the walk
    /// recorded for it.
    within: Option<(PathBuf, Identity)>,
    path: PathBuf,
    record: WalkRecord,
}

impl<H: Delete> Run<'_, '_, H> {
    fn protocol(
        &mut self,
        judged_last_build: Option<Timestamp>,
        rules: &Rules<'_>,
        idle_for: Option<IdleFor>,
    ) -> Result<(), ReclaimEnd> {
        let (host, target, deadline) = (self.host, self.target, self.deadline);
        let path = &target.path;
        let wanted = Options::from_rules(rules);
        let have = self.checker.options();
        if have.users != wanted.users || have.sockets != wanted.sockets {
            return Err(ReclaimEnd::Stopped(Stop::Failed {
                path: path.clone(),
                error: HostError::invalid(
                    "in-use check",
                    path.display(),
                    "the checker's options disagree with the walk's rules; build it with \
                     Checker::new from the same rules",
                ),
            }));
        }
        // The checkout path rule, as the source profile says, on the
        // target as found (a path check: before any lock is taken; the
        // same order as eligibility, so both give the same reason).
        judge_checkout_paths(target, rules.checkouts).map_err(ReclaimEnd::Skipped)?;
        // Only a layout diskgc knows how to lock and reclaim is locked: an
        // unknown lock name would not be taken.
        judge_layout(path, &target.inspected).map_err(ReclaimEnd::Skipped)?;
        let reached = step(reach(host, target, deadline))?;

        // Step 1: every lock of every profile directory, in Cargo's order.
        // Held until this function returns (step 4).
        let mut locks: Vec<Locked<H::Fd>> = Vec::new();
        let mut dirs = Vec::new();
        for profile in &target.inspected.profiles {
            let at = path.join(&profile.rel);
            let dir = step(open_profile(host, &reached.target, &at, profile, deadline))?;
            let mut ordered: Vec<&LockFile> = profile.locks.iter().collect();
            ordered.sort_by_key(|lock| lock_rank(&lock.name));
            for lock in ordered {
                locks.push(step(lock_file(host, &dir, &at, lock, deadline))?);
                if self.locked_at.is_none() {
                    self.locked_at = Some(host.now());
                }
            }
            dirs.push(dir);
        }

        // Step 2: recheck under the locks.
        let inspected = step(inspect(
            host,
            &reached.target.fd,
            reached.target.mount.id,
            path,
            deadline,
        ))?;
        judge_inspected(target, &inspected, rules).map_err(ReclaimEnd::Skipped)?;
        let mut budget = Budget::new(rules.max_entries);
        let mut walks = Vec::new();
        let mut walked: Vec<&ProfileDir> = Vec::new();
        // The same profiles in the same order as found (judge_inspected).
        for (profile, dir) in inspected.profiles.iter().zip(dirs) {
            if profile.is_empty() {
                continue;
            }
            let at = path.join(&profile.rel);
            let entries = step(walk_entries(
                host,
                &dir,
                &at,
                profile,
                rules,
                &mut budget,
                deadline,
            ))?;
            walks.push(ProfileWalk {
                dir,
                path: at,
                entries,
            });
            walked.push(profile);
        }
        let last = last_build(&inspected, &walks);
        if let Some(judged) = judged_last_build
            && last.is_none_or(|last| last > judged)
        {
            return Err(ReclaimEnd::Skipped(Skip::new(
                Reason::Changed,
                path.clone(),
                format!(
                    "built since it was judged: last build now {}, when judged {}",
                    last.map_or("unknown".to_owned(), seconds),
                    seconds(judged)
                ),
            )));
        }
        judge_idle(host, path, last, idle_for).map_err(ReclaimEnd::Skipped)?;
        // A scan made now, under the locks: one made before them could
        // miss a process started meanwhile.
        self.checker.forget();
        let scan = self
            .checker
            .current(host)
            .map_err(|error| failed(path, error))?;
        judge_in_use(scan, &walks, target, &inspected, reached.project)
            .map_err(ReclaimEnd::Skipped)?;

        // Step 3: delete, fingerprints first.
        for part in plan(&mut walks) {
            let walk = &walks[part.walk];
            check_deadline(host, deadline, &part.path).map_err(ReclaimEnd::Stopped)?;
            step(locks_unchanged(host, walk, walked[part.walk], deadline))?;
            let unit;
            let parent = match &part.within {
                None => &walk.dir,
                Some((rel, identity)) => {
                    unit = step(open_within(host, walk, rel, identity))?;
                    &unit
                }
            };
            let candidate =
                Candidate::new(parent, &part.path).expect("a part's path ends in its name");
            self.delete_part(&candidate, &part.record, &inspected, reached.project)?;
        }
        // Step 4: the locks are released when `locks` is dropped here.
        drop(locks);
        Ok(())
    }

    /// Deletes one part, right after its own in-use check and the Cargo
    /// heuristics on the same scan, through [`delete_cleared`] with the
    /// record that was checked. A clear that expired before the delete
    /// could start is checked again once, against a new scan.
    fn delete_part(
        &mut self,
        candidate: &Candidate<'_, H::Fd>,
        record: &WalkRecord,
        inspected: &Inspected,
        project: Project,
    ) -> Result<(), ReclaimEnd> {
        let (host, target) = (self.host, self.target);
        let path = candidate.path();
        for attempt in 0..2 {
            let clear = match self.checker.check(host, candidate, record) {
                Ok(Ok(clear)) => clear,
                Ok(Err(skip)) => return Err(ReclaimEnd::Skipped(skip)),
                Err(error) => return Err(failed(path, error)),
            };
            let scan = self.checker.latest().expect("the check made or kept one");
            let evidence =
                cargo_evidence(scan, &target.path, target.identity.id, inspected, project);
            if !evidence.is_empty() {
                return Err(ReclaimEnd::Skipped(Skip::new(
                    Reason::InUse,
                    target.path.clone(),
                    evidence.join("; "),
                )));
            }
            let options = DeleteOptions {
                make_writable: false,
                deadline: self.deadline,
            };
            match delete_cleared(host, clear, candidate, record, options) {
                Ok(done) => {
                    self.removed_entries += done.removed_entries;
                    self.removed_bytes = self.removed_bytes.saturating_add(done.removed_bytes);
                    return match done.end {
                        End::Complete => Ok(()),
                        End::Changed { path, detail } => Err(ReclaimEnd::Skipped(Skip::new(
                            Reason::Changed,
                            path,
                            detail,
                        ))),
                        End::DeadlinePassed { path } => {
                            Err(ReclaimEnd::Stopped(Stop::DeadlinePassed { path }))
                        }
                        End::Failed { path, error } => Err(ReclaimEnd::Failed { path, error }),
                        End::GateRefused { path, error } => {
                            Err(ReclaimEnd::GateRefused { path, error })
                        }
                    };
                }
                Err(NotCleared::Expired(_)) if attempt == 0 => self.checker.forget(),
                Err(e) => {
                    return Err(failed(
                        path,
                        HostError::invalid(
                            "in-use check",
                            path.display(),
                            format!("the clear could not be used: {e:?}"),
                        ),
                    ));
                }
            }
        }
        Err(failed(
            path,
            HostError::invalid(
                "in-use check",
                path.display(),
                "a fresh check expired before the deletion could start, twice",
            ),
        ))
    }
}

/// `t` as seconds with nanoseconds, for messages.
fn seconds(t: Timestamp) -> String {
    format!("{}.{:09} s", t.secs, t.nanos)
}

/// An error of the run at `path`.
fn failed(path: &Path, error: HostError) -> ReclaimEnd {
    ReclaimEnd::Stopped(Stop::Failed {
        path: path.to_owned(),
        error,
    })
}

/// A lock name's place in [`LOCK_ORDER`]; a name not in it (never: only
/// known names pass [`judge_layout`]) is taken last.
fn lock_rank(name: &OsStr) -> usize {
    LOCK_ORDER
        .iter()
        .position(|known| name == *known)
        .unwrap_or(LOCK_ORDER.len())
}

/// Takes the lock file `lock` of the profile directory open as `dir` (at
/// `profile_path`): opened `O_PATH` below it, without following a symlink
/// or crossing a mount; it must be the lock file found (identity, type
/// included: `changed` otherwise); then reopened read-only through that fd
/// (never by name, so nothing but that regular file is ever opened) and
/// `flock`ed exclusively without waiting (`locked` when someone else holds
/// it, shared or exclusive).
fn lock_file<H: Host>(
    host: &H,
    dir: &Resolved<H::Fd>,
    profile_path: &Path,
    lock: &LockFile,
    deadline: Option<MonoTime>,
) -> Judged<Locked<H::Fd>> {
    let at = profile_path.join(&lock.name);
    check_deadline(host, deadline, &at)?;
    let skip = |reason, detail: String| Ok(Err(Skip::new(reason, at.clone(), detail)));
    let (fd, stat) = match open_beneath(
        host,
        &dir.fd,
        dir.mount.id,
        Path::new(&lock.name),
        Access::Path,
        Want::Any,
    ) {
        Ok(opened) => opened,
        Err(ResolveError::Host(e)) => return failure(at.clone(), "open", &e),
        Err(
            e @ ResolveError::UnsafePath {
                why: Unsafe::CrossesMount | Unsafe::LeftMount { .. },
                ..
            },
        ) => return skip(Reason::ContainsMount, e.to_string()),
        Err(e @ ResolveError::UnsupportedFs { .. }) => {
            return skip(Reason::UnsupportedFs, e.to_string());
        }
        Err(e) => return skip(Reason::Changed, format!("not the lock file found: {e}")),
    };
    let now = Identity::of(&stat);
    if now != Some(lock.identity) {
        return skip(
            Reason::Changed,
            format!(
                "not the lock file found (found {:?}, now {now:?})",
                lock.identity
            ),
        );
    }
    let readable = match host.reopen_regular(&fd) {
        Ok(readable) => readable,
        Err(e) => return failure(at.clone(), "reopen", &e),
    };
    drop(fd);
    match host.flock_try(readable, LockMode::Exclusive) {
        Ok(TryLock::Acquired(locked)) => Ok(Ok(locked)),
        Ok(TryLock::Busy(_)) => skip(
            Reason::Locked,
            "held by another process (a Cargo building, checking, running or testing this \
             target)"
                .to_owned(),
        ),
        Err(e) => failure(at.clone(), "flock", &e),
    }
}

/// The lock files of the profile directory of `walk` (whose locks diskgc
/// holds, `locked` as found) listed again: any created, removed or
/// replaced since they were taken is `changed`. A Cargo whose lock set
/// differs (a 1.99.1 build on a target 1.90 built creates
/// `.cargo-build-lock` and waits for `.cargo-lock`) is starting.
fn locks_unchanged<H: Host>(
    host: &H,
    walk: &ProfileWalk<H::Fd>,
    locked: &ProfileDir,
    deadline: Option<MonoTime>,
) -> Judged<()> {
    let names = judged!(list(host, &walk.dir.fd, &walk.path, deadline));
    let now = judged!(profile_dir(
        host,
        &walk.dir.fd,
        locked.rel.clone(),
        &walk.path,
        &walk.dir.stat,
        names
    ));
    if now.same_locks(locked) {
        Ok(Ok(()))
    } else {
        Ok(Err(Skip::new(
            Reason::Changed,
            walk.path.clone(),
            format!(
                "a lock file was created, removed or replaced while diskgc held the locks (a \
                 Cargo with another lock set is starting): held {:?}, now {:?}",
                names_of(locked),
                names_of(&now)
            ),
        )))
    }
}

fn names_of(profile: &ProfileDir) -> Vec<&OsStr> {
    profile.locks.iter().map(|l| l.name.as_os_str()).collect()
}

/// The directory `rel` below the profile directory of `walk`, which the
/// walk recorded as `identity`: opened below it and matched (`changed`
/// otherwise), as the parent of a part split off an entry's record.
fn open_within<H: Host>(
    host: &H,
    walk: &ProfileWalk<H::Fd>,
    rel: &Path,
    identity: &Identity,
) -> Judged<Resolved<H::Fd>> {
    let at = walk.path.join(rel);
    let (fd, stat) = judged!(open_dir(host, &walk.dir.fd, walk.dir.mount.id, rel, &at));
    if Identity::of(&stat) != Some(*identity) {
        return Ok(Err(Skip::new(
            Reason::Changed,
            at,
            "not the directory the walk recorded (replaced in between)",
        )));
    }
    Ok(Ok(Resolved {
        fd,
        mount: walk.dir.mount.clone(),
        stat,
    }))
}

/// The order of the deletion: first the fingerprints of every profile
/// directory (so that a profile deleted part-way always rebuilds), in
/// whatever layout or mix of layouts it holds (a target that Cargo 1.99.1
/// and the nightly both built has `.fingerprint/` and `deps/` and
/// per-unit units in `build/`): the entry `.fingerprint/` if there is one,
/// and every `build/<package>/<hash>/fingerprint`, each split off the
/// record of `build/` as a part of its own; then, per profile directory,
/// every other entry in name order with `build/` and `deps/` last (the
/// directories that tell the layout, so that a profile stopped part-way is
/// still recognised). Takes the records out of `walks`.
fn plan<F>(walks: &mut [ProfileWalk<F>]) -> Vec<Part> {
    let mut fingerprints = Vec::new();
    let mut rest = Vec::new();
    for (i, walk) in walks.iter_mut().enumerate() {
        let mut entries = std::mem::take(&mut walk.entries);
        if let Some(at) = entries
            .iter()
            .position(|(_, record)| record.root().name == FINGERPRINT_DIR)
        {
            let (path, record) = entries.remove(at);
            fingerprints.push(Part {
                walk: i,
                within: None,
                path,
                record,
            });
        }
        if let Some((build_path, build)) = entries
            .iter_mut()
            .find(|(_, record)| record.root().name == BUILD_DIR)
        {
            for rel in unit_fingerprints(build) {
                let unit = rel.parent().expect("a unit's fingerprint has a parent");
                let identity = build.entry_at(unit).expect("found in the record").identity;
                let record = build.split_off(&rel).expect("found in the record");
                fingerprints.push(Part {
                    walk: i,
                    within: Some((Path::new(BUILD_DIR).join(unit), identity)),
                    path: build_path.join(&rel),
                    record,
                });
            }
        }
        // Stable: name order otherwise.
        entries.sort_by_key(|(_, record)| marker_rank(&record.root().name));
        rest.extend(entries.into_iter().map(|(path, record)| Part {
            walk: i,
            within: None,
            path,
            record,
        }));
    }
    fingerprints.extend(rest);
    fingerprints
}

/// The classic layout's fingerprint directory.
const FINGERPRINT_DIR: &str = ".fingerprint";
/// The per-unit layout's units directory (`build/<package>/<hash>/`), also
/// the classic layout's build-script directory (whose units hold no
/// `fingerprint`; splitting one off there would only delete it earlier).
const BUILD_DIR: &str = "build";
/// The classic layout's dependency directory.
const DEPS_DIR: &str = "deps";
/// A per-unit layout unit's fingerprint, `build/<package>/<hash>/fingerprint`.
const UNIT_FINGERPRINT: &str = "fingerprint";

/// Deleted last, in this order: what tells a profile directory's layout
/// ([`ProfileDir::layout`]).
fn marker_rank(name: &OsStr) -> u8 {
    if name == BUILD_DIR {
        1
    } else if name == DEPS_DIR {
        2
    } else {
        0
    }
}

/// `<package>/<hash>/fingerprint` of every unit in the walk record of a
/// profile directory's `build/`, whatever its layout (a classic profile
/// that the nightly also built holds per-unit units there too), in record
/// (name) order.
fn unit_fingerprints(build: &WalkRecord) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for package in build.listing_at(Path::new("")).unwrap_or_default() {
        let package = Path::new(&package.name);
        for unit in build.listing_at(package).unwrap_or_default() {
            let unit = package.join(&unit.name);
            let has = build
                .listing_at(&unit)
                .unwrap_or_default()
                .iter()
                .any(|e| e.name == UNIT_FINGERPRINT);
            if has {
                found.push(unit.join(UNIT_FINGERPRINT));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::CARGO_LOCK_NAMES;

    /// Every known lock name has its place in the order, and nothing else
    /// does: a name missing there would be taken last, one added to the
    /// allowlist must be placed by a measurement.
    #[test]
    fn the_lock_order_is_the_allowlist_in_cargos_order() {
        let mut order = LOCK_ORDER.to_vec();
        let mut known = CARGO_LOCK_NAMES.to_vec();
        order.sort_unstable();
        known.sort_unstable();
        assert_eq!(order, known);
        assert!(lock_rank(OsStr::new(".cargo-build-lock")) < lock_rank(OsStr::new(".cargo-lock")));
        assert!(
            lock_rank(OsStr::new(".cargo-lock")) < lock_rank(OsStr::new(".cargo-artifact-lock"))
        );
    }

    #[test]
    fn the_layout_markers_go_last() {
        let mut names = ["deps", "build", "a", ".fingerprint", "z"];
        names.sort_by_key(|n| marker_rank(OsStr::new(n)));
        assert_eq!(names, ["a", ".fingerprint", "z", "build", "deps"]);
    }
}
