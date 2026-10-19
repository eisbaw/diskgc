//! The deletion oracle (docs/testing.md, Grounding, acceptance signal 1).
//!
//! A fixture declares which objects are eligible to be deleted. The oracle
//! snapshots the whole container filesystem before and after the code under
//! test runs; any object outside the eligible set that disappeared from its
//! path, or was replaced there (same path, different (dev, ino, btime)), is a
//! violation, whatever the code under test reported. The strace record is a
//! second, independent view: any traced unlink, rmdir or rename that took an
//! object present before the run away from (or replaced it at) a path
//! outside the eligible set is a violation too.
//!
//! The verdict is computed from the fixture's declaration only, never from
//! the output of the code under test. The only allowlisted paths are diskgc's
//! own state directory ([`DISKGC_STATE_DIR`]) and the concurrent writers the
//! fixture declares. Objects that appear are not violations (Cargo creates
//! files); objects created and removed within the run are not either.
//! Traced calls on paths the before snapshot does not have are classified
//! and logged ([`Unmatched`]), never dropped silently.
//!
//! Judging is mandatory: every traced run is entered in its test's
//! [`crate::under_test::Ledger`], [`Oracle::judge_runs`] marks the runs it
//! judged, and the test fails if one is left unjudged (`suite::run`).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::info;
use crate::snapshot::{self, Entry, Kind, Snapshot};
use crate::strace::Event;
use crate::under_test::Run;

/// diskgc's state directory for the `tester` user: `$XDG_STATE_HOME/diskgc`
/// with the XDG default (docs/architecture.md, index snapshot).
pub const DISKGC_STATE_DIR: &str = "/home/tester/.local/state/diskgc";

/// What a scenario declares about deletions.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    /// Objects (with everything below them) that may disappear or be replaced.
    pub eligible: Vec<PathBuf>,
    /// Subtrees where the scenario's own concurrent writers may change things.
    pub concurrent_writers: Vec<PathBuf>,
}

impl Fixture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn eligible(mut self, path: impl Into<PathBuf>) -> Self {
        self.eligible.push(path.into());
        self
    }

    /// Adds every eligible path of `other` (a second target in one run).
    pub fn eligible_from(mut self, other: Fixture) -> Self {
        self.eligible.extend(other.eligible);
        self
    }

    pub fn concurrent_writer(mut self, path: impl Into<PathBuf>) -> Self {
        self.concurrent_writers.push(path.into());
        self
    }

    /// Whether a change at `path` is expected (eligible) or allowlisted.
    pub fn covers(&self, path: &Path) -> bool {
        let below = |roots: &[PathBuf]| roots.iter().any(|r| path.starts_with(r));
        below(&self.eligible)
            || below(&self.concurrent_writers)
            || path.starts_with(DISKGC_STATE_DIR)
    }
}

/// A read-only view of one fixture for a judgement. Looking up each path's
/// ancestors bounds the number of probes by path depth, regardless of how
/// many eligible roots the fixture declares. It is rebuilt from the public
/// fixture fields for each judgement, so edits to those fields cannot leave
/// a stale index.
struct Coverage<'a> {
    roots: HashSet<&'a Path>,
    /// An empty root is a prefix of every path, including absolute paths;
    /// absolute paths do not have an empty ancestor to probe.
    covers_all: bool,
}

impl<'a> Coverage<'a> {
    fn new(fixture: &'a Fixture) -> Self {
        let covers_all = fixture
            .eligible
            .iter()
            .chain(&fixture.concurrent_writers)
            .any(|root| root.as_os_str().is_empty());
        let roots = fixture
            .eligible
            .iter()
            .chain(&fixture.concurrent_writers)
            .map(PathBuf::as_path)
            .chain(std::iter::once(Path::new(DISKGC_STATE_DIR)))
            .collect();
        Self { roots, covers_all }
    }

    fn covers(&self, path: &Path) -> bool {
        self.covers_all || self.covers_if(path, |ancestor| self.roots.contains(ancestor))
    }

    fn covers_if(&self, path: &Path, contains: impl FnMut(&Path) -> bool) -> bool {
        path.ancestors().any(contains)
    }
}

/// How an object outside the eligible set changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Deleted { before: Entry },
    Replaced { before: Entry, after: Entry },
}

/// A change the snapshots show outside the eligible set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: PathBuf,
    pub change: Change,
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::File => "file",
        Kind::Dir => "directory",
        Kind::Symlink => "symlink",
        Kind::Other => "special file",
    }
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.change {
            Change::Deleted { before } => write!(
                f,
                "DELETED {} {} ({}; key {})",
                kind_name(before.kind),
                self.path.display(),
                before.key,
                before.key.kind()
            ),
            Change::Replaced { before, after } => write!(
                f,
                "REPLACED {} {}: before {} {}, after {} {} (key {})",
                kind_name(before.kind),
                self.path.display(),
                kind_name(before.kind),
                before.key,
                kind_name(after.kind),
                after.key,
                if before.key.btime.is_some() && after.key.btime.is_some() {
                    before.key.kind()
                } else {
                    "(path, dev, ino)"
                }
            ),
        }
    }
}

/// A traced call that changed a path outside the eligible set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TracedViolation {
    pub path: PathBuf,
    pub pid: u32,
    pub syscall: String,
    /// `removed` (unlink/rmdir target, rename source) or `replaced` (rename
    /// target).
    pub how: &'static str,
}

impl std::fmt::Display for TracedViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TRACED {} {} by pid {} ({})",
            self.how,
            self.path.display(),
            self.pid,
            self.syscall
        )
    }
}

/// Snapshot diff outside the fixture's eligible set and allowlist.
pub fn diff(
    before: &BTreeMap<PathBuf, Entry>,
    after: &BTreeMap<PathBuf, Entry>,
    fixture: &Fixture,
) -> Vec<Violation> {
    diff_with_coverage(before, after, &Coverage::new(fixture))
}

fn diff_with_coverage(
    before: &BTreeMap<PathBuf, Entry>,
    after: &BTreeMap<PathBuf, Entry>,
    coverage: &Coverage<'_>,
) -> Vec<Violation> {
    before
        .iter()
        .filter(|(path, _)| !coverage.covers(path))
        .filter_map(|(path, old)| {
            let change = match after.get(path) {
                None => Change::Deleted { before: *old },
                Some(new) if !old.key.same_object(&new.key) || old.kind != new.kind => {
                    Change::Replaced {
                        before: *old,
                        after: *new,
                    }
                }
                Some(_) => return None,
            };
            Some(Violation {
                path: path.clone(),
                change,
            })
        })
        .collect()
}

/// Traced calls that removed or replaced, outside the fixture's eligible set
/// and allowlist, an object that existed before the run.
pub fn traced_violations(
    events: &[Event],
    before: &BTreeMap<PathBuf, Entry>,
    fixture: &Fixture,
) -> Vec<TracedViolation> {
    traced_violations_with_coverage(events, before, &Coverage::new(fixture))
}

fn traced_violations_with_coverage(
    events: &[Event],
    before: &BTreeMap<PathBuf, Entry>,
    coverage: &Coverage<'_>,
) -> Vec<TracedViolation> {
    let mut found = Vec::new();
    for event in events.iter().filter(|e| e.took_effect()) {
        let changed = event
            .removed
            .iter()
            .map(|p| (p, "removed"))
            .chain(event.replaced.iter().map(|p| (p, "replaced")));
        for (path, how) in changed {
            if before.contains_key(path) && !coverage.covers(path) {
                found.push(TracedViolation {
                    path: path.clone(),
                    pid: event.pid,
                    syscall: event.syscall.clone(),
                    how,
                });
            }
        }
    }
    found
}

/// Why a traced removal or replacement names a path that is not in the
/// "before" snapshot. Only [`Unmatched::CreatedDuringRun`] is expected; the
/// others mean the strace view could not check the call by path (the
/// snapshot view still judges the object), so they are logged one by one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unmatched {
    /// The nearest ancestor the snapshots know is a directory: the object
    /// was created during the run (objects created and removed within a run
    /// are not violations).
    CreatedDuringRun,
    /// Relative, or with `.` or `..` components: not the path the snapshot
    /// keys the object by.
    NonCanonical,
    /// A directory component carries the ` (deleted)` suffix strace's `-y`
    /// prints for an fd whose directory was removed.
    DeletedDirectory,
    /// The nearest ancestor the snapshots know is this symlink: the object
    /// was reached through it and is keyed by its target path.
    SymlinkPrefix(PathBuf),
    /// No ancestor is a directory in either snapshot (e.g. below a file, or
    /// below a mount the snapshot skips).
    NoKnownParent,
    /// Relative to a directory fd strace could not name (its path is longer
    /// than a page: a tree deeper than `PATH_MAX`); see
    /// [`crate::strace::UNNAMED_FD_PREFIX`].
    UnnamedDirFd,
}

impl std::fmt::Display for Unmatched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unmatched::CreatedDuringRun => f.write_str("created during the run"),
            Unmatched::NonCanonical => f.write_str("non-canonical path (relative, . or ..)"),
            Unmatched::DeletedDirectory => f.write_str("directory fd of a deleted directory"),
            Unmatched::SymlinkPrefix(link) => {
                write!(f, "reached through the symlink {}", link.display())
            }
            Unmatched::NoKnownParent => f.write_str("no ancestor directory in either snapshot"),
            Unmatched::UnnamedDirFd => {
                f.write_str("relative to a directory fd strace could not name (path too long)")
            }
        }
    }
}

/// A traced call that took effect on a path the "before" snapshot does not
/// have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmatchedRemoval {
    pub path: PathBuf,
    pub pid: u32,
    pub syscall: String,
    /// `removed` or `replaced`, as in [`TracedViolation`].
    pub how: &'static str,
    pub why: Unmatched,
}

impl std::fmt::Display for UnmatchedRemoval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "UNMATCHED {} {} by pid {} ({}): {}",
            self.how,
            self.path.display(),
            self.pid,
            self.syscall,
            self.why
        )
    }
}

/// Classifies a path that is in neither snapshot's key set (see
/// [`Unmatched`]).
pub fn classify_unmatched(
    path: &Path,
    before: &BTreeMap<PathBuf, Entry>,
    after: &BTreeMap<PathBuf, Entry>,
) -> Unmatched {
    // On the bytes: `Path::components` hides `.`, `//` and a trailing `/`,
    // which the snapshot's paths never have.
    let bytes = path.as_os_str().as_bytes();
    if bytes.starts_with(crate::strace::UNNAMED_FD_PREFIX.as_bytes()) {
        return Unmatched::UnnamedDirFd;
    }
    let lexically_canonical = bytes == b"/"
        || bytes.strip_prefix(b"/").is_some_and(|rest| {
            rest.split(|b| *b == b'/')
                .all(|seg| !matches!(seg, b"" | b"." | b".."))
        });
    if !lexically_canonical {
        return Unmatched::NonCanonical;
    }
    let deleted_dir = path.parent().is_some_and(|parent| {
        parent
            .components()
            .any(|c| c.as_os_str().as_bytes().ends_with(b" (deleted)"))
    });
    if deleted_dir {
        return Unmatched::DeletedDirectory;
    }
    for ancestor in path.ancestors().skip(1) {
        let known = before.get(ancestor).or_else(|| after.get(ancestor));
        match known.map(|e| e.kind) {
            None => continue,
            Some(Kind::Dir) => return Unmatched::CreatedDuringRun,
            Some(Kind::Symlink) => return Unmatched::SymlinkPrefix(ancestor.to_path_buf()),
            Some(Kind::File | Kind::Other) => return Unmatched::NoKnownParent,
        }
    }
    Unmatched::NoKnownParent
}

/// Traced calls that took effect on paths the "before" snapshot does not
/// have, classified. They are not violations: the snapshot diff judges the
/// objects; this only makes sure no traced call is dropped unexplained.
pub fn unmatched_removals(
    events: &[Event],
    before: &BTreeMap<PathBuf, Entry>,
    after: &BTreeMap<PathBuf, Entry>,
) -> Vec<UnmatchedRemoval> {
    let mut found = Vec::new();
    for event in events.iter().filter(|e| e.took_effect()) {
        let changed = event
            .removed
            .iter()
            .map(|p| (p, "removed"))
            .chain(event.replaced.iter().map(|p| (p, "replaced")));
        for (path, how) in changed.filter(|(p, _)| !before.contains_key(*p)) {
            found.push(UnmatchedRemoval {
                path: path.clone(),
                pid: event.pid,
                syscall: event.syscall.clone(),
                how,
                why: classify_unmatched(path, before, after),
            });
        }
    }
    found
}

/// How many suspicious unmatched calls are logged one by one.
const UNMATCHED_LOG_LIMIT: usize = 50;

/// Log lines for the unmatched calls: every suspicious one (up to
/// [`UNMATCHED_LOG_LIMIT`], then a count), and the expected ones
/// ([`Unmatched::CreatedDuringRun`]) as a count with a few examples.
pub fn unmatched_log(unmatched: &[UnmatchedRemoval]) -> Vec<String> {
    let (created, suspicious): (Vec<_>, Vec<_>) = unmatched
        .iter()
        .partition(|u| u.why == Unmatched::CreatedDuringRun);
    let mut lines: Vec<String> = suspicious
        .iter()
        .take(UNMATCHED_LOG_LIMIT)
        .map(|u| u.to_string())
        .collect();
    if suspicious.len() > UNMATCHED_LOG_LIMIT {
        lines.push(format!(
            "... and {} more unmatched traced calls not shown",
            suspicious.len() - UNMATCHED_LOG_LIMIT
        ));
    }
    if !created.is_empty() {
        let examples: Vec<String> = created
            .iter()
            .take(3)
            .map(|u| u.path.display().to_string())
            .collect();
        lines.push(format!(
            "{} traced calls on objects created during the run (not in the before snapshot), e.g. {examples:?}",
            created.len()
        ));
    }
    lines
}

/// The oracle's judgement of one run.
#[must_use = "a verdict nobody asserts on judges nothing"]
#[derive(Debug, Clone)]
pub struct Verdict {
    pub snapshot: Vec<Violation>,
    pub traced: Vec<TracedViolation>,
    /// Traced calls on paths the before snapshot does not have (logged, not
    /// violations).
    pub unmatched: Vec<UnmatchedRemoval>,
}

impl Verdict {
    /// Paths the snapshot diff flags.
    pub fn snapshot_paths(&self) -> BTreeSet<PathBuf> {
        self.snapshot.iter().map(|v| v.path.clone()).collect()
    }

    /// Paths the strace record flags.
    pub fn traced_paths(&self) -> BTreeSet<PathBuf> {
        self.traced.iter().map(|v| v.path.clone()).collect()
    }

    pub fn is_clean(&self) -> bool {
        self.snapshot.is_empty() && self.traced.is_empty()
    }

    /// Every violation, one per line.
    pub fn report(&self) -> String {
        let mut lines: Vec<String> = self.snapshot.iter().map(|v| v.to_string()).collect();
        lines.extend(self.traced.iter().map(|v| v.to_string()));
        if lines.is_empty() {
            "no violations".to_owned()
        } else {
            lines.join("\n")
        }
    }

    /// Fails the test with the full report unless nothing outside the
    /// eligible set changed.
    pub fn assert_clean(&self) -> Result<(), String> {
        if self.is_clean() {
            Ok(())
        } else {
            Err(format!(
                "deletion oracle: objects outside the eligible set changed:\n{}",
                self.report()
            ))
        }
    }
}

/// An armed oracle: the fixture and the snapshot taken before the run.
#[derive(Debug)]
pub struct Oracle {
    fixture: Fixture,
    before: Snapshot,
    /// When the before snapshot was complete; runs judged by this oracle
    /// must have started after it.
    armed_at: Instant,
}

impl Oracle {
    /// Takes the "before" snapshot. Fails if an eligible path does not exist
    /// (a fixture typo would otherwise make the eligible set silently empty).
    pub fn arm(fixture: Fixture) -> Result<Self, String> {
        let before = snapshot::take()?;
        let armed_at = Instant::now();
        info!(
            "oracle armed: {} objects in {:?}; {}; mounts not walked: {}",
            before.entries.len(),
            before.took,
            before.key_stats,
            skipped_text(&before)
        );
        let missing: Vec<_> = fixture
            .eligible
            .iter()
            .filter(|p| !before.entries.contains_key(*p))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "fixture declares eligible paths that do not exist: {missing:?}"
            ));
        }
        Ok(Self {
            fixture,
            before,
            armed_at,
        })
    }

    /// Takes the "after" snapshot and judges the run.
    pub fn judge(&self, run: &Run) -> Result<Verdict, String> {
        self.judge_runs(&[run])
    }

    /// Takes the "after" snapshot and judges several runs of code under test
    /// that happened since [`Oracle::arm`], and marks them judged in their
    /// test's ledger. Fails for a run that started before the oracle was
    /// armed: its deletions could predate the before snapshot.
    pub fn judge_runs(&self, runs: &[&Run]) -> Result<Verdict, String> {
        let early: Vec<&Vec<String>> = runs
            .iter()
            .filter(|r| r.started < self.armed_at)
            .map(|r| &r.argv)
            .collect();
        if !early.is_empty() {
            return Err(format!(
                "deletion oracle: run(s) started before the oracle was armed cannot be judged \
                 by it: {early:?}"
            ));
        }
        let after = snapshot::take()?;
        let events: Vec<Event> = runs.iter().flat_map(|r| r.events.iter().cloned()).collect();
        let argvs: Vec<&Vec<String>> = runs.iter().map(|r| &r.argv).collect();
        info!(
            "oracle after {argvs:?}: {} objects in {:?}; {}",
            after.entries.len(),
            after.took,
            after.key_stats
        );
        let coverage = Coverage::new(&self.fixture);
        let verdict = Verdict {
            snapshot: diff_with_coverage(&self.before.entries, &after.entries, &coverage),
            traced: traced_violations_with_coverage(&events, &self.before.entries, &coverage),
            unmatched: unmatched_removals(&events, &self.before.entries, &after.entries),
        };
        for line in unmatched_log(&verdict.unmatched) {
            info!("oracle: {line}");
        }
        let unnamed: Vec<&crate::strace::Unnamed> =
            runs.iter().flat_map(|r| r.unnamed_calls.iter()).collect();
        if !unnamed.is_empty() {
            info!(
                "oracle: {} call(s) strace could not name (no path to check; the snapshot view \
                 judges their effect): {:?}",
                unnamed.len(),
                unnamed.iter().take(5).collect::<Vec<_>>()
            );
        }
        info!(
            "oracle verdict ({} traced calls): {}",
            events.len(),
            verdict.report()
        );
        for run in runs {
            run.mark_judged();
        }
        Ok(verdict)
    }
}

/// `path (why), ...` for the mounts a snapshot did not walk.
fn skipped_text(snapshot: &Snapshot) -> String {
    snapshot
        .skipped
        .iter()
        .map(|(path, why)| format!("{} ({why})", path.display()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Key;
    use crate::strace::Outcome;

    fn entry(ino: u64, btime: Option<(i64, u32)>) -> Entry {
        Entry {
            kind: Kind::File,
            key: Key {
                dev: (0, 42),
                ino,
                btime,
            },
        }
    }

    fn tree(items: &[(&str, Entry)]) -> BTreeMap<PathBuf, Entry> {
        items.iter().map(|(p, e)| (PathBuf::from(p), *e)).collect()
    }

    fn fixture() -> Fixture {
        Fixture::new()
            .eligible("/tmp/cand")
            .concurrent_writer("/tmp/writer")
    }

    #[test]
    fn indexed_coverage_preserves_component_wise_prefixes_and_the_state_allowlist() {
        let fixture = fixture().eligible("relative/ok").eligible("/tmp/cand/.");
        let indexed = Coverage::new(&fixture);
        for (path, covered) in [
            ("/tmp/cand", true),
            ("/tmp/cand/child", true),
            ("/tmp/candidate", false),
            ("/tmp/cand-sibling", false),
            ("/tmp", false),
            ("/tmp/writer/log", true),
            ("/tmp/writers/log", false),
            ("relative/ok/child", true),
            ("relative/okay", false),
            ("/home/tester/.local/state/diskgc/index", true),
            ("/home/tester/.local/state/diskgc-old/index", false),
        ] {
            let path = Path::new(path);
            assert_eq!(fixture.covers(path), covered, "linear: {path:?}");
            assert_eq!(indexed.covers(path), covered, "indexed: {path:?}");
        }
        for path in [
            "/tmp/cand/../other",
            "/tmp/./cand/child",
            "//tmp//cand//child",
            "relative/ok/../other",
        ] {
            let path = Path::new(path);
            assert_eq!(indexed.covers(path), fixture.covers(path), "{path:?}");
        }

        // Public fixture fields can contain an empty prefix. Preserve the
        // old Path::starts_with behavior for that input as well.
        let any = Fixture::new().concurrent_writer(PathBuf::new());
        for path in [Path::new("/tmp/x"), Path::new("relative/x")] {
            assert_eq!(Coverage::new(&any).covers(path), any.covers(path));
        }
    }

    #[test]
    fn coverage_probes_are_bounded_by_path_depth_with_many_eligible_roots() {
        let mut fixture = Fixture::new();
        fixture.eligible = (0..16_000)
            .map(|n| PathBuf::from(format!("/tmp/eligible/{n}")))
            .collect();
        let indexed = Coverage::new(&fixture);
        let covered = Path::new("/tmp/eligible/15999/deep");
        let missing = Path::new("/tmp/other/deep");
        assert_eq!(indexed.roots.len(), 16_001); // eligible roots and the state allowlist
        for (path, expected, expected_probes) in [(covered, true, 2), (missing, false, 4)] {
            let mut probes = 0;
            let result = indexed.covers_if(path, |ancestor| {
                probes += 1;
                indexed.roots.contains(ancestor)
            });
            assert_eq!(result, expected, "{path:?}");
            assert_eq!(probes, expected_probes, "{path:?}");
        }
    }

    #[test]
    fn coverage_reflects_edits_to_public_fixture_fields_on_the_next_judgement() {
        let before = tree(&[("/tmp/newly-eligible/file", entry(1, None))]);
        let after = tree(&[]);
        let mut fixture = Fixture::new();
        assert_eq!(diff(&before, &after, &fixture).len(), 1);
        fixture.eligible.push("/tmp/newly-eligible".into());
        assert!(diff(&before, &after, &fixture).is_empty());
    }

    #[test]
    fn deletions_inside_the_eligible_set_and_allowlist_are_not_violations() {
        let before = tree(&[
            ("/tmp/cand", entry(1, None)),
            ("/tmp/cand/x", entry(2, None)),
            ("/tmp/writer/log", entry(3, None)),
            ("/home/tester/.local/state/diskgc/index-1", entry(4, None)),
            ("/tmp/keep", entry(5, None)),
        ]);
        let after = tree(&[("/tmp/keep", entry(5, None))]);
        assert!(diff(&before, &after, &fixture()).is_empty());
    }

    #[test]
    fn deletion_outside_the_eligible_set_is_a_violation() {
        let before = tree(&[
            ("/tmp/candidate-sibling", entry(1, None)),
            ("/etc/x", entry(2, None)),
        ]);
        let after = tree(&[]);
        let v = diff(&before, &after, &fixture());
        // "/tmp/candidate-sibling" shares a string prefix with "/tmp/cand"
        // but is not below it.
        assert_eq!(v.len(), 2);
        assert!(v.iter().all(|v| matches!(v.change, Change::Deleted { .. })));
    }

    #[test]
    fn replacement_by_inode_or_birth_time_is_a_violation() {
        let before = tree(&[
            ("/tmp/lock", entry(1, Some((100, 0)))),
            ("/tmp/reused-ino", entry(2, Some((100, 0)))),
            ("/tmp/same", entry(3, Some((100, 0)))),
        ]);
        let after = tree(&[
            ("/tmp/lock", entry(9, Some((200, 0)))),
            ("/tmp/reused-ino", entry(2, Some((200, 0)))),
            ("/tmp/same", entry(3, Some((100, 0)))),
        ]);
        let v = diff(&before, &after, &Fixture::new());
        let paths: Vec<_> = v.iter().map(|v| v.path.clone()).collect();
        assert_eq!(
            paths,
            vec![PathBuf::from("/tmp/lock"), PathBuf::from("/tmp/reused-ino")]
        );
        assert!(
            v.iter()
                .all(|v| matches!(v.change, Change::Replaced { .. }))
        );
    }

    #[test]
    fn a_new_object_is_not_a_violation() {
        let before = tree(&[]);
        let after = tree(&[("/tmp/new", entry(1, None))]);
        assert!(diff(&before, &after, &Fixture::new()).is_empty());
    }

    fn event(removed: &[&str], replaced: &[&str], outcome: Outcome) -> Event {
        Event {
            pid: 1,
            syscall: "renameat2".into(),
            removed: removed.iter().map(PathBuf::from).collect(),
            replaced: replaced.iter().map(PathBuf::from).collect(),
            outcome,
        }
    }

    #[test]
    fn traced_removals_of_pre_existing_objects_outside_the_set_are_violations() {
        let before = tree(&[
            ("/tmp/cand/a", entry(1, None)),
            ("/tmp/keep", entry(2, None)),
            ("/tmp/dst", entry(3, None)),
        ]);
        let events = [
            event(&["/tmp/cand/a"], &[], Outcome::Ok),
            event(&["/tmp/keep"], &[], Outcome::Ok),
            // Created during the run, then renamed over a pre-existing file.
            event(&["/tmp/dst.new"], &["/tmp/dst"], Outcome::Ok),
            // Failed calls change nothing.
            event(&["/tmp/keep"], &[], Outcome::Failed("EACCES".into())),
        ];
        let v = traced_violations(&events, &before, &fixture());
        let got: Vec<_> = v.iter().map(|v| (v.path.clone(), v.how)).collect();
        assert_eq!(
            got,
            vec![
                (PathBuf::from("/tmp/keep"), "removed"),
                (PathBuf::from("/tmp/dst"), "replaced")
            ]
        );
    }

    fn of_kind(kind: Kind, ino: u64) -> Entry {
        Entry {
            kind,
            ..entry(ino, None)
        }
    }

    #[test]
    fn unmatched_paths_are_classified_not_dropped() {
        let before = tree(&[
            ("/", of_kind(Kind::Dir, 1)),
            ("/tmp", of_kind(Kind::Dir, 2)),
            ("/tmp/real", of_kind(Kind::Dir, 3)),
            ("/tmp/real/a", of_kind(Kind::File, 4)),
            ("/tmp/link", of_kind(Kind::Symlink, 5)),
            ("/tmp/file", of_kind(Kind::File, 6)),
        ]);
        let after = tree(&[("/tmp/made", of_kind(Kind::Dir, 7))]);
        let cases = [
            ("/tmp/real/new", Unmatched::CreatedDuringRun),
            // Parent created during the run too (and still there, or gone).
            ("/tmp/made/x", Unmatched::CreatedDuringRun),
            ("/tmp/gone/deeper/x", Unmatched::CreatedDuringRun),
            ("/tmp/real/../real/a", Unmatched::NonCanonical),
            ("/tmp/./real/a", Unmatched::NonCanonical),
            ("tmp/real/a", Unmatched::NonCanonical),
            ("/tmp//real/a", Unmatched::NonCanonical),
            ("/tmp/real/a/", Unmatched::NonCanonical),
            ("/tmp/old (deleted)/a", Unmatched::DeletedDirectory),
            (
                "/tmp/link/a",
                Unmatched::SymlinkPrefix(PathBuf::from("/tmp/link")),
            ),
            (
                "/tmp/link/sub/a",
                Unmatched::SymlinkPrefix(PathBuf::from("/tmp/link")),
            ),
            ("/tmp/file/a", Unmatched::NoKnownParent),
            ("<fd 4>/f", Unmatched::UnnamedDirFd),
        ];
        for (path, want) in cases {
            assert_eq!(
                classify_unmatched(Path::new(path), &before, &after),
                want,
                "{path}"
            );
        }
        // A file literally named "x (deleted)" is a matched path, never
        // classified; only directory components count.
        assert_eq!(
            classify_unmatched(Path::new("/tmp/real/x (deleted)"), &before, &after),
            Unmatched::CreatedDuringRun
        );
    }

    #[test]
    fn unmatched_removals_skip_matched_and_failed_calls() {
        let before = tree(&[
            ("/tmp", of_kind(Kind::Dir, 2)),
            ("/tmp/link", of_kind(Kind::Symlink, 5)),
            ("/tmp/keep", of_kind(Kind::File, 6)),
        ]);
        let events = [
            event(&["/tmp/keep"], &[], Outcome::Ok),
            event(&["/tmp/link/x"], &[], Outcome::Ok),
            event(&["/tmp/link/y"], &[], Outcome::Failed("ENOENT".into())),
            event(&["/tmp/new.tmp"], &["/tmp/new"], Outcome::Ok),
        ];
        let got: Vec<_> = unmatched_removals(&events, &before, &BTreeMap::new())
            .into_iter()
            .map(|u| (u.path, u.how, u.why))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    PathBuf::from("/tmp/link/x"),
                    "removed",
                    Unmatched::SymlinkPrefix(PathBuf::from("/tmp/link"))
                ),
                (
                    PathBuf::from("/tmp/new.tmp"),
                    "removed",
                    Unmatched::CreatedDuringRun
                ),
                (
                    PathBuf::from("/tmp/new"),
                    "replaced",
                    Unmatched::CreatedDuringRun
                ),
            ]
        );
    }

    fn unmatched(path: &str, why: Unmatched) -> UnmatchedRemoval {
        UnmatchedRemoval {
            path: PathBuf::from(path),
            pid: 1,
            syscall: "unlinkat".into(),
            how: "removed",
            why,
        }
    }

    #[test]
    fn the_unmatched_log_lists_suspicious_calls_and_counts_expected_ones() {
        let mut all: Vec<UnmatchedRemoval> = (0..1000)
            .map(|i| unmatched(&format!("/tmp/churn/f{i}"), Unmatched::CreatedDuringRun))
            .collect();
        all.push(unmatched("/tmp/a/../b", Unmatched::NonCanonical));
        let lines = unmatched_log(&all);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("UNMATCHED removed /tmp/a/../b"));
        assert!(lines[0].contains("non-canonical"));
        assert!(lines[1].starts_with("1000 traced calls on objects created during the run"));

        let many: Vec<UnmatchedRemoval> = (0..UNMATCHED_LOG_LIMIT + 7)
            .map(|i| unmatched(&format!("/x/../{i}"), Unmatched::NonCanonical))
            .collect();
        let lines = unmatched_log(&many);
        assert_eq!(lines.len(), UNMATCHED_LOG_LIMIT + 1);
        assert_eq!(
            lines.last().unwrap(),
            "... and 7 more unmatched traced calls not shown"
        );
    }
}
