//! The `cargo-target` kind (docs/safety.md, `cargo-target`): discovery of
//! Cargo target directories below the roots, what they hold (profile
//! directories, lock files, layout), the last-build measure, eligibility,
//! the Cargo process heuristics, and the reclaim protocol that deletes.
//!
//! The data first:
//!
//! - a [`Target`] is a target directory as [`discover`] found it: its path,
//!   the root it was found under, its identity, and what [`inspect`] read
//!   in it ([`Inspected`]);
//! - an [`Inspected`] lists the target's **profile directories**
//!   (`<t>/<p>/` and `<t>/<triple>/<p>/` holding a `.cargo-*lock*` name,
//!   discovery's shape), each with its lock files (name and
//!   identity, type included) and everything else in it (name and type,
//!   [`ProfileEntry`]), plus the other directories directly in the target.
//!   Everything else is derived from it on demand: whether a lock name is
//!   known, which lock set a profile holds ([`LockSet`]), its layout
//!   ([`Layout`]);
//! - [`eligibility`] judges one target at the time it is called, for a plan
//!   or a survey: it reaches the target again from its root, inspects it
//!   again, walks every entry of its profile directories with the deletion
//!   engine's eligibility walk, measures the last build, applies the idle
//!   requirement and the in-use check (task-6's scan plus
//!   [`cargo_evidence`]). It takes no lock and deletes nothing.
//!
//! - [`reclaim`] is the reclaim protocol: lock every lock file, recheck
//!   under the locks, delete inside the locked profile directories
//!   (fingerprints first), release. It builds on the same parts: [`reach`],
//!   [`inspect`] and [`Inspected::same_locks`] (a new profile or lock file
//!   since discovery is `changed`), [`judge_layout`], the entry walks,
//!   [`last_build`] and [`cargo_evidence`].
//!
//! Nothing here follows a symlink or crosses a mount: every directory is
//! opened below its parent with `open_beneath`, and the walks are the
//! engine's.

mod discover;
mod eligible;
mod evidence;
mod reclaim;

use std::ffi::OsString;
use std::path::PathBuf;

use crate::engine::{CARGO_LOCK_NAMES, Identity, Seen};
use crate::host::{Dev, DevIno, FileType, Stat, Timestamp};

pub use discover::{
    CARGO_MANIFEST, DISCOVERY_DEPTH, DiscoverCursor, Discovery, Project, Reached, Stepped,
    discover, inspect, reach,
};
pub use eligible::{
    Eligible, ProfileWalk, Tally, eligibility, judge_layout, last_build, tally, walk_profiles,
};
pub(crate) use evidence::cargo_evidence;
pub use reclaim::{LOCK_ORDER, ReclaimPolicy, Reclaimed, reclaim, reclaim_checked};

/// Shared with the `fs` kind ([`crate::outcome`]); named here too, where
/// the Cargo kind's callers have always found them.
pub use crate::outcome::{Judged, ReclaimEnd, Stop};

/// The signature a valid `CACHEDIR.TAG` starts with (Cache Directory
/// Tagging Specification; Cargo writes it into every target directory it
/// creates).
pub const CACHEDIR_TAG_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// A Cargo target directory as discovery found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The root it was found under: keep markers from here down to the
    /// target count (docs/reference.md, Protection marker).
    pub root: PathBuf,
    /// Its path: the root and the names that led to it.
    pub path: PathBuf,
    /// Its identity when found: reached again, it must be the same object
    /// (`changed` otherwise). Its parent is the project directory of the
    /// process heuristics ([`cargo_evidence`]).
    pub identity: Identity,
    /// The space of the mount discovery found it on
    /// ([`crate::places::space_of`]; never its `st_dev`, which on btrfs is
    /// its subvolume's): `other_fs` unless it is `PATH`'s.
    pub space: Dev,
    /// What it held when found.
    pub inspected: Inspected,
    /// What the index needs of the `statx` that found it (the tally).
    pub seen: Seen,
}

/// What a target directory holds that the kind needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspected {
    /// Its profile directories, sorted by `rel`.
    pub profiles: Vec<ProfileDir>,
    /// The directories directly in the target that are not profile
    /// directories (target triples, `doc`, `tmp`, ...), sorted by `rel`:
    /// their identity for the in-use check, and the parents of the profile
    /// directories two levels down.
    pub dirs: Vec<Dir>,
    /// The first keep marker found in the target directory or in a
    /// directory directly in it, relative to the target.
    pub keep_marker: Option<PathBuf>,
}

/// A directory of a target, relative to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dir {
    pub rel: PathBuf,
    pub identity: Identity,
}

/// A profile directory: `<target>/<profile>/` or
/// `<target>/<triple>/<profile>/`, recognised by a `.cargo-*lock*` file in
/// it (any such name, known or not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileDir {
    /// Below the target: `debug`, or `x86_64-unknown-linux-gnu/release`.
    pub rel: PathBuf,
    pub identity: Identity,
    /// Its own newest mtime or ctime; `None` when the kernel did not report
    /// one (the last build is then unknown).
    pub changed: Option<Timestamp>,
    /// Every `.cargo-*lock*` file in it, known names or not, sorted by name.
    pub locks: Vec<LockFile>,
    /// Everything else in it, sorted by name: what the reclaim protocol
    /// deletes, each as a candidate of its own.
    pub entries: Vec<ProfileEntry>,
}

/// An entry of a profile directory other than a lock file, as `statx`
/// (not following it) showed it when inspected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEntry {
    pub name: OsString,
    pub file_type: FileType,
}

/// An entry of a profile directory with a `.cargo-*lock*` name. Never
/// deleted or moved (the PRD). Only a regular file is a lock file:
/// anything else with such a name refuses the target ([`judge_layout`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockFile {
    pub name: OsString,
    /// Its identity, its type (`statx`, not followed) included.
    pub identity: Identity,
    /// Its newest mtime or ctime (`None`: not reported).
    pub changed: Option<Timestamp>,
}

impl LockFile {
    /// Whether its name is on the allowlist of known lock names
    /// ([`CARGO_LOCK_NAMES`]).
    pub fn is_known(&self) -> bool {
        CARGO_LOCK_NAMES.iter().any(|known| self.name == *known)
    }
}

/// Which known lock files a profile directory holds, as measured per Cargo
/// version (docs/safety.md, `cargo-target`: Lock files).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockSet {
    /// A whole profile: `.cargo-lock` alone (Cargo 1.90 and older),
    /// with `.cargo-build-lock` (`cargo check` of 1.99.1 and the nightly,
    /// the 1.96 nightly), or with both newer locks (a build of 1.99.1 and
    /// the nightly).
    Whole,
    /// `.cargo-build-lock` without `.cargo-lock`: the build directory's
    /// half of a split build directory (`CARGO_BUILD_BUILD_DIR`, 1.99.1 and
    /// the nightly).
    SplitBuildDir,
    /// `.cargo-lock` and `.cargo-artifact-lock` without
    /// `.cargo-build-lock`: the target directory's half of a split build
    /// directory.
    SplitArtifactDir,
    /// Any other combination: never measured.
    Unmeasured,
}

impl LockSet {
    /// What the report says about a set that is not whole.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Whole => "a whole profile",
            Self::SplitBuildDir => {
                "the build directory's half of a split build directory (only \
                 .cargo-build-lock): its other half is not known, and both halves would have to \
                 be locked together"
            }
            Self::SplitArtifactDir => {
                "the target directory's half of a split build directory (.cargo-lock and \
                 .cargo-artifact-lock without .cargo-build-lock): its other half is not known, \
                 and both halves would have to be locked together"
            }
            Self::Unmeasured => "a combination of lock files no measured Cargo makes",
        }
    }
}

/// Where a profile directory keeps its fingerprints, which the reclaim
/// protocol deletes first so that a half-deleted profile always rebuilds
/// (docs/safety.md, `cargo-target`: Reclaim protocol).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `.fingerprint/` (and `deps/`, `build/<pkg>-<hash>/`) in the profile
    /// directory: Cargo 1.90, 1.99.1, and the build directory's half of a
    /// split build directory.
    Classic,
    /// The new build-dir layout (the 2026-10-16 nightly's default): no
    /// `.fingerprint/` and no `deps/`; each unit in
    /// `build/<package>/<hash>/` with its `fingerprint/` and `out/`.
    PerUnit,
}

impl ProfileDir {
    /// Whether `other` is the same profile directory (place and identity)
    /// with the same lock files (names and identities, types included).
    pub fn same_locks(&self, other: &ProfileDir) -> bool {
        self.rel == other.rel
            && self.identity == other.identity
            && self.locks.len() == other.locks.len()
            && self
                .locks
                .iter()
                .zip(&other.locks)
                .all(|(a, b)| a.name == b.name && a.identity == b.identity)
    }

    /// The lock files whose names are not on the allowlist.
    pub fn unknown_locks(&self) -> impl Iterator<Item = &LockFile> {
        self.locks.iter().filter(|l| !l.is_known())
    }

    fn has_lock(&self, name: &str) -> bool {
        self.locks.iter().any(|l| l.name == name)
    }

    /// Which known lock set it holds (unknown names are judged apart:
    /// [`ProfileDir::unknown_locks`]).
    pub fn lock_set(&self) -> LockSet {
        match (
            self.has_lock(".cargo-lock"),
            self.has_lock(".cargo-build-lock"),
            self.has_lock(".cargo-artifact-lock"),
        ) {
            (true, false, false) | (true, true, _) => LockSet::Whole,
            (false, true, false) => LockSet::SplitBuildDir,
            (true, false, true) => LockSet::SplitArtifactDir,
            (false, _, _) => LockSet::Unmeasured,
        }
    }

    /// It holds nothing but lock files.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether it holds a directory (a real one: not a symlink, not a
    /// file) called `name`.
    fn has_dir(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e.name == name && e.file_type == FileType::Directory)
    }

    /// Its layout, by where its fingerprints are: a directory `.fingerprint`
    /// or `deps` in it is [`Layout::Classic`] (a profile whose
    /// `.fingerprint/` the protocol already deleted keeps `deps/`), else a
    /// directory `build` is [`Layout::PerUnit`]. `None`: neither, an unknown
    /// layout. Only directories count: a file or a symlink with one of those
    /// names is not Cargo's (a non-Cargo cache must not pass by its names).
    pub fn layout(&self) -> Option<Layout> {
        if self.has_dir(".fingerprint") || self.has_dir("deps") {
            Some(Layout::Classic)
        } else if self.has_dir("build") {
            Some(Layout::PerUnit)
        } else {
            None
        }
    }
}

impl Inspected {
    /// Whether `other` has the same profile directories with the same lock
    /// files (names and identities): a profile or a lock file created,
    /// removed or replaced in between makes them differ (the reclaim
    /// protocol's `changed`, docs/safety.md). Other entries do not count:
    /// a build changes them, and the last build measures that.
    pub fn same_locks(&self, other: &Inspected) -> bool {
        self.profiles.len() == other.profiles.len()
            && self
                .profiles
                .iter()
                .zip(&other.profiles)
                .all(|(a, b)| a.same_locks(b))
    }

    /// The (device, inode) of every object of the target outside the
    /// profile directories' entries: the directories directly in it, the
    /// profile directories and their lock files (the target directory's own
    /// is the caller's). What the in-use check matches by identity besides
    /// the entries' walk records.
    pub fn objects(&self) -> impl Iterator<Item = (PathBuf, DevIno)> + '_ {
        let dirs = self.dirs.iter().map(|d| (d.rel.clone(), d.identity.id));
        let profiles = self.profiles.iter().flat_map(|p| {
            std::iter::once((p.rel.clone(), p.identity.id))
                .chain(p.locks.iter().map(|l| (p.rel.join(&l.name), l.identity.id)))
        });
        dirs.chain(profiles)
    }
}

/// The newest of a stat's mtime and ctime; `None` when either is not
/// reported (the age is then unknown, never assumed).
fn newest_change(stat: &Stat) -> Option<Timestamp> {
    Some(stat.mtime?.max(stat.ctime?))
}
