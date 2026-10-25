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
//! 6. it is not a device node or of unknown type, does not have a Cargo
//!    lock file's name ([`CARGO_LOCK_NAMES`], anywhere), and is not a lock
//!    file of a Cargo target inside the candidate (the structural
//!    recogniser) (`contains_special`);
//! 7. unless [`Rules::checkouts`] is [`Checkouts::Allow`] (a profile
//!    that says `checkouts: allow`), it does not have a VCS checkout
//!    marker's name ([`CHECKOUT_MARKERS`], whatever its type, anywhere, the
//!    candidate itself included) (`contains_checkout`).
//!
//! The first entry that fails stops the walk. Host errors on the way are
//! classified in one place, [`host_failure`]: fail closed, never treated as
//! empty.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::host::{Base, DirListing, Errno, FileType, Host, HostError, MonoTime, Stat, Timestamp};
use crate::resolve::{ResolveError, Unsafe};

use super::frames::{OpenFailed, OpenFailure, Stack};
use super::{Candidate, Entry, Identity, Reason, Skip, WalkRecord, deadline_passed};

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

/// The file that marks a Cargo target directory for the recogniser.
pub const CACHEDIR_TAG: &str = "CACHEDIR.TAG";

/// The known Cargo lock file names (docs/safety.md, `cargo-target`, Lock
/// files). An entry of one of these names anywhere in a candidate, the
/// candidate itself included, refuses it: no Cargo lock file is ever
/// deleted (the PRD), whatever directory the candidate is.
pub const CARGO_LOCK_NAMES: &[&str] = &[".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

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
/// tree before statx; checks the deadline before every `statx`, open and
/// listing.
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
    let mount_points: Vec<PathBuf> = host
        .mountinfo()
        .map_err(|error| WalkStop::Failed {
            path: PathBuf::from("/proc/self/mountinfo"),
            error,
        })?
        .into_iter()
        .map(|mount| mount.mount_point)
        .filter(|mount| mount.starts_with(candidate.path))
        .collect();
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
    let mut walker = Walker {
        host,
        candidate,
        rules,
        deadline,
        mount_points,
        listings: Vec::new(),
        entries: 0,
        bytes: 0,
        newest_change: None,
        times_unknown: false,
    };
    let root = walker.admit(None, candidate.name.to_owned(), &stat, path)?;
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
        entries: walker.entries,
        bytes: walker.bytes,
        newest_change: if walker.times_unknown {
            None
        } else {
            walker.newest_change
        },
    })
}

struct Walker<'w, H: Host> {
    host: &'w H,
    candidate: &'w Candidate<'w, H::Fd>,
    rules: &'w Rules<'w>,
    deadline: Option<MonoTime>,
    mount_points: Vec<PathBuf>,
    /// The record's listings so far (complete for every listed directory).
    listings: Vec<Vec<Entry>>,
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
            // Where a Cargo target holding this directory as a profile
            // directory would be: its parent, or its grandparent (a target
            // triple between), inside the candidate.
            let targets =
                [i.checked_sub(1), i.checked_sub(2)].map(|k| k.map(|k| stack.frame(k).listing));
            let entry = self.admit(Some(targets), name.clone(), &stat, path)?;
            entries.push(entry);
        }
        let listing = stack.frame(i).listing;
        self.listings[listing] = entries;
        Ok(())
    }

    /// Checks one entry (the rules in the module docs) and counts it.
    /// `targets` are the listings of its directory's parent and grandparent
    /// inside the candidate, where a Cargo target would be if the entry is
    /// a lock file (`None` for the candidate itself, which is in no
    /// directory of the candidate).
    fn admit(
        &mut self,
        targets: Option<[Option<usize>; 2]>,
        name: OsString,
        stat: &Stat,
        path: impl Fn() -> PathBuf,
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
        if name == KEEP_MARKER {
            return Err(skip(Reason::KeepMarker, "inside the candidate".to_owned()));
        }
        if !self.rules.owners.contains(&identity.uid) {
            return Err(skip(
                Reason::NotOwner,
                format!(
                    "owned by uid {}, not by {:?}",
                    identity.uid, self.rules.owners
                ),
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
        if CARGO_LOCK_NAMES.iter().any(|lock| name == *lock) {
            return Err(skip(
                Reason::ContainsSpecial,
                "a Cargo lock file name (no Cargo lock file is ever deleted, wherever it is)"
                    .to_owned(),
            ));
        }
        if let Some(targets) = targets
            && is_cargo_lock_name(&name)
            && let Some(up) = targets
                .iter()
                .position(|t| t.is_some_and(|listing| self.has_tag(listing)))
        {
            let target = ["parent", "grandparent"][up];
            return Err(skip(
                Reason::ContainsSpecial,
                format!(
                    "a Cargo lock file in a profile directory of a Cargo target (its directory's \
                     {target} holds {CACHEDIR_TAG})"
                ),
            ));
        }
        if self.rules.checkouts == Checkouts::Refuse && is_checkout_marker(&name) {
            return Err(skip(
                Reason::ContainsCheckout,
                format!(
                    "a version-control checkout's metadata ({:?}, never followed): it may hold \
                     uncommitted work, and the profile refuses checkouts (`checkouts: refuse`, \
                     the default)",
                    identity.file_type
                ),
            ));
        }
        let listing = (identity.file_type == FileType::Directory).then(|| {
            self.listings.push(Vec::new());
            self.listings.len() - 1
        });
        let bytes = stat.blocks.map_or(0, |b| b.saturating_mul(512));
        self.entries += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        match (stat.mtime, stat.ctime) {
            (Some(m), Some(c)) => {
                self.newest_change = self.newest_change.max(Some(m.max(c)));
            }
            _ => self.times_unknown = true,
        }
        Ok(Entry {
            name,
            identity,
            bytes,
            listing,
        })
    }

    /// Whether the listed directory holds `CACHEDIR.TAG`: with a lock file
    /// in a directory one or two levels below, it is a Cargo target for the
    /// structural recogniser (docs/safety.md: `<t>/<p>/` or
    /// `<t>/<triple>/<p>/`). The tag's content is not read: any entry of
    /// that name counts, which errs on the side of keeping.
    fn has_tag(&self, listing: usize) -> bool {
        self.listings[listing]
            .iter()
            .any(|e| e.name == CACHEDIR_TAG)
    }
}

/// Whether `name` matches `.cargo-*lock*` (any lock name, known or not).
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
            format!("replaced while the walk ran (now {found:?})"),
        ),
        OpenFailure::Resolve(ResolveError::Host(e)) => return host_failure(path, "open", &e),
        OpenFailure::Resolve(e) => match &e {
            ResolveError::UnsafePath {
                why: Unsafe::CrossesMount | Unsafe::LeftMount { .. },
                ..
            } => (Reason::ContainsMount, e.to_string()),
            ResolveError::UnsupportedFs { .. } => (Reason::UnsupportedFs, e.to_string()),
            // Gone, or no longer a directory (a symlink now): it changed.
            ResolveError::Missing { .. }
            | ResolveError::UnsafePath { .. }
            | ResolveError::Host(_) => (Reason::Changed, e.to_string()),
        },
    };
    WalkStop::Skipped(Skip::new(reason, path, detail))
}
