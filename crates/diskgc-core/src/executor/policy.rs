//! The `protect` and `!profile:` verdicts on one candidate: matches
//! enumerated by discovery, and overlap proved by path, by place in the
//! filesystem (mountinfo's root field) or by object identity below a
//! screened mount chain (docs/safety.md, Filesystem candidates). [`super::tier_policy`] caches these per tier for delete
//! actions.

use crate::cargo_target::{self, Stop};
use crate::engine::{Reason, Skip};
use crate::fs_kind;
use crate::host::{Dev, DevIno, Host, HostError, MonoTime, MountEntry};
use crate::resolve::{MountScreenError, Want, mount_for, resolve_in, screen_mount_chain};
use crate::select;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{NegatedSource, ProtectedKind, ProtectedSource, scan_negated};

/// Discover the complete raw match set. An unreadable or unsafe branch can
/// hide a protected descendant, so it excludes the run instead of yielding
/// a partial allow decision. A missing root is an empty match set.
pub(super) fn scan_protections<H: Host>(
    host: &H,
    protections: &[ProtectedSource],
    deadline: Option<MonoTime>,
) -> Result<Vec<PathBuf>, String> {
    let mut matches = Vec::new();
    for protection in protections {
        let mut refused = Vec::new();
        match &protection.kind {
            ProtectedKind::Fs(find) => {
                let (found, stopped) = fs_kind::discover(host, &protection.roots, find, deadline);
                if let Some(stop) = stopped {
                    return Err(format!(
                        "protect `{}` discovery stopped: {stop}",
                        protection.profile
                    ));
                }
                refused.extend(found.refused);
                for (path, candidate) in found.candidates {
                    match candidate {
                        Ok(_) => matches.push(path),
                        Err(skip) => refused.push(skip),
                    }
                }
            }
            ProtectedKind::CargoTarget => {
                let found =
                    cargo_target::discover(host, &protection.roots, deadline).map_err(|stop| {
                        format!("protect `{}` discovery stopped: {stop}", protection.profile)
                    })?;
                refused.extend(found.refused);
                matches.extend(found.targets.into_iter().map(|target| target.path));
            }
        }
        for skip in refused {
            if skip.reason == Reason::Missing && protection.roots.contains(&skip.path) {
                continue;
            }
            return Err(format!(
                "protect `{}` cannot prove matches at {}: {}",
                protection.profile,
                skip.path.display(),
                skip
            ));
        }
    }
    Ok(matches)
}

/// Compare names, places in the filesystem and object identity. A
/// same-device bind mount can expose a protected subtree under a different
/// name, so lexical prefixes alone are insufficient: [`overlaps_in_filesystem`]
/// compares where each path is in its filesystem, which covers a bind of any
/// directory (TASK-260); [`overlaps_with`] compares ancestor identities, which
/// also catches another name of a protected file itself (a hard link).
pub(super) fn overlaps_checked<H: Host>(
    host: &H,
    candidate: &Path,
    matches: &[PathBuf],
) -> Result<bool, String> {
    overlaps_checked_until(host, candidate, matches, None)
}

pub(super) fn overlaps_checked_until<H: Host>(
    host: &H,
    candidate: &Path,
    matches: &[PathBuf],
    deadline: Option<MonoTime>,
) -> Result<bool, String> {
    if matches.is_empty() {
        return Ok(false);
    }
    if matches.iter().any(|path| select::overlaps(candidate, path)) {
        return Ok(true);
    }
    // Preflight every ancestor before opening even the deepest path: a
    // local bind mount can sit below a network or FUSE mount point.
    let mounts = host
        .mountinfo()
        .map_err(|error| format!("cannot read mountinfo to prove overlap: {error}"))?;
    let check_time = || {
        if deadline.is_some_and(|end| host.now() >= end) {
            Err(format!(
                "overlap check timed out at {}",
                candidate.display()
            ))
        } else {
            Ok(())
        }
    };
    screen_overlap_mounts(&mounts, candidate, matches, check_time)?;
    if overlaps_in_filesystem(&mounts, candidate, matches)? {
        return Ok(true);
    }
    // The candidate and every match are resolved here against the same
    // table, and `resolve_in` refuses a path whose live mount is not the
    // table's, so their places above rest on mounts confirmed live. A mount
    // made after the table was read is not seen (docs/safety.md, limits).
    overlaps_with(candidate, matches, |path| {
        check_time()?;
        let at = resolve_in(host, &mounts, path, Want::Any)
            .map_err(|error| format!("cannot prove overlap at {}: {error}", path.display()))?;
        at.stat.dev_ino().ok_or_else(|| {
            format!(
                "cannot prove overlap at {}: statx omitted inode",
                path.display()
            )
        })
    })
}

/// Screens the candidate and every match before any of them is resolved,
/// so a refused chain is an error whichever paths the comparison later
/// needs ([`resolve_in`] screens each one it reaches as well).
pub(super) fn screen_overlap_mounts(
    mounts: &[MountEntry],
    candidate: &Path,
    matches: &[PathBuf],
    mut check_time: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    for path in std::iter::once(candidate).chain(matches.iter().map(PathBuf::as_path)) {
        check_time()?;
        screen_mount_chain(mounts, path).map_err(|why| match why {
            MountScreenError::Unsafe { at, why } => {
                format!("cannot prove overlap at {}: {why}", at.display())
            }
            MountScreenError::Networked { at, fs_type, .. } => format!(
                "cannot prove overlap at {}: filesystem type {fs_type} may block on I/O",
                at.display()
            ),
            MountScreenError::PrivateTmp {
                at,
                mount_point,
                fs_root,
            } => format!(
                "cannot prove overlap at {}: mount {} is bound from systemd PrivateTmp root {}",
                at.display(),
                mount_point.display(),
                fs_root.display()
            ),
        })?;
    }
    Ok(())
}

/// Whether the candidate and a match are the same place in one filesystem
/// or one lies inside the other: each translated to the device of the mount
/// it is on and its path from that filesystem's root (the mount's mountinfo
/// root field joined with the rest of the path), then compared by name. A
/// bind mount of a protected tree's subdirectory, or of one of its
/// ancestors, maps back to the protected tree. A filesystem mounted at or
/// below a match is part of what the match shows, so a mount of the
/// candidate's filesystem there whose root overlaps the candidate's place
/// is an overlap too (a mount deep in a protected Cargo target, which
/// discovery does not look into, bound again elsewhere).
pub(super) fn overlaps_in_filesystem(
    mounts: &[MountEntry],
    candidate: &Path,
    matches: &[PathBuf],
) -> Result<bool, String> {
    let (dev, at) = place_in_filesystem(mounts, candidate)?;
    // The mount points where the candidate's place, or a directory holding
    // it or held by it, is shown: few, so each match is checked cheaply.
    let mut shown_at = Vec::new();
    for mount in mounts.iter().filter(|mount| mount.dev == dev) {
        if select::overlaps(&at, absolute_root(mount, candidate)?) {
            shown_at.push(mount.mount_point.as_path());
        }
    }
    for protected in matches {
        let (protected_dev, protected_at) = place_in_filesystem(mounts, protected)?;
        if protected_dev == dev && select::overlaps(&at, &protected_at) {
            return Ok(true);
        }
        if shown_at.iter().any(|point| point.starts_with(protected)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `path`'s device and path from its filesystem's root, from the mount table
/// alone. An unclear table or a root field that is not an absolute path
/// cannot be translated: that fails closed.
fn place_in_filesystem(mounts: &[MountEntry], path: &Path) -> Result<(Dev, PathBuf), String> {
    let mount = mount_for(mounts, path)
        .map_err(|why| format!("cannot prove overlap at {}: {why}", path.display()))?;
    let below = path
        .strip_prefix(&mount.mount_point)
        .expect("mount_for returns a mount whose point is a prefix");
    Ok((mount.dev, absolute_root(mount, path)?.join(below)))
}

/// The mount's root field, which must be an absolute path to translate
/// anything on that filesystem (`path`: what was being proved).
fn absolute_root<'m>(mount: &'m MountEntry, path: &Path) -> Result<&'m Path, String> {
    if mount.fs_root.is_absolute() {
        Ok(&mount.fs_root)
    } else {
        Err(format!(
            "cannot prove overlap at {}: mount {} has filesystem root {}, not an absolute path",
            path.display(),
            mount.mount_point.display(),
            mount.fs_root.display()
        ))
    }
}

pub(super) fn overlaps_with(
    candidate: &Path,
    matches: &[PathBuf],
    mut identity: impl FnMut(&Path) -> Result<DevIno, String>,
) -> Result<bool, String> {
    if matches.is_empty() {
        return Ok(false);
    }
    if matches.iter().any(|path| select::overlaps(candidate, path)) {
        return Ok(true);
    }
    let mut cache = HashMap::<PathBuf, DevIno>::new();
    let mut cached_identity = |path: &Path| {
        if let Some(id) = cache.get(path) {
            return Ok(*id);
        }
        let id = identity(path)?;
        cache.insert(path.to_owned(), id);
        Ok(id)
    };
    let candidate_ancestors = ancestor_identities(candidate, &mut cached_identity)?;
    let mut by_identity = HashMap::<DevIno, Vec<&PathBuf>>::new();
    for (prefix, id) in &candidate_ancestors {
        by_identity.entry(*id).or_default().push(prefix);
    }
    for protected in matches {
        for (protected_prefix, protected_id) in
            ancestor_identities(protected, &mut cached_identity)?
        {
            if let Some(candidate_prefixes) = by_identity.get(&protected_id) {
                for candidate_prefix in candidate_prefixes {
                    let candidate_suffix =
                        candidate.strip_prefix(candidate_prefix).expect("ancestor");
                    let protected_suffix =
                        protected.strip_prefix(&protected_prefix).expect("ancestor");
                    if select::overlaps(candidate_suffix, protected_suffix) {
                        return Ok(true);
                    }
                }
            }
        }
    }
    Ok(false)
}

pub(super) fn ancestor_identities(
    path: &Path,
    identity: &mut impl FnMut(&Path) -> Result<DevIno, String>,
) -> Result<Vec<(PathBuf, DevIno)>, String> {
    path.ancestors()
        .map(|ancestor| Ok((ancestor.to_owned(), identity(ancestor)?)))
        .collect()
}

pub(super) fn excluded_by<H: Host>(
    host: &H,
    sources: &[NegatedSource],
    matches: &[Vec<PathBuf>],
    candidate: &Path,
) -> Result<Option<String>, String> {
    excluded_by_until(host, sources, matches, candidate, None)
}

pub(super) fn excluded_by_until<H: Host>(
    host: &H,
    sources: &[NegatedSource],
    matches: &[Vec<PathBuf>],
    candidate: &Path,
    deadline: Option<MonoTime>,
) -> Result<Option<String>, String> {
    for (source, paths) in sources.iter().zip(matches) {
        if overlaps_checked_until(host, candidate, paths, deadline)? {
            return Ok(Some(source.profile.clone()));
        }
    }
    Ok(None)
}

/// Called by each kind while it still holds its candidate lock and before
/// its final action, enumerating every `protect` and `!profile:` match
/// again: before a command action, which may touch anything in the
/// candidate (a delete action asks [`tier_policy::DeletePolicy`] instead).
/// Protection is global; negation belongs to the selected source term.
/// Uncertain discovery or identity stops the run.
pub(super) fn policy_skip<H: Host>(
    host: &H,
    protections: &[ProtectedSource],
    negated: &[NegatedSource],
    deadline: Option<MonoTime>,
    path: &Path,
) -> Result<Option<Skip>, Stop> {
    let protected = scan_protections(host, protections, deadline)
        .map_err(|error| policy_failed(path, "protect recheck", error))?;
    if let Some(skip) = protected_verdict(host, &protected, deadline, path)? {
        return Ok(Some(skip));
    }
    let negated_matches = scan_negated(host, negated, deadline)
        .map_err(|error| policy_failed(path, "!profile recheck", error))?;
    excluded_verdict(host, negated, &negated_matches, deadline, path)
}

/// `path` judged against enumerated `protect` and `!profile:` matches (one
/// list per `negated` source) at the final action check. Uncertain identity
/// stops the run.
pub(super) fn policy_verdict<H: Host>(
    host: &H,
    protected: &[PathBuf],
    negated: &[NegatedSource],
    negated_matches: &[Vec<PathBuf>],
    deadline: Option<MonoTime>,
    path: &Path,
) -> Result<Option<Skip>, Stop> {
    if let Some(skip) = protected_verdict(host, protected, deadline, path)? {
        return Ok(Some(skip));
    }
    excluded_verdict(host, negated, negated_matches, deadline, path)
}

pub(super) fn protected_verdict<H: Host>(
    host: &H,
    protected: &[PathBuf],
    deadline: Option<MonoTime>,
    path: &Path,
) -> Result<Option<Skip>, Stop> {
    let overlaps = overlaps_checked_until(host, path, protected, deadline)
        .map_err(|error| policy_failed(path, "protect overlap", error))?;
    Ok(overlaps.then(|| {
        Skip::new(
            Reason::Protected,
            path.to_owned(),
            "overlaps a protected match at final action check".to_owned(),
        )
    }))
}

pub(super) fn excluded_verdict<H: Host>(
    host: &H,
    negated: &[NegatedSource],
    negated_matches: &[Vec<PathBuf>],
    deadline: Option<MonoTime>,
    path: &Path,
) -> Result<Option<Skip>, Stop> {
    Ok(
        excluded_by_until(host, negated, negated_matches, path, deadline)
            .map_err(|error| policy_failed(path, "!profile overlap", error))?
            .map(|profile| {
                Skip::new(
                    Reason::Excluded,
                    path.to_owned(),
                    format!("overlaps !profile:{profile} at final action check"),
                )
            }),
    )
}

pub(super) fn policy_failed(path: &Path, what: &'static str, detail: String) -> Stop {
    Stop::Failed {
        path: path.to_owned(),
        error: HostError::invalid(what, path.display(), detail),
    }
}
