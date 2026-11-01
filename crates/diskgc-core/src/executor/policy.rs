//! The `protect` and `!profile:` verdicts on one candidate: matches
//! enumerated by discovery, and overlap proved by path, by place in the
//! filesystem (mountinfo's root field) or by object identity below a
//! screened mount chain (docs/safety.md, Filesystem candidates). [`super::tier_policy`] caches these per tier for delete
//! actions.

use crate::cargo_target::{self, Stop};
use crate::engine::{Reason, Skip, may_be_networked};
use crate::fs_kind;
use crate::host::{Dev, DevIno, Host, HostError, MonoTime, MountEntry};
use crate::resolve::{
    MountScreenError, Want, mount_for, place_in_filesystem, resolve_in, screen_mount_chain,
};
use crate::select;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::tier_policy::Protection;
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
pub fn overlaps_checked<H: Host>(
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
    refuse_overlay(&mounts, candidate, matches)?;
    if overlaps_in_filesystem(&mounts, candidate, matches, check_time)? {
        return Ok(true);
    }
    // The precondition of the "no" above: the candidate and every match are
    // resolved here against the same table, and `resolve_in` refuses a path
    // whose live mount is not the table's (a covered mount, still listed),
    // so their places rest on mounts confirmed live. A mount made after the
    // table was read is not seen (docs/safety.md, limits).
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

/// The overlay rules of docs/safety.md, Not protected, not excluded: with
/// any match, a candidate or match on an overlay mount, or a mount that
/// stores its data in another filesystem's directories ([`stored_elsewhere`])
/// at or below a match ([`nested_in`], TASK-268), cannot be proved apart.
fn refuse_overlay(
    mounts: &[MountEntry],
    candidate: &Path,
    matches: &[PathBuf],
) -> Result<(), String> {
    for path in std::iter::once(candidate).chain(matches.iter().map(PathBuf::as_path)) {
        let mount = mount_for(mounts, path)
            .map_err(|why| format!("cannot prove overlap at {}: {why}", path.display()))?;
        if mount.fs_type == OVERLAY {
            return Err(format!(
                "cannot prove overlap at {}: it is on overlay mount {}, whose upper and lower \
                 directories may hold a protected or negated match under another name",
                path.display(),
                mount.mount_point.display()
            ));
        }
    }
    let nested: Vec<&MountEntry> = mounts
        .iter()
        .filter(|mount| stored_elsewhere(&mount.fs_type))
        .collect();
    if nested.is_empty() {
        return Ok(());
    }
    let by_id: HashMap<u64, &MountEntry> = mounts.iter().map(|mount| (mount.id, mount)).collect();
    let places = matches
        .iter()
        .map(|matched| place_of(mounts, matched))
        .collect::<Result<Vec<_>, _>>()?;
    for mount in nested {
        if let Some(matched) = nested_in(&by_id, mount, matches, &places, candidate)? {
            return Err(format!(
                "{} mount {} lies inside the match {} (by path or by its place on a mount of \
                 the match's filesystem), and {} may be anywhere: no candidate can be proved \
                 apart from the match while it is mounted (cannot prove overlap at {})",
                mount.fs_type,
                mount.mount_point.display(),
                matched.display(),
                if mount.fs_type == OVERLAY {
                    "its upper and lower directories"
                } else {
                    "the directories its data is stored in"
                },
                candidate.display(),
            ));
        }
    }
    Ok(())
}

/// The type name overlayfs has in mountinfo.
const OVERLAY: &str = "overlay";

/// Kernel filesystems that stack on directories of other filesystems and
/// write through to them: eCryptfs (its ciphertext directory), aufs (its
/// branches), shiftfs (the directory it shifts). Refused only when mounted
/// at or below a match; on the candidate's or match's own mount they are
/// taken as local (docs/safety.md, limits), or an eCryptfs-encrypted home
/// would stop every run.
const STACKED_ON_DIRECTORIES: &[&str] = &["ecryptfs", "aufs", "shiftfs"];

/// Whether a filesystem of `fs_type` may store its data in directories of
/// another filesystem, which mountinfo does not name: overlayfs, the
/// [`STACKED_ON_DIRECTORIES`], and every type the mount screen refuses
/// (FUSE such as `bindfs` or `fuse-overlayfs`, an NFS export of this host,
/// an automount point).
fn stored_elsewhere(fs_type: &str) -> bool {
    fs_type == OVERLAY || STACKED_ON_DIRECTORIES.contains(&fs_type) || may_be_networked(fs_type)
}

/// The match `mount` lies at or below, if any: its mount point at or below
/// the match's path, or one of the places its point is attached at down
/// its parent chain ([`attachments`]) inside the match's place on the same
/// device (`places`: each match's, in order), which finds it through a
/// bind of the match or of one of its ancestors. A chain that cannot be
/// placed is an error (`candidate`: what was being proved).
fn nested_in<'m>(
    by_id: &HashMap<u64, &MountEntry>,
    mount: &MountEntry,
    matches: &'m [PathBuf],
    places: &[(Dev, PathBuf)],
    candidate: &Path,
) -> Result<Option<&'m Path>, String> {
    let mut attached_at = Vec::new();
    attachments(by_id, mount, candidate, &mut attached_at)?;
    Ok(matches
        .iter()
        .zip(places)
        .find(|(matched, (dev, place))| {
            // By path as well as by place: a mount whose parent the table
            // does not list (another namespace's, or a root outside the
            // process's) has no attachment to compare, yet lies below
            // the match's path.
            mount.mount_point.starts_with(matched)
                || attached_at
                    .iter()
                    .any(|(on, at)| on == dev && at.starts_with(place))
        })
        .map(|(matched, _)| matched.as_path()))
}

/// Whether the candidate and a match are the same place in one filesystem
/// or one lies inside the other: each translated to the device of the mount
/// it is on and its path from that filesystem's root (the mount's mountinfo
/// root field joined with the rest of the path), then compared by name. A
/// bind mount of a protected tree's subdirectory, or of one of its
/// ancestors, maps back to the protected tree.
///
/// A filesystem mounted at or below a match is part of what the match
/// shows. Every mount showing the candidate's place (a mount of its device
/// whose root overlaps it) is attached somewhere: its mount point is a
/// place on its parent mount, whose own point is a place on its parent, and
/// so on. An overlap is counted when one of those places lies inside a
/// match's place on the same device, or a mount point lies at or below the
/// match's path: a mount deep in a protected Cargo target (discovery does
/// not look there) or on a private bind of a protected subdirectory, shown
/// again elsewhere.
///
/// Precondition for a "no": the places come from the table alone, so the
/// caller must resolve the candidate and every match on the mount the table
/// names (as [`overlaps_with`] does); a covered mount stays in the table.
pub(super) fn overlaps_in_filesystem(
    mounts: &[MountEntry],
    candidate: &Path,
    matches: &[PathBuf],
    mut check_time: impl FnMut() -> Result<(), String>,
) -> Result<bool, String> {
    let by_id: HashMap<u64, &MountEntry> = mounts.iter().map(|mount| (mount.id, mount)).collect();
    let (dev, at) = place_of(mounts, candidate)?;
    // Mount points showing the candidate's place, and the places on other
    // mounts they are attached at: few, so each match is checked cheaply.
    let mut shown_at = Vec::new();
    let mut attached_at = Vec::new();
    for mount in mounts.iter().filter(|mount| mount.dev == dev) {
        if select::overlaps(&at, root_of(mount, candidate)?) {
            shown_at.push(mount.mount_point.as_path());
            attachments(&by_id, mount, candidate, &mut attached_at)?;
        }
    }
    for protected in matches {
        check_time()?;
        let (protected_dev, protected_at) = place_of(mounts, protected)?;
        if (protected_dev == dev && select::overlaps(&at, &protected_at))
            || shown_at.iter().any(|point| point.starts_with(protected))
            || attached_at
                .iter()
                .any(|(on, place)| *on == protected_dev && place.starts_with(&protected_at))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The places `mount`'s point is attached at, down its parent chain: on
/// each parent, the parent's root field joined with the child's point below
/// the parent's (a mount stacked on a point: the parent's root). The chain
/// ends at a mount whose parent the table does not list or that is its own
/// parent (the root of the namespace's tree). A child point not below its
/// parent's, or a chain longer than the table, cannot be placed: an error.
fn attachments(
    by_id: &HashMap<u64, &MountEntry>,
    mount: &MountEntry,
    candidate: &Path,
    out: &mut Vec<(Dev, PathBuf)>,
) -> Result<(), String> {
    let mut child = mount;
    for _ in 0..=by_id.len() {
        let Some(parent) = by_id
            .get(&child.parent_id)
            .filter(|parent| parent.id != child.id)
        else {
            return Ok(());
        };
        let place = place_in_filesystem(parent, &child.mount_point).ok_or_else(|| {
            format!(
                "cannot prove overlap at {}: mount {} cannot be placed on its parent {} \
                 (root field {})",
                candidate.display(),
                child.mount_point.display(),
                parent.mount_point.display(),
                parent.fs_root.display()
            )
        })?;
        out.push((parent.dev, place));
        child = parent;
    }
    Err(format!(
        "cannot prove overlap at {}: the parent chain of mount {} does not end",
        candidate.display(),
        mount.mount_point.display()
    ))
}

/// `path`'s device and place in its filesystem, from the mount table alone;
/// an unclear table or a root field that is not absolute fails closed.
fn place_of(mounts: &[MountEntry], path: &Path) -> Result<(Dev, PathBuf), String> {
    let mount = mount_for(mounts, path)
        .map_err(|why| format!("cannot prove overlap at {}: {why}", path.display()))?;
    root_of(mount, path)?;
    let place = place_in_filesystem(mount, path).expect("mount_for returns a prefix mount");
    Ok((mount.dev, place))
}

/// The mount's root field, which must be an absolute path to translate
/// anything on that filesystem (`path`: what was being proved).
fn root_of<'m>(mount: &'m MountEntry, path: &Path) -> Result<&'m Path, String> {
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

/// When a judgement enumerates the matches again (docs/safety.md, Not
/// protected, not excluded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refresh {
    /// Only when the enumerated matches cannot settle it: one of them cannot
    /// be proven any more (removed or moved since: a `cargo clean` of a
    /// protected target), so the verdict on them errs.
    OnError,
    /// Also when they show no overlap: before an action that acts on what
    /// it finds when it runs, or after a walk that may have seen a match
    /// made since the enumeration.
    Always,
}

/// A verdict a match set gives: an overlap settles it, whatever matches
/// were enumerated since.
pub(super) trait Verdict {
    fn overlaps(&self) -> bool;
}

impl Verdict for bool {
    fn overlaps(&self) -> bool {
        *self
    }
}

/// The negated profile a candidate overlaps, if any.
impl Verdict for Option<String> {
    fn overlaps(&self) -> bool {
        self.is_some()
    }
}

/// The one `protect` / `!profile:` judgement every check makes (TASK-269):
/// `verdict` on the `enumerated` matches; on matches enumerated now
/// (`enumerate`) when that errs or, under [`Refresh::Always`], shows no
/// overlap. The fresh matches come back (`Some`) so the caller can keep
/// them in place of the old ones and later checks need not enumerate
/// again. Only an error on the fresh matches, or of their enumeration, is
/// an error: a match gone since says nothing about this candidate.
pub(super) fn judge<M: ?Sized, O: std::borrow::Borrow<M>, V: Verdict>(
    enumerated: &M,
    refresh: Refresh,
    verdict: impl Fn(&M) -> Result<V, String>,
    enumerate: impl FnOnce() -> Result<O, String>,
) -> Result<(V, Option<O>), String> {
    match verdict(enumerated) {
        Ok(found) if found.overlaps() || refresh == Refresh::OnError => return Ok((found, None)),
        Ok(_) | Err(_) => {}
    }
    let fresh = enumerate()?;
    let found = verdict(fresh.borrow())?;
    Ok((found, Some(fresh)))
}

/// [`judge`] for `protect`: overlap with `enumerated`'s matches, or with
/// [`Protection::enumerate`]'s.
pub(super) fn judge_protect<H: Host>(
    host: &H,
    protections: &[ProtectedSource],
    enumerated: &Protection,
    candidate: &Path,
    deadline: Option<MonoTime>,
    refresh: Refresh,
) -> Result<(bool, Option<Protection>), String> {
    if protections.is_empty() {
        return Ok((false, None));
    }
    judge(
        enumerated,
        refresh,
        |protection: &Protection| {
            overlaps_checked_until(host, candidate, &protection.matches, deadline)
        },
        || Protection::enumerate(host, protections, deadline),
    )
}

/// One match list per `!profile:` source of a term, in source order.
pub(super) type NegatedMatches = Vec<Vec<PathBuf>>;

/// [`judge`] for a term's `!profile:` sources (one match list each): the
/// first source `candidate` overlaps, by [`scan_negated`]'s matches when
/// `enumerated` cannot settle it.
pub(super) fn judge_negated<H: Host>(
    host: &H,
    sources: &[NegatedSource],
    enumerated: &[Vec<PathBuf>],
    candidate: &Path,
    deadline: Option<MonoTime>,
    refresh: Refresh,
) -> Result<(Option<String>, Option<NegatedMatches>), String> {
    if sources.is_empty() {
        return Ok((None, None));
    }
    judge(
        enumerated,
        refresh,
        |matches: &[Vec<PathBuf>]| excluded_by_until(host, sources, matches, candidate, deadline),
        || scan_negated(host, sources, deadline),
    )
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

#[cfg(test)]
mod tests {
    use super::refuse_overlay;
    use crate::host::{Dev, MountEntry};
    use std::path::{Path, PathBuf};

    fn mount(id: u64, parent_id: u64, point: &str, minor: u32, fs_type: &str) -> MountEntry {
        MountEntry {
            id,
            parent_id,
            dev: Dev { major: 0, minor },
            fs_root: PathBuf::from("/"),
            mount_point: PathBuf::from(point),
            fs_type: fs_type.to_owned(),
            source: "fixture".to_owned(),
            read_only: false,
        }
    }

    fn table(nested: MountEntry) -> Vec<MountEntry> {
        vec![
            mount(1, 1, "/", 1, "ext4"),
            mount(2, 1, "/proj", 2, "ext4"),
            mount(3, 1, "/cache", 3, "ext4"),
            nested,
        ]
    }

    fn refused(mounts: &[MountEntry]) -> Option<String> {
        refuse_overlay(
            mounts,
            Path::new("/cache/upper/cruft"),
            &[PathBuf::from("/proj/target")],
        )
        .err()
    }

    /// TASK-268 review (Codex 2, mped S5): eCryptfs, aufs and shiftfs write
    /// through to directories elsewhere; nested in a match they are refused
    /// like an overlay. On the candidate's own mount they are not (an
    /// eCryptfs home would stop every run): a stated limit.
    #[test]
    fn a_stacking_filesystem_inside_a_match_is_refused_but_not_one_holding_the_candidate() {
        for fs_type in ["ecryptfs", "aufs", "shiftfs", "overlay", "fuse.bindfs"] {
            let mounts = table(mount(60, 2, "/proj/target/debug/deps/view", 60, fs_type));
            let error = refused(&mounts).unwrap_or_else(|| panic!("{fs_type} not refused"));
            assert!(
                error.starts_with(&format!(
                    "{fs_type} mount /proj/target/debug/deps/view lies inside the match /proj/target"
                )),
                "{error}"
            );
            assert!(
                error.ends_with("(cannot prove overlap at /cache/upper/cruft)"),
                "{error}"
            );
        }
        for fs_type in ["ecryptfs", "aufs", "shiftfs"] {
            let mounts = table(mount(60, 3, "/cache/upper", 60, fs_type));
            assert_eq!(refused(&mounts), None, "{fs_type} under the candidate");
        }
        assert_eq!(
            refused(&table(mount(60, 2, "/proj/other", 60, "ecryptfs"))),
            None
        );
    }

    /// NIT-2: a nested mount whose parent the table does not list (another
    /// namespace's) has no attachment chain; its path alone places it.
    #[test]
    fn a_nested_mount_with_an_unlisted_parent_is_placed_by_its_path() {
        let mounts = table(mount(60, 99, "/proj/target/view", 60, "overlay"));
        assert!(refused(&mounts).is_some());
        let elsewhere = table(mount(60, 99, "/elsewhere/view", 60, "overlay"));
        assert_eq!(refused(&elsewhere), None);
    }
}
