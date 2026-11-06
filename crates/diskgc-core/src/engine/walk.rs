//! The eligibility walk (docs/safety.md, Filesystem candidates: The
//! eligibility walk).
//!
//! Every directory is opened below its parent (`open_beneath`: no symlink,
//! no mount crossing) and matched against the identity of the `statx` that
//! found it, listed in full. The mount table is read before the candidate
//! is statted; a mount point or `systemd-private-*` name is refused before
//! any statx of that entry. Every other name is `statx`ed (not followed)
//! and checked, in this order:
//!
//! 1. the kernel answered the type, inode, owner and mount id
//!    (`unsupported_fs` otherwise);
//! 2. it is on the candidate's mount (`contains_mount`), including a mount
//!    made after the table was read if its statx returns;
//! 3. it is not a `.diskgc-keep` (`kept`);
//! 4. its owner is one of [`Rules::owners`] (`not_owner`);
//! 5. it is not a socket or FIFO, if [`Rules::block_ipc`] (`contains_ipc`);
//! 6. it is not a device node or of unknown type, and does not have a
//!    Cargo lock file's name (`.cargo-*lock*`, [`is_cargo_lock_name`]:
//!    the known [`CARGO_LOCK_NAMES`] and any unknown one, anywhere), and
//!    is not at a fine-grain Cargo unit lock's place
//!    (`build/<package>/<hash>/.lock`, [`is_cargo_unit_lock_path`])
//!    (`contains_special`);
//! 7. unless [`Rules::checkouts`] is [`Checkouts::Allow`] (a profile
//!    that says `checkouts: allow`), it does not have a VCS checkout
//!    marker's name ([`CHECKOUT_MARKERS`], whatever its type, anywhere, the
//!    candidate itself included) (`contains_checkout`), unless it is an
//!    empty regular file at a place its source tolerates
//!    ([`Rules::empty_checkout_markers`], [`tolerated_marker`]).
//!
//! Before those, each directory's complete listing, the candidate's own
//! first, is judged as a whole: a container storage root
//! ([`super::container_storage`]) refuses the candidate
//! (`contains_special`).
//!
//! On an overlay with an upper layer, each admitted entry's name in that
//! layer is read as well ([`super::UpperName`], [`overlay`]; TASK-305),
//! for the in-use check's loop rules.
//!
//! The first entry that fails stops the walk. Host errors on the way are
//! classified in one place, [`host_failure`]: fail closed, never treated as
//! empty.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::host::{Base, DirListing, Errno, FileType, Host, HostError, MonoTime, Stat, Timestamp};
use crate::pattern::Pattern;
use crate::resolve::{ResolveError, Unsafe};

use super::frames::{OpenFailed, OpenFailure, Stack};
use super::{
    Candidate, Entry, Identity, Reason, Skip, Volume, WalkRecord, deadline_passed, overlay,
};

/// The keep marker (docs/reference.md, Protection marker).
pub const KEEP_MARKER: &str = ".diskgc-keep";

/// systemd's PrivateTmp tree is not cruft, even inside a candidate found
/// by a broad profile. Compare names as bytes before touching their inode.
pub const PRIVATE_TMP_PREFIX: &str = "systemd-private-";

pub fn is_private_tmp(name: &OsStr) -> bool {
    name.as_bytes().starts_with(PRIVATE_TMP_PREFIX.as_bytes())
}

pub fn path_in_private_tmp(path: &Path) -> bool {
    path.components()
        .any(|part| is_private_tmp(part.as_os_str()))
}

fn private_tmp(path: PathBuf) -> Skip {
    Skip::new(
        Reason::ContainsSpecial,
        path,
        "systemd PrivateTmp entry (systemd-private-*): never entered or deleted",
    )
}

fn mounted(path: PathBuf) -> Skip {
    Skip::new(
        Reason::ContainsMount,
        path,
        "a mount point (mountinfo): not touched",
    )
}

/// The file that marks a Cargo target directory (Cargo discovery).
pub const CACHEDIR_TAG: &str = "CACHEDIR.TAG";

/// The known Cargo lock file names (docs/safety.md, `cargo-target`, Lock
/// files): the lock set the Cargo protocol takes. Each matches
/// [`is_cargo_lock_name`], so the walk refuses it anywhere like any other
/// lock name.
pub const CARGO_LOCK_NAMES: &[&str] = &[".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

/// The name of the per-unit lock file nightly Cargo's `-Zfine-grain-locking`
/// creates and holds in each unit directory of the build directory,
/// `<profile>/build/<package>/<hash>/.lock` (measured on
/// `nightly-2026-10-16`, TASK-205; docs/safety.md, `cargo-target`, Lock
/// files). It is outside `.cargo-*lock*`, so [`is_cargo_lock_name`] does
/// not cover it: [`is_cargo_unit_lock_path`] does.
pub const CARGO_UNIT_LOCK_NAME: &str = ".lock";

/// Whether `path` is where fine-grain Cargo keeps a unit's lock: named
/// [`CARGO_UNIT_LOCK_NAME`] with a directory named `build` two levels above
/// its parent (`build/<package>/<hash>/.lock`). By the absolute path, not
/// by a target around it: the candidate may be the unit directory itself,
/// with the target's CACHEDIR.TAG and lock set outside it (TASK-167).
/// Neither the package nor the hash is checked, so a Cargo that changes
/// its hash format still fails closed. The name alone would be too wide:
/// pytest's `pytest-<N>/.lock` is the pid marker the `pytest-tmp` liveness
/// reads, in directories that profile deletes; one that happens to sit at
/// this shape (a basetemp in a `build/` directory) only keeps its
/// candidate.
pub fn is_cargo_unit_lock_path(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new(CARGO_UNIT_LOCK_NAME))
        && path.ancestors().nth(3).and_then(Path::file_name) == Some(OsStr::new("build"))
}

/// The names that mark a version-control checkout (docs/safety.md, The
/// eligibility walk: no source checkout): Git's directory, or its file in
/// a worktree, a submodule or a `--separate-git-dir` checkout; Mercurial's;
/// Subversion's. An entry of one of these names, of any type (a symlink
/// too: never followed, it counts as a checkout), anywhere in a candidate,
/// the candidate itself included, refuses it (`contains_checkout`) unless
/// its profile says `checkouts: allow` ([`Rules::checkouts`]): it may hold
/// uncommitted work.
pub const CHECKOUT_MARKERS: &[&str] = &[".git", ".hg", ".svn"];

/// Whether `name` is one of [`CHECKOUT_MARKERS`] (compared as bytes, in
/// full: `.gitignore` is not).
pub fn is_checkout_marker(name: &OsStr) -> bool {
    CHECKOUT_MARKERS.iter().any(|marker| name == *marker)
}

/// A checkout marker the walk met in a candidate (an entry whose name is
/// one of [`CHECKOUT_MARKERS`]) and admitted: under [`Checkouts::Allow`],
/// or as a [`tolerated_marker`]. What [`super::WalkRecord::contains_checkout`]
/// judges again for each source of a union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutMarker {
    /// Its names below the candidate (empty: the candidate itself).
    pub below: PathBuf,
    /// A regular file of size 0, as its `statx` showed it.
    pub empty_file: bool,
}

/// Whether a checkout marker at `below` (its path below the candidate) is
/// tolerated by a source's `empty_checkout_markers` (docs/safety.md, No
/// source checkout: a tolerated empty marker): an empty regular file
/// (`empty_file`) whose path matches one of `patterns` exactly, segment by
/// segment as `find.paths` does. A directory, a symlink, a file with any
/// content, the candidate itself (`below` empty) or another place is never
/// tolerated. An empty file is no checkout: Git reads a `.git` file as a
/// `gitdir:` pointer, and an empty one points nowhere.
pub fn tolerated_marker(patterns: &[Pattern], below: &Path, empty_file: bool) -> bool {
    if !empty_file {
        return false;
    }
    let mut names = Vec::new();
    for component in below.components() {
        match component {
            std::path::Component::Normal(name) => names.push(name),
            _ => return false,
        }
    }
    !names.is_empty() && patterns.iter().any(|p| p.matches_names(&names))
}

/// Whether a candidate may hold a version-control checkout: a profile's
/// `checkouts` field (docs/reference.md, Profiles; docs/safety.md, No
/// source checkout). The default, and what a profile without the field
/// gets, is [`Checkouts::Refuse`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Checkouts {
    /// A candidate holding a [`CHECKOUT_MARKERS`] name, or lying inside a
    /// checkout's metadata ([`checkout_metadata_below`]), is
    /// `contains_checkout`.
    #[default]
    Refuse,
    /// Neither rule applies.
    Allow,
}

impl Checkouts {
    /// Both settings, in the order docs/reference.md lists them.
    pub const ALL: [Checkouts; 2] = [Checkouts::Refuse, Checkouts::Allow];

    /// Their names as YAML and JSON spell them, in the order of
    /// [`Checkouts::ALL`]: the one list of the values.
    pub const NAMES: [&'static str; 2] = ["refuse", "allow"];

    /// The setting as YAML and JSON spell it.
    pub fn as_str(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

/// The path rule of the checkout guard (docs/safety.md, No source
/// checkout: inside a checkout's metadata, by path): under
/// [`Checkouts::Refuse`], the refusal (`contains_checkout`) of a candidate
/// at `at` found below `root` when a name strictly between them is one of
/// [`CHECKOUT_MARKERS`] (`/r/proj/.git` for `root` `/r` and `at`
/// `/r/proj/.git/objects`). Path text only, no system call. `at` itself is
/// left to the walk (which refuses it by its name), and the root's own
/// path is not looked at. Fail closed, whatever the setting: an `at` that
/// is not below `root` is a caller's mistake no rule can judge, refused as
/// `unsafe_path`. `None` when it is below `root` and, under
/// [`Checkouts::Allow`] or with no marker on the way, nothing refuses it.
pub fn checkout_metadata_below(checkouts: Checkouts, root: &Path, at: &Path) -> Option<Skip> {
    checkout_metadata(checkouts, root, at, Own::Excluded)
}

/// The same path rule for every candidate in the directory `dir`, without
/// naming one: the names from below `root` down to `dir`, `dir`'s own name
/// included (what [`checkout_metadata_below`] judges for any entry of
/// `dir`). For a kind that keeps a directory and deletes what it holds
/// (the Cargo kind's profile directories), so that the verdict does not
/// depend on what the directory holds at the time. Fail closed as
/// [`checkout_metadata_below`]: a `dir` not below `root` is `unsafe_path`
/// whatever the setting; `root` itself has no names to judge.
pub fn checkout_metadata_down_to(checkouts: Checkouts, root: &Path, dir: &Path) -> Option<Skip> {
    checkout_metadata(checkouts, root, dir, Own::Included)
}

/// Whether the path rule looks at the last name of the path it judges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Own {
    /// A candidate's own name is the walk's.
    Excluded,
    /// A directory whose entries are the candidates.
    Included,
}

fn checkout_metadata(checkouts: Checkouts, root: &Path, at: &Path, own: Own) -> Option<Skip> {
    let Ok(below) = at.strip_prefix(root) else {
        return Some(Skip::new(
            Reason::UnsafePath,
            at.to_path_buf(),
            format!(
                "not below its root {}: where it lies cannot be judged (fail closed)",
                root.display()
            ),
        ));
    };
    if checkouts == Checkouts::Allow {
        return None;
    }
    let mut names = below.components();
    if own == Own::Excluded {
        names.next_back()?;
    }
    let mut path = root.to_path_buf();
    for name in names {
        path.push(name);
        if is_checkout_marker(name.as_os_str()) {
            return Some(Skip::new(
                Reason::ContainsCheckout,
                path,
                "inside a version-control checkout's metadata (a directory on the way below the \
                 root): deleting part of it can destroy the repository, and the profile refuses \
                 checkouts (`checkouts: refuse`, the default)",
            ));
        }
    }
    None
}

/// What the walk requires of a candidate besides the fixed rules.
#[derive(Debug, Clone, Copy)]
pub struct Rules<'a> {
    /// The users every entry must belong to: the user diskgc runs as, or
    /// the `owners:` list under `--allow-root`.
    pub owners: &'a [u32],
    /// Any socket or FIFO refuses the candidate (`contains_ipc`; the
    /// default policy). Off, the walk records them and the caller decides,
    /// from [`WalkRecord::ipc_entries`], which are live (the other policy,
    /// docs/safety.md).
    pub block_ipc: bool,
    /// At most this many entries, the candidate included
    /// ([`super::DEFAULT_MAX_ENTRIES`] unless a test needs a small one).
    pub max_entries: usize,
    /// Whether a candidate may hold a VCS checkout: the source profile's
    /// setting. [`Rules::new`] (and so anything built from it without
    /// saying otherwise) refuses.
    pub checkouts: Checkouts,
    /// Under [`Checkouts::Refuse`], the places below the candidate where a
    /// checkout marker that is an empty regular file does not refuse it
    /// ([`tolerated_marker`]): the source's `empty_checkout_markers` (a
    /// built-in's definition, docs/reference.md, Profiles). Empty from
    /// [`Rules::new`]: every marker refuses.
    pub empty_checkout_markers: &'a [Pattern],
}

impl<'a> Rules<'a> {
    /// The rules of a run for `owners`: any socket or FIFO refuses (the
    /// default policy), the default entry budget, and checkouts refused
    /// ([`Checkouts::default`]). A caller that allows checkouts says so:
    /// `Rules { checkouts: Checkouts::Allow, ..Rules::new(owners) }`.
    pub fn new(owners: &'a [u32]) -> Self {
        Self {
            owners,
            block_ipc: true,
            max_entries: super::DEFAULT_MAX_ENTRIES,
            checkouts: Checkouts::default(),
            empty_checkout_markers: &[],
        }
    }
}

/// Why the walk ended without a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkStop {
    /// The candidate is not eligible.
    Skipped(Skip),
    /// The deadline passed before the walk was done (at `path`).
    DeadlinePassed { path: PathBuf },
    /// The host failed in a way that says nothing about the candidate and
    /// would hit every other one too (`EMFILE`, `ENFILE`, `ENOMEM`, `EIO`,
    /// any errno not classified otherwise): an error of the run, never a
    /// skip, so it fails loudly instead of quietly skipping every candidate.
    Failed { path: PathBuf, error: HostError },
}

impl From<Skip> for WalkStop {
    fn from(skip: Skip) -> Self {
        Self::Skipped(skip)
    }
}

/// The eligibility walk of `candidate`: its record, or why it is not
/// eligible. Reads mountinfo first and refuses any mount point in its
/// tree, by path or by place ([`crate::places::mounts_within`]), before
/// statx; checks the deadline before every `statx`, open and listing.
/// Refuses a VCS checkout inside ([`CHECKOUT_MARKERS`],
/// `contains_checkout`) unless `rules` allow checkouts (the source
/// profile's `checkouts: allow`).
pub fn walk<H: Host>(
    host: &H,
    candidate: &Candidate<'_, H::Fd>,
    rules: &Rules<'_>,
    deadline: Option<MonoTime>,
) -> Result<WalkRecord, WalkStop> {
    let path = || candidate.path.to_path_buf();
    if is_private_tmp(candidate.name) {
        return Err(private_tmp(path()).into());
    }
    if deadline_passed(host, deadline) {
        return Err(WalkStop::DeadlinePassed { path: path() });
    }
    let mounts = host.mountinfo().map_err(|error| WalkStop::Failed {
        path: PathBuf::from("/proc/self/mountinfo"),
        error,
    })?;
    // By path and by place (TASK-290): through a non-recursive bind a
    // mount point is an uncovered directory of the candidate's mount,
    // which the mount ids below do not tell, and Linux refuses to remove.
    let mount_points: Vec<PathBuf> = match crate::places::mounts_within(&mounts, candidate.path) {
        Ok(found) => found.into_iter().map(|(_, at)| at).collect(),
        Err(why) => {
            return Err(Skip::new(
                Reason::UnsafePath,
                path(),
                format!("cannot be placed in its filesystem (mountinfo): {why}"),
            )
            .into());
        }
    };
    if mount_points.iter().any(|mount| mount == candidate.path) {
        return Err(mounted(path()).into());
    }
    let stat = match host.statx(Base::Dir(candidate.parent), Path::new(candidate.name)) {
        Ok(stat) => stat,
        Err(e) if e.is(Errno::ENOENT) => {
            return Err(Skip::new(Reason::Missing, path(), e.to_string()).into());
        }
        Err(e) => return Err(host_failure(path(), "stat", &e)),
    };
    // On an overlay, its upper layer: what a deletion through it changes
    // (TASK-307, TASK-305).
    let mut overlay = if candidate.fs_type() == "overlay" {
        match mounts.iter().find(|m| m.id == candidate.mount_id) {
            Some(mount) => overlay::upper_of(host, &mounts, mount),
            None => overlay::Walk::Unreached {
                why: "its mount is not in the mount table".to_owned(),
                subvol: None,
            },
        }
    } else {
        overlay::Walk::Off
    };
    // The candidate's own subvolume, which every directory below must be
    // in (no nested subvolume, TASK-286, TASK-307); a directory candidate
    // in another than its parent directory is itself the top of one.
    if stat.file_type == Some(FileType::Directory) {
        let parent = match host.statx(Base::Dir(candidate.parent), Path::new("")) {
            Ok(parent) => parent,
            Err(e) => return Err(host_failure(path(), "stat", &e)),
        };
        super::same_subvolume(
            &path(),
            Volume::of(&stat),
            Volume::of(&parent),
            Some(candidate.fs_type()),
        )?;
        prove_candidate(host, &mut overlay, candidate.path, &stat, &parent)?;
    }
    let mut walker = Walker {
        host,
        candidate,
        volume: Volume::of(&stat),
        overlay,
        upper_unread: None,
        rules,
        deadline,
        mount_points,
        listings: Vec::new(),
        markers: Vec::new(),
        entries: 0,
        bytes: 0,
        newest_change: None,
        times_unknown: false,
    };
    let root = walker.admit(candidate.name.to_owned(), &stat, path)?;
    let mut stack = Stack::new(host, candidate, deadline);
    if let Some(listing) = root.listing {
        stack.push(listing, root.name.clone(), root.identity, root.bytes);
    }
    while let Some(i) = stack.top() {
        if !stack.frame(i).listed {
            walker.list(&mut stack, i)?;
            stack.frame_mut(i).listed = true;
            continue;
        }
        let frame = stack.frame(i);
        let entries = &walker.listings[frame.listing];
        let next = entries[frame.cursor..]
            .iter()
            .position(|e| e.listing.is_some())
            .map(|offset| frame.cursor + offset);
        match next {
            Some(pos) => {
                let dir = &entries[pos];
                let (listing, name, identity, bytes) = (
                    dir.listing.expect("found as a directory"),
                    dir.name.clone(),
                    dir.identity,
                    dir.bytes,
                );
                stack.frame_mut(i).cursor = pos + 1;
                stack.push(listing, name, identity, bytes);
            }
            None => {
                stack.pop();
            }
        }
    }
    Ok(WalkRecord {
        root,
        origin: candidate.path().to_path_buf(),
        mount_id: candidate.mount_id(),
        listings: walker.listings,
        markers: walker.markers,
        entries: walker.entries,
        bytes: walker.bytes,
        newest_change: if walker.times_unknown {
            None
        } else {
            walker.newest_change
        },
        upper: match &walker.overlay {
            overlay::Walk::Off => None,
            overlay::Walk::Reached { layer, .. } => Some(layer.shown(walker.upper_unread)),
            overlay::Walk::Unreached { why, .. } => Some(super::UpperLayer::Unreached(why.clone())),
        },
    })
}

/// The overlay proof for a directory candidate at `path` on an overlay
/// ([`overlay`]): its upper entry must be in its directory's upper
/// subvolume, and becomes the volume its directories are checked against;
/// where the layer is not reached, its `stx_subvol` must be its parent's.
fn prove_candidate<H: Host>(
    host: &H,
    walk: &mut overlay::Walk<H::Fd>,
    path: &Path,
    stat: &Stat,
    parent: &Stat,
) -> Result<(), WalkStop> {
    match walk {
        overlay::Walk::Off => Ok(()),
        overlay::Walk::Unreached { why, subvol } => {
            overlay::unreached(path, stat.subvol, parent.subvol, why)?;
            *subvol = stat.subvol;
            Ok(())
        }
        overlay::Walk::Reached { layer, .. } if !layer.has_subvolumes() => Ok(()),
        overlay::Walk::Reached { layer, subvolume } => {
            let volume = |at: &Path| -> Result<Option<Volume>, WalkStop> {
                match overlay::upper_stat(host, layer, at) {
                    Ok(found) => Ok(found.as_ref().map(Volume::of)),
                    Err(error) if error.is_out_of_descriptors() => Err(WalkStop::Failed {
                        path: layer.name_of(at).unwrap_or_else(|| at.to_owned()),
                        error,
                    }),
                    Err(e) => Err(overlay::unproved(path, &e.to_string()).into()),
                }
            };
            // Lower-only: so is everything below it, no proof needed.
            let Some(own) = volume(path)? else {
                return Ok(());
            };
            let dir = path.parent().unwrap_or(path);
            let outer = volume(dir)?
                .ok_or_else(|| overlay::unproved(path, "its upper entry's directory has none"))?;
            overlay::upper_boundary(path, own, outer, &layer.fs_type, &layer.at)?;
            *subvolume = Some(own);
            Ok(())
        }
    }
}

struct Walker<'w, H: Host> {
    host: &'w H,
    candidate: &'w Candidate<'w, H::Fd>,
    /// The candidate's own subvolume: a directory below in another one is
    /// a nested subvolume ([`super::same_subvolume`]).
    volume: Volume,
    /// On an overlay, its upper layer ([`overlay`]): where each entry's
    /// upper name is read, and the candidate's subvolume proof there.
    overlay: overlay::Walk<H::Fd>,
    /// Why some entry's upper name could not be read (the first).
    upper_unread: Option<String>,
    rules: &'w Rules<'w>,
    deadline: Option<MonoTime>,
    mount_points: Vec<PathBuf>,
    /// The record's listings so far (complete for every listed directory).
    listings: Vec<Vec<Entry>>,
    /// The checkout markers admitted so far ([`CheckoutMarker`]).
    markers: Vec<CheckoutMarker>,
    entries: usize,
    bytes: u64,
    /// The newest mtime or ctime so far.
    newest_change: Option<Timestamp>,
    /// Some entry's mtime or ctime was not reported.
    times_unknown: bool,
}

impl<H: Host> Walker<'_, H> {
    /// Lists frame `i`'s directory and admits every entry in it.
    fn list(&mut self, stack: &mut Stack<'_, H>, i: usize) -> Result<(), WalkStop> {
        if let Err(failed) = stack.open(i) {
            return Err(open_failure(stack.path(failed.frame, None), failed));
        }
        if deadline_passed(self.host, self.deadline) {
            return Err(WalkStop::DeadlinePassed {
                path: stack.path(i, None),
            });
        }
        let budget = self.rules.max_entries.saturating_sub(self.entries);
        let mut names = match self.host.read_dir(stack.fd(i), budget) {
            Ok(DirListing::Names(names)) => names,
            Ok(DirListing::TooMany) => {
                return Err(Skip::new(
                    Reason::TooManyEntries,
                    stack.path(i, None),
                    format!(
                        "the candidate has more than {} entries",
                        self.rules.max_entries
                    ),
                )
                .into());
            }
            Err(e) => return Err(host_failure(stack.path(i, None), "list", &e)),
        };
        names.sort();
        // By the whole listing, before any entry is admitted: a storage
        // root is told by names side by side (container_storage), and the
        // candidate itself is the first directory listed.
        super::container_storage::check_listing(&stack.path(i, None), &names)?;
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            let path = || stack.path(i, Some(&name));
            if deadline_passed(self.host, self.deadline) {
                return Err(WalkStop::DeadlinePassed { path: path() });
            }
            if is_private_tmp(&name) {
                return Err(private_tmp(path()).into());
            }
            if !self.mount_points.is_empty() {
                let at = path();
                if self.mount_points.contains(&at) {
                    return Err(mounted(at).into());
                }
            }
            let stat = match self.host.statx(Base::Dir(stack.fd(i)), Path::new(&name)) {
                Ok(stat) => stat,
                Err(e) => return Err(host_failure(path(), "stat", &e)),
            };
            let entry = self.admit(name.clone(), &stat, path)?;
            entries.push(entry);
        }
        // The directory's own times once it has been listed: an entry
        // created in it or renamed into it before or while it was listed
        // moved them (POSIX), whatever the moved entry's own ctime does on
        // this filesystem. The stat that admitted it, made while its parent
        // was listed, can predate such a change; the newest change must not
        // (protect and !profile rely on it, TASK-200).
        match self.host.statx(Base::Dir(stack.fd(i)), Path::new("")) {
            Ok(after) => self.note_times(&after),
            Err(e) => return Err(host_failure(stack.path(i, None), "stat", &e)),
        }
        let listing = stack.frame(i).listing;
        self.listings[listing] = entries;
        Ok(())
    }

    /// Folds an mtime and ctime into the newest change (unknown: the
    /// record's newest change is unknown).
    fn note_times(&mut self, stat: &Stat) {
        match (stat.mtime, stat.ctime) {
            (Some(m), Some(c)) => {
                self.newest_change = self.newest_change.max(Some(m.max(c)));
            }
            _ => self.times_unknown = true,
        }
    }

    /// Checks one entry (the rules in the module docs) and counts it; on an
    /// overlay with a reached upper layer, reads its upper entry first, for
    /// its upper name and a directory's subvolume proof.
    fn admit(
        &mut self,
        name: OsString,
        stat: &Stat,
        path: impl Fn() -> PathBuf,
    ) -> Result<Entry, WalkStop> {
        let upper = self.upper_entry(stat, &path)?;
        let mut entry = self.check(name, stat, &path, upper.as_ref())?;
        entry.upper = self.upper_name(&entry.identity, &path, upper.as_ref());
        Ok(entry)
    }

    /// The upper entry of the entry at `path` (`stat` through the
    /// overlay), where the layer was reached and it may have one; diskgc
    /// out of file descriptors is an error of the run.
    fn upper_entry(
        &self,
        stat: &Stat,
        path: impl Fn() -> PathBuf,
    ) -> Result<Option<Result<Option<Stat>, HostError>>, WalkStop> {
        let overlay::Walk::Reached { layer, .. } = &self.overlay else {
            return Ok(None);
        };
        if !stat.file_type.is_some_and(overlay::has_upper_name) {
            return Ok(None);
        }
        match overlay::upper_stat(self.host, layer, &path()) {
            Err(error) if error.is_out_of_descriptors() => Err(WalkStop::Failed {
                path: layer.name_of(&path()).unwrap_or_else(&path),
                error,
            }),
            found => Ok(Some(found)),
        }
    }

    /// An entry's name in its overlay's upper layer from its upper entry
    /// `upper` ([`Self::upper_entry`]; [`super::UpperName`]).
    fn upper_name(
        &mut self,
        identity: &Identity,
        path: impl Fn() -> PathBuf,
        upper: Option<&Result<Option<Stat>, HostError>>,
    ) -> super::UpperName {
        if !overlay::has_upper_name(identity.file_type) {
            return super::UpperName::None;
        }
        match (&self.overlay, upper) {
            (overlay::Walk::Off, _) => super::UpperName::None,
            // The record says why (`UpperLayer::Unreached`).
            (overlay::Walk::Unreached { .. }, _) => super::UpperName::Unknown,
            (overlay::Walk::Reached { layer, .. }, Some(found)) => {
                let (name, unread) = overlay::upper_name(layer, &path(), found);
                if self.upper_unread.is_none() {
                    self.upper_unread = unread;
                }
                name
            }
            // Read for every type `has_upper_name` admits.
            (overlay::Walk::Reached { .. }, None) => super::UpperName::Unknown,
        }
    }

    /// Checks one entry (the rules in the module docs) and counts it;
    /// `upper` is its upper entry on an overlay ([`Self::upper_entry`]).
    fn check(
        &mut self,
        name: OsString,
        stat: &Stat,
        path: impl Fn() -> PathBuf,
        upper: Option<&Result<Option<Stat>, HostError>>,
    ) -> Result<Entry, Skip> {
        let skip = |reason, detail: String| Skip::new(reason, path(), detail);
        let (Some(identity), Some(mnt_id)) = (Identity::of(stat), stat.mnt_id) else {
            return Err(skip(
                Reason::UnsupportedFs,
                format!("the kernel did not report its type, inode, owner and mount id: {stat:?}"),
            ));
        };
        if mnt_id != self.candidate.mount_id {
            return Err(skip(
                Reason::ContainsMount,
                format!(
                    "on mount {mnt_id}, not the candidate's mount {}",
                    self.candidate.mount_id
                ),
            ));
        }
        if identity.file_type == FileType::Directory {
            super::same_subvolume(
                &path(),
                Volume::of(stat),
                self.volume,
                Some(self.candidate.fs_type()),
            )?;
            match (&self.overlay, upper) {
                (
                    overlay::Walk::Reached {
                        layer,
                        subvolume: Some(candidate),
                    },
                    Some(found),
                ) => match found {
                    Ok(None) => {}
                    Ok(Some(upper)) => overlay::upper_boundary(
                        &path(),
                        Volume::of(upper),
                        *candidate,
                        &layer.fs_type,
                        &layer.at,
                    )?,
                    Err(e) => return Err(overlay::unproved(&path(), &e.to_string())),
                },
                (overlay::Walk::Unreached { why, subvol }, _) => {
                    overlay::unreached(&path(), stat.subvol, *subvol, why)?;
                }
                _ => {}
            }
        }
        if name == KEEP_MARKER {
            return Err(skip(Reason::KeepMarker, "inside the candidate".to_owned()));
        }
        if !self.rules.owners.contains(&identity.uid) {
            return Err(skip(
                Reason::NotOwner,
                super::not_owner_detail(identity.uid, self.rules.owners),
            ));
        }
        match identity.file_type {
            FileType::Socket | FileType::Fifo if self.rules.block_ipc => {
                return Err(skip(
                    Reason::ContainsIpc,
                    format!(
                        "a {:?} (any socket or FIFO keeps its candidate)",
                        identity.file_type
                    ),
                ));
            }
            FileType::CharDevice | FileType::BlockDevice | FileType::Unknown(_) => {
                return Err(skip(
                    Reason::ContainsSpecial,
                    format!(
                        "a {:?} (device nodes and unknown types are never deleted)",
                        identity.file_type
                    ),
                ));
            }
            FileType::Regular
            | FileType::Directory
            | FileType::Symlink
            | FileType::Socket
            | FileType::Fifo => {}
        }
        // By name alone, at any depth, tag or no tag (TASK-167): a lock
        // file Cargo holds is protected only while it stays where Cargo
        // opens it, and a lock below a profile directory's top level, or
        // in a target whose CACHEDIR.TAG is outside the candidate, has no
        // structure here to recognise it by. Refusing a non-Cargo file
        // that happens to have such a name only keeps a candidate.
        if is_cargo_lock_name(&name) {
            let known = if CARGO_LOCK_NAMES.iter().any(|lock| name == *lock) {
                "a known"
            } else {
                "an unknown"
            };
            return Err(skip(
                Reason::ContainsSpecial,
                format!(
                    "{known} Cargo lock file name (`.cargo-*lock*`): no Cargo lock file is ever \
                     deleted, wherever it is"
                ),
            ));
        }
        // Fine-grain Cargo holds this lock while it builds the unit and
        // takes it again on the next build: unlinked, a later Cargo would
        // lock a new inode while an earlier one holds the old. Fail closed
        // (TASK-205). Its path is only built for this one name.
        if name == CARGO_UNIT_LOCK_NAME && is_cargo_unit_lock_path(&path()) {
            return Err(skip(
                Reason::ContainsSpecial,
                "a fine-grain Cargo unit lock (`build/<package>/<hash>/.lock`): no Cargo lock \
                 file is ever deleted, wherever it is"
                    .to_owned(),
            ));
        }
        if is_checkout_marker(&name) {
            let at = path();
            let marker = CheckoutMarker {
                below: at
                    .strip_prefix(self.candidate.path)
                    .map_or_else(|_| at.clone(), Path::to_path_buf),
                empty_file: identity.file_type == FileType::Regular && stat.size == Some(0),
            };
            if self.rules.checkouts == Checkouts::Refuse
                && !tolerated_marker(
                    self.rules.empty_checkout_markers,
                    &marker.below,
                    marker.empty_file,
                )
            {
                return Err(skip(
                    Reason::ContainsCheckout,
                    format!(
                        "a version-control checkout's metadata ({:?}, never followed): it may \
                         hold uncommitted work, and the profile refuses checkouts (`checkouts: \
                         refuse`, the default){}",
                        identity.file_type,
                        if self.rules.empty_checkout_markers.is_empty() {
                            ""
                        } else {
                            "; only an empty regular file at a place its \
                             `empty_checkout_markers` names is tolerated"
                        }
                    ),
                ));
            }
            self.markers.push(marker);
        }
        let listing = (identity.file_type == FileType::Directory).then(|| {
            self.listings.push(Vec::new());
            self.listings.len() - 1
        });
        let bytes = stat.blocks.map_or(0, |b| b.saturating_mul(512));
        self.entries += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        self.note_times(stat);
        Ok(Entry {
            name,
            identity,
            bytes,
            listing,
            upper: super::UpperName::None,
        })
    }
}

/// Whether `name` matches `.cargo-*lock*` (any lock name, known or not):
/// what the walk refuses anywhere, and what marks a profile directory for
/// Cargo discovery.
pub fn is_cargo_lock_name(name: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes()
        .strip_prefix(b".cargo-")
        .is_some_and(|rest| rest.windows(4).any(|w| w == b"lock"))
}

/// A host error while reading what the walk must check (`what`: stat,
/// open or list `path`). Never treated as empty or as nothing to check; the
/// one place that classifies them:
///
/// - `ENOENT`: the object vanished while the walk ran, a concurrent change:
///   `changed`;
/// - `EACCES`, `EPERM`: this user may not read it (a mode-000 directory):
///   `unreadable`, the candidate is refused;
/// - anything else (`EMFILE`, `ENFILE`, `ENOMEM`, `EIO`, a malformed
///   answer): says nothing about this candidate and would hit the next one
///   too: [`WalkStop::Failed`], an error of the run.
pub(crate) fn host_failure(path: PathBuf, what: &str, e: &HostError) -> WalkStop {
    if e.is(Errno::ENOENT) {
        Skip::new(Reason::Changed, path, format!("gone while walked: {e}")).into()
    } else if e.is(Errno::EACCES) || e.is(Errno::EPERM) {
        Skip::new(
            Reason::Unreadable,
            path,
            format!("cannot {what} it (fail closed): {e}"),
        )
        .into()
    } else {
        WalkStop::Failed {
            path,
            error: e.clone(),
        }
    }
}

/// Why a directory of the walk (at `path`) could not be opened.
fn open_failure(path: PathBuf, failed: OpenFailed) -> WalkStop {
    let (reason, detail) = match failed.why {
        OpenFailure::DeadlinePassed => return WalkStop::DeadlinePassed { path },
        OpenFailure::NotSame { found } => (
            Reason::Changed,
            format!(
                "replaced while the walk ran (now {})",
                Identity::shown(found)
            ),
        ),
        OpenFailure::Resolve(ResolveError::Host(e)) => return host_failure(path, "open", &e),
        OpenFailure::Resolve(e) => match &e {
            ResolveError::UnsafePath {
                why: Unsafe::CrossesMount | Unsafe::LeftMount { .. },
                ..
            } => (Reason::ContainsMount, e.to_string()),
            ResolveError::UnsupportedFs { .. } => (Reason::UnsupportedFs, e.to_string()),
            // The walk descends with open_beneath, which never screens;
            // mapped for completeness.
            ResolveError::Screened { why, .. } => (why.skip_reason(), e.to_string()),
            // Gone, or no longer a directory (a symlink now): it changed.
            ResolveError::Missing { .. }
            | ResolveError::UnsafePath { .. }
            | ResolveError::Host(_) => (Reason::Changed, e.to_string()),
        },
    };
    WalkStop::Skipped(Skip::new(reason, path, detail))
}
