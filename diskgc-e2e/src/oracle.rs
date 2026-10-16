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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
    before
        .iter()
        .filter(|(path, _)| !fixture.covers(path))
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
    let mut found = Vec::new();
    for event in events.iter().filter(|e| e.took_effect()) {
        let changed = event
            .removed
            .iter()
            .map(|p| (p, "removed"))
            .chain(event.replaced.iter().map(|p| (p, "replaced")));
        for (path, how) in changed {
            if before.contains_key(path) && !fixture.covers(path) {
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

/// The oracle's judgement of one run.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub snapshot: Vec<Violation>,
    pub traced: Vec<TracedViolation>,
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
}

impl Oracle {
    /// Takes the "before" snapshot. Fails if an eligible path does not exist
    /// (a fixture typo would otherwise make the eligible set silently empty).
    pub fn arm(fixture: Fixture) -> Result<Self, String> {
        let before = snapshot::take()?;
        info!(
            "oracle armed: {} objects in {:?}; {}; skipped pseudo mounts {:?}",
            before.entries.len(),
            before.took,
            before.key_stats,
            before.skipped
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
        Ok(Self { fixture, before })
    }

    /// Takes the "after" snapshot and judges the run.
    pub fn judge(&self, run: &Run) -> Result<Verdict, String> {
        self.judge_runs(&[run])
    }

    /// Takes the "after" snapshot and judges several runs of code under test
    /// that happened since [`Oracle::arm`].
    pub fn judge_runs(&self, runs: &[&Run]) -> Result<Verdict, String> {
        let after = snapshot::take()?;
        let events: Vec<Event> = runs.iter().flat_map(|r| r.events.iter().cloned()).collect();
        let argvs: Vec<&Vec<String>> = runs.iter().map(|r| &r.argv).collect();
        info!(
            "oracle after {argvs:?}: {} objects in {:?}; {}",
            after.entries.len(),
            after.took,
            after.key_stats
        );
        let verdict = Verdict {
            snapshot: diff(&self.before.entries, &after.entries, &self.fixture),
            traced: traced_violations(&events, &self.before.entries, &self.fixture),
        };
        info!(
            "oracle verdict ({} traced calls): {}",
            events.len(),
            verdict.report()
        );
        Ok(verdict)
    }
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
}
