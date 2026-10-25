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
        let active: Vec<usize> = (0..find.patterns.len()).collect();
        let mut walk = Walk {
            host,
            root,
            find,
            mounts: &mounts,
            deadline,
            record_structural,
            out,
            seen: &mut seen,
        };
        walk.visit(&resolved, root, 0, &active)?;
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

/// The walk of one root.
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

    /// The directory `dir` at `path`, `level` names below the root, with
    /// the patterns `active` (indices) still matching the way down.
    fn visit(
        &mut self,
        dir: &Resolved<H::Fd>,
        path: &Path,
        level: usize,
        active: &[usize],
    ) -> Result<(), Stop> {
        check_deadline(self.host, self.deadline, path)?;
        let names = self.names(dir, path, level, active);
        // A listing of a huge directory can itself take long, and so can
        // one that fails: checked whatever it returned.
        check_deadline(self.host, self.deadline, path)?;
        let names = match names? {
            Ok(names) => names,
            Err(skip) => {
                self.out.refused.push(skip);
                return Ok(());
            }
        };
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
            if is_private_tmp(name) {
                self.out.private_tmp += 1;
                continue;
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
                continue;
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
                continue;
            }
            // A big flat directory must not run past the deadline between
            // two candidates: checked before every `statx`.
            check_deadline(self.host, self.deadline, &at)?;
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
                && !excluded
                && type_matches(self.find.entry_type, file_type)
                && self.candidate(dir, &at, name, &stat)?
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
                    continue;
                }
                // Gone, replaced by a symlink, or another mount: nothing of
                // this root's to find there.
                Err(_) => continue,
            };
            // A checkout's metadata is still searched, so what is found
            // in it is reported (`contains_checkout`), never just missing.
            self.visit(&child, &at, level + 1, &deeper)?;
        }
        Ok(())
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
