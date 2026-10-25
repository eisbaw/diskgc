//! Discovery of `fs` candidates (docs/safety.md, Filesystem candidates:
//! Before the walk; docs/reference.md, Profiles: `find`): what a source
//! profile's patterns find below its roots, each candidate with the root it
//! was found under and its identity. It judges nothing about a candidate's
//! contents: that is [`super::judge`], which the dry run ([`super::plan`])
//! and the reclaim ([`super::reclaim`]) share, each on the candidates
//! discovery found.
//!
//! The walk of a root goes one level per pattern segment: a directory is
//! listed only when an active pattern has a `*` at that level, otherwise
//! its literal names are looked up directly; only names that match some
//! active pattern are ever `statx`ed. Below a root it never follows a
//! symlink (`statx` does not follow, directories are opened with
//! `open_beneath`), never crosses a mount, and never descends into a
//! candidate (so a candidate inside another one of the same profile is not
//! found: the outer one is, docs/reference.md).
//!
//! **Mounts first.** `mountinfo` is read before anything is `statx`ed: a
//! root is refused by its mount's line (a network filesystem, a bind mount
//! of a `systemd-private-*` directory) before it is opened; a name that is
//! a mount point is never `statx`ed (a dead FUSE mount cannot hang the
//! walk), and as a candidate it is `contains_mount`, like a candidate with
//! a mount inside, before the source's own `where` would look into it.
//! `systemd-private-*` is never a candidate and never entered.
//!
//! Per candidate, in this order: its identity (a type, inode or owner the
//! kernel does not report is `unsupported_fs`), found once (a candidate
//! reached again through another root or pattern is listed once, under the
//! outermost root: roots are walked outermost first), a mount at or inside
//! it, then the source's own `where` (`has` / `has_dir` / `has_not` / `lacks_entry`), which decides
//! whether it is a candidate at all.
//!
//! The deadline is checked before every `statx`, after every listing
//! (whatever it returned), before the `where` predicates' I/O, and once
//! more before discovery reports that it is complete.
//!
//! **Resumable** ([`DiscoverCursor`], the background tally's, Mark
//! 2026-10-25). The walk keeps its position as names only: per directory
//! on the way down, its name, its identity, the patterns still active, its
//! sorted names (once listed) and how many are done. [`discover`] runs it
//! to the end in one call; [`DiscoverCursor::step`] runs it until a pause
//! callback says stop (checked before every listing and every `statx`,
//! never inside a candidate's checks) and holds no file descriptor between
//! steps. Each step reaches the root again from its mount and reopens the
//! directories on the way by name: one that is gone ends its branch; one
//! replaced by another inode (or a root replaced) is refused (`changed`),
//! so a listing over it is never complete. Names created in a directory
//! after it was listed are not found by this walk.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::judge::{
    check_root, mounted_at_or_inside, skip_or_stop, source_where, unreachable_root,
};
use super::source_anchor::{self, SourceEvidence};
use super::{Find, is_private_tmp};
use crate::engine::{DEFAULT_MAX_ENTRIES, Identity, Reason, Seen, Skip};
use crate::host::{Base, DevIno, DirListing, Errno, FileType, Host, MonoTime, MountEntry, Stat};
use crate::outcome::Stop;
use crate::profile::EntryType;
use crate::resolve::{ResolveError, Resolved, Want, open_beneath, resolve_in};

/// A candidate as discovery found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The root it was found under (absolute, `{uid}` replaced): the path
    /// rule of the checkout guard and the keep markers count from here
    /// down, and the reclaim reaches it again from here.
    pub root: PathBuf,
    /// Its path: the root and the names that led to it.
    pub path: PathBuf,
    /// Its identity when found (`statx`, not followed). Reached again, it
    /// must be the same object (`changed` otherwise).
    pub identity: Identity,
    /// The recorded source that proved this output rebuildable when found.
    pub source_evidence: Option<SourceEvidence>,
    /// When it was found (after its `statx`): a liveness contract's scan
    /// must start no earlier (Owner PID's ordering).
    pub found_at: MonoTime,
    /// What the index needs of the `statx` that found it (the tally,
    /// docs/architecture.md): the inode of the directory it was found in
    /// and `max(mtime, ctime)` of its root.
    pub seen: Seen,
}

/// What [`discover`] found.
#[derive(Debug, Default)]
pub struct Discovery {
    /// Every candidate, in the order found: to be judged, or refused by
    /// discovery already (a mount point or a mount inside, never looked
    /// into; an identity the kernel does not report).
    pub candidates: Vec<(PathBuf, Result<Found, Skip>)>,
    /// Structural matches of a guarding union source, recorded before
    /// source-owned evidence or anchors decide action admission.
    pub guard_candidates: Vec<Found>,
    /// Roots and directories on the way that could not be read or must not
    /// be (a missing root, a root on a network filesystem or reached through
    /// a symlink, an unreadable directory, a candidate whose own `where`
    /// cannot be told), each with its reason.
    pub refused: Vec<Skip>,
    /// Candidates found again (another root or pattern reached the same
    /// object): listed once.
    pub duplicates: Vec<PathBuf>,
    /// `systemd-private-*` names a pattern matched, and roots inside one
    /// (by their path, or a bind mount of one by their mount's root; those
    /// are in `refused` too, as `unsafe_path`): never searched, never
    /// candidates.
    pub private_tmp: usize,
}

/// Finds the candidates of `find` below `roots` (absolute, `{uid}`
/// replaced); see the module docs. Errors of the run (a failing disk, no
/// fds left, the deadline) stop it: they are returned with what was found
/// until then. Anything about one root, directory or candidate is in the
/// result.
pub fn discover<H: Host>(
    host: &H,
    roots: &[PathBuf],
    find: &Find,
    deadline: Option<MonoTime>,
) -> (Discovery, Option<Stop>) {
    let mut out = Discovery::default();
    let stopped = discover_into(host, roots, find, deadline, false, &mut out).err();
    (out, stopped)
}

/// Discover admitted candidates and structural matches of any union term
/// with positive retention guards. The latter includes guards from term
/// filters even when source-owned evidence rejects action admission.
pub fn discover_union<H: Host>(
    host: &H,
    roots: &[PathBuf],
    find: &Find,
    deadline: Option<MonoTime>,
) -> (Discovery, Option<Stop>) {
    let mut out = Discovery::default();
    let stopped = discover_into(
        host,
        roots,
        find,
        deadline,
        find.has_retention_guard(),
        &mut out,
    )
    .err();
    (out, stopped)
}

fn discover_into<H: Host>(
    host: &H,
    roots: &[PathBuf],
    find: &Find,
    deadline: Option<MonoTime>,
    record_structural: bool,
    out: &mut Discovery,
) -> Result<(), Stop> {
    let mounts = host.mountinfo().map_err(|error| Stop::Failed {
        path: PathBuf::from("/proc/self/mountinfo"),
        error,
    })?;
    let mut seen = HashSet::new();
    let mut seen_roots = HashSet::new();
    let mut ordered: Vec<&PathBuf> = roots.iter().collect();
    ordered.sort_by_key(|root| root.components().count());
    for root in ordered {
        check_deadline(host, deadline, root)?;
        match check_root(root, &mounts) {
            Ok(()) => {}
            Err(Ok(why)) => {
                // A misconfigured root (an error that excludes the tier),
                // reported as the reach of a found candidate reports it.
                out.private_tmp += 1;
                out.refused.push(private_root(root, &why));
                continue;
            }
            Err(Err(skip)) => {
                out.refused.push(skip);
                continue;
            }
        }
        let resolved = match resolve_in(host, &mounts, root, Want::Directory) {
            Ok(resolved) => resolved,
            Err(e) => {
                out.refused.push(unreachable_root(root, e)?);
                continue;
            }
        };
        // The outermost spelling wins. A second name for the same directory
        // (including a bind mount alias) must not cause another walk.
        let Some(identity) = resolved.stat.dev_ino() else {
            out.refused.push(Skip::new(
                Reason::UnsupportedFs,
                root.to_owned(),
                "the root inode identity was not reported; overlapping roots cannot be deduplicated",
            ));
            continue;
        };
        if !seen_roots.insert(identity) {
            continue;
        }
        let mut cursor = DiscoverCursor::new(root.to_owned(), record_structural);
        cursor.root_id = Some(identity);
        cursor.frames.push(Frame::root(find));
        cursor.seen = std::mem::take(&mut seen);
        let walked = cursor.walk(
            host,
            find,
            &mounts,
            deadline,
            out,
            vec![resolved],
            &mut |_| false,
        );
        seen = std::mem::take(&mut cursor.seen);
        walked?;
    }
    // A walk whose last step crossed the deadline is not complete.
    check_deadline(host, deadline, roots.first().map_or(Path::new("/"), |r| r))?;
    Ok(())
}

/// A root inside systemd's PrivateTmp (`why`): `unsafe_path`, a
/// misconfiguration, never searched.
pub(super) fn private_root(root: &Path, why: &str) -> Skip {
    Skip::new(
        Reason::UnsafePath,
        root.to_owned(),
        format!("{why}: never searched, its candidates never reached"),
    )
}

pub(super) fn check_deadline<H: Host>(
    host: &H,
    deadline: Option<MonoTime>,
    path: &Path,
) -> Result<(), Stop> {
    if deadline.is_some_and(|d| host.now() >= d) {
        return Err(Stop::DeadlinePassed {
            path: path.to_owned(),
        });
    }
    Ok(())
}

fn type_matches(wanted: EntryType, found: FileType) -> bool {
    match wanted {
        EntryType::Dir => found == FileType::Directory,
        EntryType::File => found == FileType::Regular,
        EntryType::Any => matches!(found, FileType::Directory | FileType::Regular),
    }
}

/// A directory on the walk's way down (see the module doc, Resumable).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    /// Its name in its parent (`None`: the root).
    name: Option<OsString>,
    /// Names below the root.
    level: usize,
    /// The patterns (indices) still matching on the way down.
    active: Vec<usize>,
    /// Its names to look at, sorted, once listed.
    names: Option<Vec<OsString>>,
    /// How many of `names` are done.
    next: usize,
    /// Its identity when it was entered (`None`: the root before its
    /// first step).
    id: Option<DevIno>,
}

impl Frame {
    fn root(find: &Find) -> Self {
        Self {
            name: None,
            level: 0,
            active: (0..find.patterns.len()).collect(),
            names: None,
            next: 0,
            id: None,
        }
    }
}

/// What one [`DiscoverCursor::step`] came to.
#[derive(Debug)]
pub enum Stepped {
    /// The pause callback stopped it; call again to go on.
    Paused,
    /// The walk reached its end.
    Done,
}

/// A resumable discovery of one root (see the module doc, Resumable). Holds
/// names, never a file descriptor.
#[derive(Debug, Clone)]
pub struct DiscoverCursor {
    root: PathBuf,
    record_structural: bool,
    /// The root's identity, once reached.
    root_id: Option<DevIno>,
    frames: Vec<Frame>,
    /// Candidates found so far, by identity (found once).
    seen: HashSet<DevIno>,
}

impl DiscoverCursor {
    /// A discovery of `root` (absolute, `{uid}` replaced) by `find`.
    pub fn new(root: PathBuf, record_structural: bool) -> Self {
        Self {
            root,
            record_structural,
            root_id: None,
            frames: Vec::new(),
            seen: HashSet::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// An upper bound of the heap the cursor owns, as the index counts its
    /// own (`crate::index::memory`): the kept listings, the frames and the
    /// set of candidates found.
    pub fn counted_bytes(&self) -> usize {
        use crate::index::memory::{heap, vec_bytes};
        use std::mem::size_of;
        let name = |n: &OsString| heap(n.capacity());
        heap(self.root.capacity())
            + vec_bytes(self.frames.capacity(), size_of::<Frame>())
            + self
                .frames
                .iter()
                .map(|f| {
                    f.name.as_ref().map_or(0, name)
                        + vec_bytes(f.active.capacity(), size_of::<usize>())
                        + f.names.as_ref().map_or(0, |names| {
                            vec_bytes(names.capacity(), size_of::<OsString>())
                                + names.iter().map(name).sum::<usize>()
                        })
                })
                .sum::<usize>()
            // A swiss table: a control byte and the entry per bucket, at
            // most twice the entries in buckets.
            + heap(self.seen.capacity().max(1) * 2 * (size_of::<DevIno>() + 1))
    }

    /// Walks on, adding what it finds to `out`, until `pause` (asked with
    /// the operations about to be made, before every listing and every
    /// `statx`) says stop, or the end. Refusals go to `out.refused`. An
    /// error of the run (a failing disk, no fds left) stops it; the cursor
    /// must then be dropped.
    pub fn step<H: Host>(
        &mut self,
        host: &H,
        find: &Find,
        out: &mut Discovery,
        pause: &mut dyn FnMut(usize) -> bool,
    ) -> Result<Stepped, Stop> {
        if self.frames.is_empty() && self.root_id.is_some() {
            return Ok(Stepped::Done);
        }
        let mounts = host.mountinfo().map_err(|error| Stop::Failed {
            path: PathBuf::from("/proc/self/mountinfo"),
            error,
        })?;
        let root = self.root.clone();
        match check_root(&root, &mounts) {
            Ok(()) => {}
            Err(Ok(why)) => {
                out.private_tmp += 1;
                out.refused.push(private_root(&root, &why));
                self.finish();
                return Ok(Stepped::Done);
            }
            Err(Err(skip)) => {
                out.refused.push(skip);
                self.finish();
                return Ok(Stepped::Done);
            }
        }
        let resolved = match resolve_in(host, &mounts, &root, Want::Directory) {
            Ok(resolved) => resolved,
            Err(e) => {
                out.refused.push(unreachable_root(&root, e)?);
                self.finish();
                return Ok(Stepped::Done);
            }
        };
        let Some(identity) = resolved.stat.dev_ino() else {
            out.refused.push(Skip::new(
                Reason::UnsupportedFs,
                root.clone(),
                "the root inode identity was not reported",
            ));
            self.finish();
            return Ok(Stepped::Done);
        };
        match self.root_id {
            None => {
                self.root_id = Some(identity);
                self.frames.push(Frame::root(find));
            }
            Some(id) if id != identity => {
                out.refused.push(Skip::new(
                    Reason::Changed,
                    root.clone(),
                    "the root was replaced while it was listed across batches",
                ));
                self.finish();
                return Ok(Stepped::Done);
            }
            Some(_) => {}
        }
        // Reopen the way down by name.
        let mut open = vec![resolved];
        while open.len() < self.frames.len() {
            let depth = open.len();
            let frame = &self.frames[depth];
            let name = frame
                .name
                .clone()
                .expect("a frame below the root has a name");
            let at = self.path_of(depth);
            let parent = &open[depth - 1];
            let reopened = open_beneath(
                host,
                &parent.fd,
                parent.mount.id,
                Path::new(&name),
                crate::host::Access::Path,
                Want::Directory,
            );
            match reopened {
                Ok((fd, stat)) if stat.dev_ino() == frame.id => open.push(Resolved {
                    fd,
                    mount: parent.mount.clone(),
                    stat,
                }),
                Ok(_) => {
                    out.refused.push(Skip::new(
                        Reason::Changed,
                        at,
                        "replaced while it was listed across batches",
                    ));
                    self.frames.truncate(depth);
                }
                Err(ResolveError::Host(e)) if !e.is(Errno::ENOENT) => {
                    let skip = skip_or_stop(at, "open", &e)?;
                    out.refused.push(skip);
                    self.frames.truncate(depth);
                }
                // Gone: nothing of this root's is there any more.
                Err(_) => self.frames.truncate(depth),
            }
        }
        let walked = self.walk(host, find, &mounts, None, out, open, pause)?;
        Ok(walked)
    }

    fn finish(&mut self) {
        self.frames.clear();
        if self.root_id.is_none() {
            // Refused before it was reached: done all the same.
            self.root_id = Some(DevIno {
                dev: crate::host::Dev { major: 0, minor: 0 },
                ino: 0,
            });
        }
    }

    /// The path of the frame at `depth`.
    fn path_of(&self, depth: usize) -> PathBuf {
        let mut path = self.root.clone();
        for frame in &self.frames[1..=depth] {
            path.push(frame.name.as_ref().expect("named"));
        }
        path
    }

    /// The walk from the open frames `open` (one per frame).
    #[allow(clippy::too_many_arguments)]
    fn walk<H: Host>(
        &mut self,
        host: &H,
        find: &Find,
        mounts: &[MountEntry],
        deadline: Option<MonoTime>,
        out: &mut Discovery,
        mut open: Vec<Resolved<H::Fd>>,
        pause: &mut dyn FnMut(usize) -> bool,
    ) -> Result<Stepped, Stop> {
        assert_eq!(open.len(), self.frames.len(), "a frame per open directory");
        let root = self.root.clone();
        let mut seen = std::mem::take(&mut self.seen);
        let result = (|| -> Result<Stepped, Stop> {
            loop {
                let Some(depth) = self.frames.len().checked_sub(1) else {
                    return Ok(Stepped::Done);
                };
                let path = self.path_of(depth);
                let mut walk = Walk {
                    host,
                    root: &root,
                    find,
                    mounts,
                    deadline,
                    record_structural: self.record_structural,
                    out: &mut *out,
                    seen: &mut seen,
                };
                if self.frames[depth].names.is_none() {
                    if pause(1) {
                        return Ok(Stepped::Paused);
                    }
                    check_deadline(host, deadline, &path)?;
                    let frame = &self.frames[depth];
                    let names = walk.names(&open[depth], &path, frame.level, &frame.active);
                    // A listing of a huge directory can itself take long,
                    // and so can one that fails: checked whatever it
                    // returned.
                    check_deadline(host, deadline, &path)?;
                    match names? {
                        Ok(names) => self.frames[depth].names = Some(names),
                        Err(skip) => {
                            walk.out.refused.push(skip);
                            self.frames[depth].names = Some(Vec::new());
                        }
                    }
                }
                let frame = &self.frames[depth];
                let Some(name) = frame
                    .names
                    .as_ref()
                    .expect("listed")
                    .get(frame.next)
                    .cloned()
                else {
                    self.frames.pop();
                    open.pop();
                    continue;
                };
                let level = frame.level;
                let active = frame.active.clone();
                match walk.entry(&open[depth], &path, level, &active, &name, pause)? {
                    Entry::Paused => return Ok(Stepped::Paused),
                    Entry::Done => self.frames[depth].next += 1,
                    Entry::Descend { child, deeper } => {
                        self.frames[depth].next += 1;
                        let id = child.stat.dev_ino();
                        open.push(*child);
                        self.frames.push(Frame {
                            name: Some(name),
                            level: level + 1,
                            active: deeper,
                            names: None,
                            next: 0,
                            id,
                        });
                    }
                }
            }
        })();
        self.seen = seen;
        result
    }
}

/// What one name of a directory came to.
enum Entry<F> {
    /// The pause callback stopped before it: do it again next time.
    Paused,
    Done,
    /// A directory to walk next, with the patterns that go deeper.
    Descend {
        child: Box<Resolved<F>>,
        deeper: Vec<usize>,
    },
}

/// One step's view of the walk of one root.
struct Walk<'a, H: Host> {
    host: &'a H,
    /// The root being walked.
    root: &'a Path,
    find: &'a Find,
    mounts: &'a [MountEntry],
    deadline: Option<MonoTime>,
    record_structural: bool,
    out: &'a mut Discovery,
    seen: &'a mut HashSet<DevIno>,
}

impl<H: Host> Walk<'_, H> {
    fn is_mount_point(&self, at: &Path) -> bool {
        self.mounts.iter().any(|m| m.mount_point == at)
    }

    /// The entry `name` of the directory `dir` at `path`, `level` names
    /// below the root, with the patterns `active` (indices) still matching
    /// the way down.
    fn entry(
        &mut self,
        dir: &Resolved<H::Fd>,
        path: &Path,
        level: usize,
        active: &[usize],
        name: &OsStr,
        pause: &mut dyn FnMut(usize) -> bool,
    ) -> Result<Entry<H::Fd>, Stop> {
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
            return Ok(Entry::Done);
        }
        if is_private_tmp(name) {
            self.out.private_tmp += 1;
            return Ok(Entry::Done);
        }
        let at = path.join(name);
        // Exclusions remove only the matching candidate. A positive
        // pattern can still need this path as an intermediate directory.
        let excluded = !self.find.exclude.is_empty()
            && crate::fs_kind::path_matches(Some(&[self.root]), &self.find.exclude, &at);
        let has_deeper = matching
            .iter()
            .any(|&i| self.find.patterns[i].depth() > level + 1);
        if excluded && !has_deeper {
            return Ok(Entry::Done);
        }
        let ends = matching
            .iter()
            .any(|&i| self.find.patterns[i].depth() == level + 1);
        if self.is_mount_point(&at) {
            // Never statted: a dead mount would hang it.
            if ends && !excluded {
                self.out.candidates.push((
                    at.clone(),
                    Err(Skip::new(
                        Reason::ContainsMount,
                        at,
                        "a mount point (mountinfo): not touched",
                    )),
                ));
            }
            return Ok(Entry::Done);
        }
        // A candidate's checks are a handful of operations more; an
        // estimate, charged with its statx.
        if pause(if ends { 4 } else { 1 }) {
            return Ok(Entry::Paused);
        }
        // A big flat directory must not run past the deadline between
        // two candidates: checked before every `statx`.
        check_deadline(self.host, self.deadline, &at)?;
        let stat = match self.host.statx(Base::Dir(&dir.fd), Path::new(name)) {
            Ok(stat) => stat,
            Err(e) if e.is(Errno::ENOENT) => return Ok(Entry::Done),
            Err(e) => {
                let skip = skip_or_stop(at, "stat", &e)?;
                self.out.refused.push(skip);
                return Ok(Entry::Done);
            }
        };
        let (Some(file_type), Some(mnt_id)) = (stat.file_type, stat.mnt_id) else {
            self.out.refused.push(Skip::new(
                Reason::UnsupportedFs,
                at,
                format!("the kernel did not report its type and mount id: {stat:?}"),
            ));
            return Ok(Entry::Done);
        };
        if ends
            && !excluded
            && type_matches(self.find.entry_type, file_type)
            && self.candidate(dir, &at, name, &stat)?
        {
            // Never descended into: the outer candidate is the one.
            return Ok(Entry::Done);
        }
        let deeper: Vec<usize> = matching
            .into_iter()
            .filter(|&i| self.find.patterns[i].depth() > level + 1)
            .collect();
        if deeper.is_empty() || file_type != FileType::Directory || mnt_id != dir.mount.id {
            return Ok(Entry::Done);
        }
        let child = match open_beneath(
            self.host,
            &dir.fd,
            dir.mount.id,
            Path::new(name),
            crate::host::Access::Path,
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
                return Ok(Entry::Done);
            }
            // Gone, replaced by a symlink, or another mount: nothing of
            // this root's to find there.
            Err(_) => return Ok(Entry::Done),
        };
        // A checkout's metadata is still searched, so what is found
        // in it is reported (`contains_checkout`), never just missing.
        Ok(Entry::Descend {
            child: Box::new(child),
            deeper,
        })
    }

    /// The names to look at in `dir`, sorted: the whole listing when an
    /// active pattern has a `*` at this level, otherwise the literal names.
    fn names(
        &mut self,
        dir: &Resolved<H::Fd>,
        path: &Path,
        level: usize,
        active: &[usize],
    ) -> Result<Result<Vec<OsString>, Skip>, Stop> {
        let mut literals = Vec::new();
        let mut glob = false;
        for &i in active {
            match self.find.patterns[i].segment(level).map(|s| s.literal()) {
                Some(Some(name)) => literals.push(name.to_owned()),
                Some(None) => glob = true,
                None => {}
            }
        }
        if !glob {
            literals.sort();
            literals.dedup();
            return Ok(Ok(literals));
        }
        Ok(match self.host.read_dir(&dir.fd, DEFAULT_MAX_ENTRIES) {
            Ok(DirListing::Names(mut names)) => {
                names.sort();
                Ok(names)
            }
            Ok(DirListing::TooMany) => Err(Skip::new(
                Reason::TooManyEntries,
                path.to_owned(),
                format!("more than {DEFAULT_MAX_ENTRIES} entries: not listed"),
            )),
            Err(e) => Err(skip_or_stop(path.to_owned(), "list", &e)?),
        })
    }

    /// The entry `name` (at `at`, `stat` not followed) in `dir`, of the
    /// type wanted, at the end of a pattern: a candidate unless the
    /// source's own `where` says no. Recorded if it is one; `true` then
    /// (it is not descended into).
    fn candidate(
        &mut self,
        dir: &Resolved<H::Fd>,
        at: &Path,
        name: &OsStr,
        stat: &Stat,
    ) -> Result<bool, Stop> {
        let Some(identity) = Identity::of(stat) else {
            self.out.candidates.push((
                at.to_owned(),
                Err(Skip::new(
                    Reason::UnsupportedFs,
                    at.to_owned(),
                    format!("the kernel did not report its type, inode and owner: {stat:?}"),
                )),
            ));
            return Ok(true);
        };
        // Mounts first (docs/safety.md, Before the walk): the source's own
        // `where` looks inside the candidate, and a dead mount there would
        // hang it. Refused whatever `where` would say: it cannot be asked.
        if let Some(skip) = mounted_at_or_inside(self.mounts, at) {
            self.out.candidates.push((at.to_owned(), Err(skip)));
            return Ok(true);
        }
        if self.record_structural {
            self.out.guard_candidates.push(Found {
                root: self.root.to_owned(),
                path: at.to_owned(),
                identity,
                source_evidence: None,
                found_at: self.host.now(),
                seen: Seen::of(stat, &dir.stat),
            });
        }
        match source_where(
            self.host,
            self.find,
            dir,
            at,
            name,
            stat.file_type,
            self.deadline,
        )? {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(skip) => {
                self.out.refused.push(skip);
                return Ok(false);
            }
        }
        let source_evidence = if let Some(kind) = self.find.source_anchor {
            match source_anchor::check(
                self.host,
                source_anchor::AnchorCheck::new(kind, self.root, at, identity, self.deadline),
                dir,
                name,
            )? {
                Ok(Some(evidence)) => Some(evidence),
                Ok(None) => return Ok(false),
                Err(skip) => {
                    self.out.refused.push(skip);
                    return Ok(false);
                }
            }
        } else {
            None
        };
        // Dedupe only actual source matches. An earlier route to this inode
        // may fail `where` or its anchor while another root supplies the
        // source evidence needed to select it.
        if !self.seen.insert(identity.id) {
            self.out.duplicates.push(at.to_owned());
            return Ok(true);
        }
        self.out.candidates.push((
            at.to_owned(),
            Ok(Found {
                root: self.root.to_owned(),
                path: at.to_owned(),
                identity,
                source_evidence,
                found_at: self.host.now(),
                seen: Seen::of(stat, &dir.stat),
            }),
        ));
        Ok(true)
    }
}
