//! The project dependency contracts' pure rules (docs/safety.md, Producer
//! liveness: Project dependency directories): which names are an
//! ecosystem's manifest, VCS metadata or a dependency directory, whether a
//! workspace manifest declares workspaces, and the verdict of rules 1 (no
//! change) and 2 (no read) on the newest times the walks found. The walks
//! themselves and rule 3 (no visible use) are I/O and the scan's: they are
//! in [`crate::fs_kind::project`].
//!
//! An unchanged `node_modules` or `.venv` can belong to a project used
//! today: a reader changes neither mtime nor ctime (TASK-79's container
//! probe). So the evidence comes from the project around it, and from
//! read times (`relatime`) where the mount keeps them.

use std::path::{Path, PathBuf};

use crate::host::Timestamp;
use crate::profile::{IdleFor, Idleness, Unknown};
use crate::units::format_duration;

/// Version-control metadata: a project's top level holds one, and the
/// project walk leaves it out (a fetch is not project use).
pub const VCS_METADATA: [&str; 3] = [".git", ".hg", ".svn"];

/// Node's dependency directory name: an ancestor of this name makes a
/// candidate a nested one (`filtered`), whichever contract judges it.
pub const NODE_MODULES: &str = "node_modules";

/// The file a Python virtual environment holds at its top (PEP 405).
pub const PYVENV_CFG: &str = "pyvenv.cfg";

/// Node's project manifest; also, with a `workspaces` field, a workspace
/// root's.
pub const PACKAGE_JSON: &str = "package.json";

/// pnpm's workspace root marker.
pub const PNPM_WORKSPACE: &str = "pnpm-workspace.yaml";

/// The largest workspace `package.json` diskgc reads: a larger one is
/// `producer_unknown`, never guessed at.
pub const WORKSPACE_MANIFEST_MAX_BYTES: usize = 1 << 20;

/// The fixed names of a Python project manifest; `requirements*.txt`
/// ([`is_requirements`]) counts too.
pub const PYTHON_MANIFESTS: [&str; 4] = ["pyproject.toml", "setup.py", "setup.cfg", "Pipfile"];

/// The most names diskgc lists in a candidate's parent looking for a
/// `requirements*.txt` (only when no fixed manifest is there): a larger
/// directory is `unreadable` (a listing cut short), not a project without
/// a manifest.
pub const PARENT_NAMES_MAX: usize = 65_536;

/// Whether `name` is a `requirements*.txt` (`requirements.txt`,
/// `requirements-dev.txt`, ...).
pub fn is_requirements(name: &[u8]) -> bool {
    name.starts_with(b"requirements") && name.ends_with(b".txt") && name.len() >= 16
}

/// The nearest ancestor of `candidate` (the candidate itself excluded)
/// named [`NODE_MODULES`], by its path text: a candidate inside one is a
/// nested dependency directory (`filtered`). The whole path counts, not
/// only below the source root: a root supplied inside `node_modules` does
/// not make a package's own dependencies a project's.
pub fn inside_node_modules(candidate: &Path) -> Option<PathBuf> {
    candidate
        .ancestors()
        .skip(1)
        .find(|a| a.file_name().is_some_and(|n| n == NODE_MODULES))
        .map(Path::to_path_buf)
}

/// Whether a `package.json` declares workspaces (a top-level `workspaces`
/// field, of any value), or why it cannot be told (not JSON, not an
/// object).
pub fn declares_workspaces(content: &[u8]) -> Result<bool, String> {
    let value: serde_json::Value =
        serde_json::from_slice(content).map_err(|e| format!("not JSON: {e}"))?;
    match value {
        serde_json::Value::Object(fields) => Ok(fields.contains_key("workspaces")),
        _ => Err("not a JSON object".to_owned()),
    }
}

/// The newest time of a kind (change or read) in the trees a contract
/// measured, with the path that has it, or the path whose time was not
/// reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Newest {
    /// Every time was reported; `None`: no entry of that kind (no regular
    /// file, for reads).
    Known(Option<(Timestamp, PathBuf)>),
    /// The time of this entry (or of some entry in this tree) was not
    /// reported.
    Unknown(PathBuf),
}

impl Newest {
    /// The newer of two (an unknown one wins: nothing is known then).
    pub fn max(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unknown(p), _) | (_, Self::Unknown(p)) => Self::Unknown(p),
            (Self::Known(a), Self::Known(b)) => Self::Known(match (a, b) {
                (Some(a), Some(b)) => Some(if b.0 > a.0 { b } else { a }),
                (a, b) => a.or(b),
            }),
        }
    }
}

/// What rules 1 and 2 judge: the newest change (mtime or ctime of any
/// entry) and the newest read (atime of a regular file) in the candidate
/// and its project, and whether the project's mount records reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub newest_change: Newest,
    pub newest_read: Newest,
    /// The mount is `noatime`: rule 2 says nothing there.
    pub noatime: bool,
}

/// Rules 1 and 2 for the idle bound `bound` (the term's: the source's
/// `min_idle_for` and the tier's `idle_for` filters, the longest; none is
/// a zero bound) at `now`. `Err`: the `not_idle` detail, naming the rule
/// and the newest path.
pub fn idle(activity: &Activity, bound: Option<IdleFor>, now: Timestamp) -> Result<(), String> {
    let bound = bound.unwrap_or(IdleFor(std::time::Duration::ZERO));
    let judge = |rule: &str, what: &str, newest: &Newest| -> Result<(), String> {
        match newest {
            Newest::Unknown(path) => Err(format!(
                "{rule}: the {what} of {} was not reported, so the project's last {what} is \
                 unknown",
                path.display()
            )),
            Newest::Known(None) => Ok(()),
            Newest::Known(Some((at, path))) => match bound.verdict(Some(*at), now) {
                Idleness::Idle => Ok(()),
                Idleness::NotIdle => Err(format!(
                    "{rule}: {} has a {what} within the idle bound {} (the newest in the \
                     project)",
                    path.display(),
                    format_duration(bound.0)
                )),
                Idleness::Unknown(Unknown::InFuture) => Err(format!(
                    "{rule}: {} has a {what} in the future",
                    path.display()
                )),
                Idleness::Unknown(Unknown::Unanswered) => Err(format!(
                    "{rule}: the {what} of {} is unknown",
                    path.display()
                )),
            },
        }
    };
    judge("rule 1 (no change)", "change", &activity.newest_change)?;
    if activity.noatime {
        // Under noatime the kernel records no read: rule 2 says nothing
        // (the stated residual).
        return Ok(());
    }
    judge("rule 2 (no read)", "read", &activity.newest_read)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Timestamp {
        Timestamp { secs, nanos: 0 }
    }

    const DAY: i64 = 86_400;

    fn activity(change: i64, read: i64, noatime: bool) -> Activity {
        Activity {
            newest_change: Newest::Known(Some((at(change), PathBuf::from("/p/src/a.js")))),
            newest_read: Newest::Known(Some((at(read), PathBuf::from("/p/node_modules/x/i.js")))),
            noatime,
        }
    }

    fn thirty_days() -> Option<IdleFor> {
        Some(IdleFor(std::time::Duration::from_secs(30 * 86_400)))
    }

    #[test]
    fn a_project_neither_changed_nor_read_within_the_bound_is_idle() {
        let now = at(100 * DAY);
        assert_eq!(
            idle(&activity(40 * DAY, 40 * DAY, false), thirty_days(), now),
            Ok(())
        );
    }

    #[test]
    fn a_recent_change_or_read_keeps_it() {
        let now = at(100 * DAY);
        let changed = idle(&activity(98 * DAY, 40 * DAY, false), thirty_days(), now);
        assert!(
            changed
                .unwrap_err()
                .starts_with("rule 1 (no change): /p/src/a.js")
        );
        let read = idle(&activity(40 * DAY, 99 * DAY, false), thirty_days(), now);
        assert!(
            read.unwrap_err()
                .starts_with("rule 2 (no read): /p/node_modules/x/i.js")
        );
    }

    /// The stated residual: under noatime a read leaves no trace, so rule
    /// 2 says nothing (the read time the model shows is ignored).
    #[test]
    fn noatime_silences_rule_two_only() {
        let now = at(100 * DAY);
        assert_eq!(
            idle(&activity(40 * DAY, 99 * DAY, true), thirty_days(), now),
            Ok(())
        );
        assert!(idle(&activity(99 * DAY, 40 * DAY, true), thirty_days(), now).is_err());
    }

    #[test]
    fn unknown_or_future_times_keep_it() {
        let now = at(100 * DAY);
        let mut unknown = activity(40 * DAY, 40 * DAY, false);
        unknown.newest_read = Newest::Unknown(PathBuf::from("/p/x"));
        assert!(idle(&unknown, thirty_days(), now).is_err());
        assert!(idle(&activity(101 * DAY, 40 * DAY, false), thirty_days(), now).is_err());
    }

    #[test]
    fn exactly_the_bound_is_idle() {
        let now = at(100 * DAY);
        assert_eq!(
            idle(&activity(70 * DAY, 70 * DAY, false), thirty_days(), now),
            Ok(())
        );
        assert!(idle(&activity(70 * DAY + 1, 70 * DAY, false), thirty_days(), now).is_err());
    }

    #[test]
    fn newest_takes_the_later_and_unknown_wins() {
        let a = Newest::Known(Some((at(1), PathBuf::from("/a"))));
        let b = Newest::Known(Some((at(2), PathBuf::from("/b"))));
        assert_eq!(a.clone().max(b.clone()), b);
        assert_eq!(b.clone().max(Newest::Known(None)), b);
        assert!(matches!(
            a.max(Newest::Unknown(PathBuf::from("/u"))),
            Newest::Unknown(_)
        ));
    }

    #[test]
    fn manifests_and_nesting_by_name() {
        for name in [
            "requirements.txt",
            "requirements-dev.txt",
            "requirements_ci.txt",
        ] {
            assert!(is_requirements(name.as_bytes()), "{name}");
        }
        for name in [
            "requirements",
            "requirements.in",
            "dev-requirements.txt",
            "requirements.txt.bak",
        ] {
            assert!(!is_requirements(name.as_bytes()), "{name}");
        }
        assert_eq!(
            inside_node_modules(Path::new("/p/node_modules/a/node_modules")),
            Some(PathBuf::from("/p/node_modules"))
        );
        assert_eq!(inside_node_modules(Path::new("/p/node_modules")), None);
        assert_eq!(inside_node_modules(Path::new("/p/.venv")), None);
    }

    #[test]
    fn workspaces_are_a_top_level_field() {
        assert_eq!(
            declares_workspaces(b"{\"workspaces\": [\"packages/*\"]}"),
            Ok(true)
        );
        assert_eq!(
            declares_workspaces(b"{\"workspaces\": {\"packages\": []}}"),
            Ok(true)
        );
        assert_eq!(
            declares_workspaces(b"{\"name\": \"a\", \"x\": {\"workspaces\": 1}}"),
            Ok(false)
        );
        assert!(declares_workspaces(b"[1]").is_err());
        assert!(declares_workspaces(b"{oops").is_err());
    }
}
