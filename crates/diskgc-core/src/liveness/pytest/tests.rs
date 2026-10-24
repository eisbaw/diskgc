use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use proptest::prelude::*;

use super::*;
use crate::host::{Dev, DevIno, Environ, Ids, Pid, Timestamp};
use crate::liveness::{OwnerPid, Unknown};

const NOW: i64 = 2_000_000_000;
const DAY: i64 = 24 * 60 * 60;

fn at(secs: i64) -> Timestamp {
    Timestamp { secs, nanos: 0 }
}

fn id(ino: u64) -> DevIno {
    DevIno {
        dev: Dev {
            major: 0,
            minor: 40,
        },
        ino,
    }
}

const ROOT: u64 = 2;
const SESSION: u64 = 100;

fn env(vars: &[(&str, &str)]) -> Environ {
    Environ {
        vars: vars
            .iter()
            .map(|(n, v)| (OsString::from(n), OsString::from(v)))
            .collect(),
        without_equals: Vec::new(),
    }
}

/// A uid 1000 process with a readable command line and environment and
/// nothing else.
fn proc(pid: u32, cmdline: &[&str], vars: &[(&str, &str)]) -> ProcessFacts {
    ProcessFacts {
        pid: Pid(pid),
        uids: Some(Ids::same(1000)),
        cmdline: Some(cmdline.iter().map(OsString::from).collect()),
        environ: Some(env(vars)),
        cwds: vec![id(1)],
        temp_roots: Vec::new(),
        fallback_roots: Vec::new(),
        basetemps: Vec::new(),
    }
}

fn names(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

/// Layout B of docs/testing.md: `/tmp/nix-shell.AB/pytest-of-tester`, its
/// sessions ten days old, no lock.
struct Case {
    name: OsString,
    listing: Result<Vec<OsString>, String>,
    lock: Option<Lock>,
    lock_owner: Option<OwnerPid>,
    newest_change: Option<Timestamp>,
    process_list: Result<(), String>,
    processes: Vec<ProcessFacts>,
    root_path: PathBuf,
    tree: HashSet<DevIno>,
}

impl Case {
    fn new(n: u32, sessions: &[&str]) -> Self {
        Self {
            name: OsString::from(format!("pytest-{n}")),
            listing: Ok(names(sessions)),
            lock: None,
            lock_owner: None,
            newest_change: Some(at(NOW - 10 * DAY)),
            process_list: Ok(()),
            processes: Vec::new(),
            root_path: PathBuf::from("/tmp/nix-shell.AB"),
            tree: [id(SESSION), id(SESSION + 1)].into_iter().collect(),
        }
    }

    fn session_path(&self) -> PathBuf {
        self.root_path
            .join("pytest-of-tester")
            .join(Path::new(&self.name))
    }

    fn verdict(&self) -> Verdict {
        let session_path = self.session_path();
        let facts = Facts {
            session_name: &self.name,
            listing: self.listing.as_deref().map_err(Clone::clone),
            lock: self.lock.clone(),
            lock_owner: self.lock_owner.clone(),
            now: at(NOW),
            newest_change: self.newest_change,
            process_list: self.process_list.clone(),
            owner: 1000,
            session_path: &session_path,
            session_tree: &self.tree,
            root_path: &self.root_path,
            root_id: id(ROOT),
            processes: &self.processes,
        };
        verdict(&facts)
    }
}

const FIVE: [&str; 5] = ["pytest-0", "pytest-1", "pytest-2", "pytest-3", "pytest-4"];

#[test]
fn every_row_is_reachable_and_named_by_its_number() {
    // 1
    let mut c = Case::new(0, &FIVE);
    c.name = "pytest-05".into();
    assert_eq!(c.verdict().row, Row::NotSession);
    // 2
    let mut c = Case::new(0, &FIVE);
    c.listing = Err("EACCES".into());
    assert_eq!(c.verdict().outcome, Outcome::Unreadable);
    assert_eq!(c.verdict().row, Row::ListingFailed);
    // 3
    let mut c = Case::new(0, &FIVE);
    c.lock = Some(Lock::Malformed("a symlink".into()));
    assert_eq!(c.verdict().row, Row::LockMalformed);
    // 4
    let mut c = Case::new(0, &FIVE);
    c.lock = Some(Lock::Pid {
        pid: 42,
        mtime: at(NOW - 10 * DAY),
    });
    c.lock_owner = Some(OwnerPid::MayBeInUse(Pid(42)));
    assert_eq!(
        c.verdict().outcome,
        Outcome::InUse(vec![(Pid(42), Evidence::OwnerPid)])
    );
    // 5
    c.lock_owner = Some(OwnerPid::Unknown(Unknown {
        rule: "hidepid".into(),
        blockers: vec![Pid(77)],
    }));
    let v = c.verdict();
    assert_eq!(v.row, Row::LockOwnerUnknown);
    assert_eq!(
        v.outcome,
        Outcome::ProducerUnknown(vec![(Pid(77), Evidence::Unreadable)])
    );
    // 6
    let mut c = Case::new(3, &FIVE);
    let mut p = proc(5, &["python", "-m", "pytest"], &[("TMPDIR", "/tmp/other")]);
    p.basetemps = vec![(
        BasetempSource::CommandLine,
        "/x".into(),
        Lookup::Resolved(vec![id(SESSION), id(ROOT)]),
    )];
    c.processes = vec![p];
    assert_eq!(c.verdict().row, Row::Named);
    // 7
    let c = Case::new(1, &FIVE);
    assert_eq!(c.verdict().row, Row::Rank);
    // 8
    let mut c = Case::new(3, &FIVE);
    let mut p = proc(5, &["sh"], &[("HOME", "/h")]);
    p.cmdline = None;
    c.processes = vec![p];
    assert_eq!(c.verdict().row, Row::NamingUndecidable);
    // 9
    let mut c = Case::new(3, &FIVE);
    let mut p = proc(5, &["sh"], &[("TMPDIR", "nix-shell.AB")]);
    p.temp_roots = vec![("nix-shell.AB".into(), Lookup::Relative)];
    c.processes = vec![p];
    assert_eq!(
        c.verdict().outcome,
        Outcome::ProducerUnknown(vec![(Pid(5), Evidence::RelativePath)])
    );
    // 10
    let mut c = Case::new(3, &FIVE);
    let mut p = proc(5, &["sh"], &[("TMPDIR", "/tmp/nix-shell.AB")]);
    p.temp_roots = vec![("/tmp/nix-shell.AB".into(), Lookup::Resolved(vec![id(ROOT)]))];
    c.processes = vec![p];
    assert_eq!(c.verdict().row, Row::RootUsed);
    // 11
    let c = Case::new(3, &FIVE);
    assert_eq!(c.verdict().row, Row::UnusedRoot);
    for (number, row) in (1..).zip(Row::ALL) {
        assert_eq!(row.number(), number);
    }
}

#[test]
fn an_ended_lock_owner_alone_never_permits() {
    // Layout A: the fallback root /tmp is used by any process of the user.
    let mut c = Case::new(0, &["pytest-0"]);
    c.root_path = "/tmp".into();
    c.lock = Some(Lock::Pid {
        pid: 42,
        mtime: at(NOW - DAY),
    });
    c.lock_owner = Some(OwnerPid::Ended);
    c.processes = vec![proc(500, &["bash"], &[("HOME", "/h")])];
    let v = c.verdict();
    assert_eq!(v.row, Row::RootUsed);
    // Below the rank, a young lock with an ended owner still fails row 7.
    let mut c = Case::new(0, &FIVE);
    c.lock = Some(Lock::Pid {
        pid: 42,
        mtime: at(NOW - DAY),
    });
    c.lock_owner = Some(OwnerPid::Ended);
    assert_eq!(c.verdict().row, Row::UnusedRoot);
    c.lock = Some(Lock::Pid {
        pid: 42,
        mtime: at(NOW - 4 * DAY),
    });
    assert_eq!(c.verdict().row, Row::Rank);
}

#[test]
fn the_dead_lock_age_has_its_minute_of_margin() {
    let edge = NOW - 3 * DAY;
    assert!(!older_than_dead_lock_age(at(edge), at(NOW)));
    assert!(!older_than_dead_lock_age(at(edge - 60), at(NOW)));
    assert!(older_than_dead_lock_age(at(edge - 61), at(NOW)));
    assert!(!older_than_dead_lock_age(at(NOW + DAY), at(NOW)));
    assert!(!older_than_dead_lock_age(at(i64::MAX), at(NOW)));
}

#[test]
fn a_running_pytest_needs_an_old_session_for_the_rank_rule() {
    let mut c = Case::new(0, &FIVE);
    c.newest_change = Some(at(NOW - DAY));
    c.processes = vec![proc(7, &["python", "-m", "pytest"], &[("HOME", "/h")])];
    assert_eq!(c.verdict().row, Row::UnusedRoot, "row 11 decides instead");
    c.newest_change = Some(at(NOW - 4 * DAY));
    assert_eq!(c.verdict().row, Row::Rank);
    // An unknown newest change is never old.
    c.newest_change = None;
    assert_ne!(c.verdict().row, Row::Rank);
}

#[test]
fn an_incomplete_process_list_counts_a_pytest_running_and_leaves_naming_undecided() {
    let mut c = Case::new(1, &FIVE);
    c.process_list = Err("hidepid".into());
    assert_eq!(c.verdict().row, Row::Rank, "old session below the rank");
    c.newest_change = Some(at(NOW - DAY));
    let v = c.verdict();
    assert_eq!(v.row, Row::NamingUndecidable);
    assert_eq!(v.outcome, Outcome::ProducerUnknown(Vec::new()));
}

#[test]
fn other_users_processes_are_not_in_the_process_set() {
    let mut c = Case::new(3, &FIVE);
    let mut daemon = proc(1, &["d"], &[]);
    daemon.uids = Some(Ids::same(0));
    daemon.environ = None;
    c.processes = vec![daemon];
    assert_eq!(c.verdict().row, Row::UnusedRoot);
    // A process whose uids are unknown is in every set.
    c.processes[0].uids = None;
    assert_eq!(c.verdict().row, Row::NamingUndecidable);
    // Any of the four uids counts.
    c.processes[0].uids = Some(Ids {
        real: 0,
        effective: 0,
        saved: 1000,
        fs: 0,
    });
    assert_eq!(c.verdict().row, Row::NamingUndecidable);
}

#[test]
fn naming_by_string_is_lexical_and_bounded_by_a_separator() {
    let s = Path::new("/tmp/nix-shell.AB/pytest-of-tester/pytest-3");
    assert!(names_path(b"/tmp/nix-shell.AB/pytest-of-tester/pytest-3", s).is_some());
    assert!(names_path(b"/tmp/nix-shell.AB/pytest-of-tester/pytest-3/t.py", s).is_some());
    assert!(names_path(b"/tmp//nix-shell.AB/./x/../pytest-of-tester/pytest-3", s).is_some());
    assert!(names_path(b"/tmp/nix-shell.AB/pytest-of-tester/pytest-30", s).is_none());
    assert!(names_path(b"nix-shell.AB/pytest-of-tester/pytest-3", s).is_none());
    assert!(names_path(b"/../tmp/nix-shell.AB/pytest-of-tester/pytest-3", s).is_some());
    assert_eq!(normalize(b"/"), Some(PathBuf::from("/")));
    assert_eq!(normalize(b"/a/.."), Some(PathBuf::from("/")));
    assert_eq!(normalize(b"//a//b/"), Some(PathBuf::from("/a/b")));
    assert_eq!(normalize(b""), None);
}

#[test]
fn row_6_names_by_argument_by_environment_and_by_resolved_basetemp() {
    let mut c = Case::new(3, &FIVE);
    let s = c.session_path();
    let mut by_arg = proc(9, &["python", s.to_str().unwrap()], &[("HOME", "/h")]);
    c.processes = vec![by_arg.clone()];
    assert_eq!(
        c.verdict().outcome,
        Outcome::InUse(vec![(Pid(9), Evidence::CommandLine)])
    );
    let by_env = proc(8, &["sh"], &[("X", &format!("/a:{}", s.display()))]);
    c.processes = vec![by_env];
    assert_eq!(
        c.verdict().outcome,
        Outcome::InUse(vec![(Pid(8), Evidence::Environment)])
    );
    by_arg.cmdline = Some(vec!["python".into()]);
    by_arg.environ = Some(env(&[("PYTEST_ADDOPTS", "--basetemp=/link/x")]));
    by_arg.basetemps = vec![(
        BasetempSource::Addopts,
        "/link/x".into(),
        Lookup::Resolved(vec![id(SESSION + 1), id(SESSION), id(ROOT)]),
    )];
    c.processes = vec![by_arg];
    assert_eq!(
        c.verdict().outcome,
        Outcome::InUse(vec![(Pid(9), Evidence::Environment)])
    );
}

#[test]
fn several_causes_are_one_entry_each_sorted_by_pid() {
    let mut c = Case::new(3, &FIVE);
    for pid in [91, 90] {
        let mut p = proc(pid, &["sh"], &[("TMPDIR", "rel")]);
        p.temp_roots = vec![("rel".into(), Lookup::Relative)];
        c.processes.push(p);
    }
    assert_eq!(
        c.verdict().outcome,
        Outcome::ProducerUnknown(vec![
            (Pid(90), Evidence::RelativePath),
            (Pid(91), Evidence::RelativePath)
        ])
    );
}

#[test]
fn pytest_signs() {
    let sign = |cmd: &[&str], vars: &[(&str, &str)]| shows_pytest(&proc(1, cmd, vars));
    assert!(sign(&["python", "-m", "pytest"], &[]));
    assert!(sign(&["/nix/store/x-python3/bin/py.test"], &[]));
    assert!(sign(
        &[
            "python",
            "-u",
            "-c",
            "import sys;exec(eval(sys.stdin.readline()))"
        ],
        &[]
    ));
    assert!(!sign(
        &[
            "python",
            "-u",
            "-c",
            "import sys;exec(eval(sys.stdin.readline()))x"
        ],
        &[]
    ));
    assert!(!sign(
        &["python", "import sys;exec(eval(sys.stdin.readline()))"],
        &[]
    ));
    for var in [
        "PYTEST_ADDOPTS",
        "PYTEST_CURRENT_TEST",
        "PYTEST_VERSION",
        "PYTEST_XDIST_WORKER",
    ] {
        assert!(sign(&["sh"], &[(var, "")]), "{var}");
    }
    assert!(!sign(
        &["sh"],
        &[("PYTEST_DEBUG_TEMPROOT", "/x"), ("HOME", "/h")]
    ));
    let mut unreadable = proc(1, &["sh"], &[]);
    unreadable.cmdline = None;
    assert!(shows_pytest(&unreadable));
}

#[test]
fn basetemp_values_as_argparse_and_shlex_read_them() {
    let cmd: Vec<OsString> = [
        "pytest",
        "--basetemp",
        "a b",
        "--basetemp=/c",
        "--basetemp=",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let e = env(&[(
        "PYTEST_ADDOPTS",
        "-x --basetemp '/d e' --basetemp=\"/f\\\"g\"",
    )]);
    let got: Vec<(BasetempSource, OsString)> = basetemp_values(Some(&cmd), Some(&e));
    assert_eq!(
        got,
        vec![
            (BasetempSource::CommandLine, "a b".into()),
            (BasetempSource::CommandLine, "/c".into()),
            (BasetempSource::Addopts, "/d e".into()),
            (BasetempSource::Addopts, "/f\"g".into()),
        ]
    );
    // A trailing --basetemp without a value, and an abbreviation, give none.
    let cmd: Vec<OsString> = ["pytest", "--basetem=/x", "--basetemp"]
        .iter()
        .map(OsString::from)
        .collect();
    assert!(basetemp_values(Some(&cmd), None).is_empty());
    assert_eq!(
        shlex_split(b"a\\ b \"c\\\\d\" 'e"),
        vec![b"a b".to_vec(), b"c\\d".to_vec(), b"e".to_vec()]
    );
}

#[test]
fn temp_root_values_skip_empty_and_keep_every_occurrence() {
    let e = env(&[
        ("TMPDIR", ""),
        ("TMPDIR", "/a"),
        ("TEMP", "rel"),
        ("PYTEST_DEBUG_TEMPROOT", "/b"),
        ("HOME", "/h"),
    ]);
    assert_eq!(
        temp_root_values(&e),
        vec![OsString::from("/a"), "rel".into(), "/b".into()]
    );
}

#[test]
fn session_grammar() {
    assert_eq!(session_number(b"pytest-0"), Some(0));
    assert_eq!(
        session_number(b"pytest-18446744073709551615"),
        Some(u64::MAX)
    );
    assert_eq!(session_number(b"pytest-18446744073709551616"), None);
    assert_eq!(session_number(b"pytest-05"), None);
    assert_eq!(rank_number(b"pytest-05"), Some(5));
    assert_eq!(rank_number(b"pytest-99999999999999999999"), None);
    for no in [
        "pytest-",
        "pytest-9x",
        "pytest-+1",
        "Pytest-1",
        "pytest-current",
    ] {
        assert_eq!(rank_number(no.as_bytes()), None, "{no}");
    }
    assert!(below_rank(0, Some(3)));
    assert!(!below_rank(1, Some(3)));
    assert!(!below_rank(0, None));
    assert!(!below_rank(u64::MAX, Some(u64::MAX)));
}

// ---- properties ----

/// Python's `int()` of a pytest name suffix, over the alphabet the
/// generator uses (digits, `_`, `+`, `-`, space, letters): optional
/// surrounding whitespace, an optional sign, digits with single
/// underscores between them. `None`: `ValueError` (pytest's `parse_num`
/// then gives -1).
fn python_int(text: &str) -> Option<i128> {
    let t = text.trim_matches(|c: char| c.is_ascii_whitespace());
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'+') => (false, &t[1..]),
        Some(b'-') => (true, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let value: i128 = digits.replace('_', "").parse().ok()?;
    Some(if negative { -value } else { value })
}

/// What a `.lock` is in the generated snapshot.
#[derive(Debug, Clone)]
enum GenLock {
    None,
    /// A regular file with a pid, this many seconds before `NOW`.
    File(i64),
    /// Not a regular file (a directory, a symlink): diskgc's row 3.
    Other,
}

#[derive(Debug, Clone)]
struct GenEntry {
    name: String,
    symlink: bool,
    lock: GenLock,
}

/// pytest 9.1.1's cleanup of `P` (`cleanup_numbered_dir` →
/// `cleanup_candidates` → `try_cleanup` → `ensure_deletable`), run by a
/// session with the default retention created at `t_new` on the snapshot
/// `entries` (its own number one above the largest): whether it deletes
/// the entry called `name`.
fn pytest_cleanup_deletes(entries: &[GenEntry], name: &str, t_new: i64) -> bool {
    let parse_num = |e: &GenEntry| -> i128 {
        let lower = e.name.to_lowercase();
        lower
            .strip_prefix("pytest-")
            .and_then(|_| python_int(&e.name["pytest-".len()..]))
            .unwrap_or(-1)
    };
    let prefixed: Vec<&GenEntry> = entries
        .iter()
        .filter(|e| e.name.to_lowercase().starts_with("pytest-"))
        .collect();
    // The new session, one above the largest, is made before the cleanup
    // runs, so the cleanup's largest number is its own.
    let max_existing = prefixed.iter().map(|e| parse_num(e)).max().unwrap_or(-1) + 1;
    let max_delete = max_existing - i128::from(RETENTION);
    let Some(entry) = prefixed.iter().find(|e| e.name == name) else {
        return false;
    };
    if parse_num(entry) > max_delete || entry.symlink {
        return false;
    }
    let dead_before = t_new - 3 * DAY;
    match entry.lock {
        GenLock::None | GenLock::Other => true,
        GenLock::File(age) => NOW - age < dead_before,
    }
}

fn gen_suffix() -> impl Strategy<Value = String> {
    prop_oneof![
        (0u64..12).prop_map(|n| n.to_string()),
        (0u64..12).prop_map(|n| format!("0{n}")),
        "[0-9_+ a-z-]{1,4}",
        Just("99999999999999999999".to_owned()),
    ]
}

fn gen_entry() -> impl Strategy<Value = GenEntry> {
    (
        prop_oneof![Just("pytest-"), Just("PYTEST-"), Just("garbage-")],
        gen_suffix(),
        any::<bool>(),
        prop_oneof![
            Just(GenLock::None),
            (0..6 * DAY).prop_map(GenLock::File),
            Just(GenLock::Other),
        ],
    )
        .prop_map(|(prefix, suffix, symlink, lock)| GenEntry {
            name: format!("{prefix}{suffix}"),
            symlink,
            lock,
        })
}

fn gen_process() -> impl Strategy<Value = ProcessFacts> {
    (
        1u32..50,
        prop_oneof![
            Just(None),
            Just(Some(Ids::same(1000))),
            Just(Some(Ids::same(0)))
        ],
        prop_oneof![
            Just(None),
            Just(Some(vec![OsString::from("sh")])),
            Just(Some(vec![
                OsString::from("python"),
                "-m".into(),
                "pytest".into()
            ])),
        ],
        prop_oneof![
            Just(None),
            Just(Some(env(&[("HOME", "/h")]))),
            Just(Some(env(&[("PYTEST_CURRENT_TEST", "t")]))),
        ],
        prop_oneof![
            Just(Vec::new()),
            Just(vec![("rel".into(), Lookup::Relative)]),
            Just(vec![("/x".into(), Lookup::Resolved(vec![id(ROOT)]))]),
            Just(vec![("/y".into(), Lookup::Unresolvable("ENOENT".into()))]),
        ],
        prop_oneof![
            Just(Vec::new()),
            Just(vec![(
                BasetempSource::CommandLine,
                "/s".into(),
                Lookup::Resolved(vec![id(SESSION), id(ROOT)])
            )]),
            Just(vec![(
                BasetempSource::Addopts,
                "r".into(),
                Lookup::Relative
            )]),
        ],
        any::<bool>(),
    )
        .prop_map(
            |(pid, uids, cmdline, environ, temp_roots, basetemps, cwd_root)| ProcessFacts {
                pid: Pid(pid),
                uids,
                cmdline,
                environ,
                cwds: if cwd_root {
                    vec![id(ROOT)]
                } else {
                    vec![id(1)]
                },
                temp_roots,
                fallback_roots: Vec::new(),
                basetemps,
            },
        )
}

#[derive(Debug, Clone)]
struct Generated {
    entries: Vec<GenEntry>,
    pick: usize,
    owner_ended: bool,
    newest_age: i64,
    complete: bool,
    processes: Vec<ProcessFacts>,
    fallback_root: bool,
}

fn gen_case() -> impl Strategy<Value = Generated> {
    (
        prop::collection::vec(gen_entry(), 1..8),
        any::<prop::sample::Index>(),
        any::<bool>(),
        0..6 * DAY,
        any::<bool>(),
        prop::collection::vec(gen_process(), 0..4),
        any::<bool>(),
    )
        .prop_map(
            |(mut entries, pick, owner_ended, newest_age, complete, processes, fallback_root)| {
                let pick = pick.index(entries.len());
                // Discovery never reports a symlink as a session (type: dir).
                entries[pick].symlink = false;
                Generated {
                    entries,
                    pick,
                    owner_ended,
                    newest_age,
                    complete,
                    processes,
                    fallback_root,
                }
            },
        )
}

/// The verdict on the generated case, for the picked entry as `S`.
fn judge(g: &Generated) -> (Verdict, bool) {
    let entry = &g.entries[g.pick];
    let listing: Vec<OsString> = g.entries.iter().map(|e| OsString::from(&e.name)).collect();
    let (lock, lock_owner) = match entry.lock {
        GenLock::None => (None, None),
        GenLock::Other => (Some(Lock::Malformed("a directory".into())), None),
        GenLock::File(age) => (
            Some(Lock::Pid {
                pid: 42,
                mtime: at(NOW - age),
            }),
            Some(if g.owner_ended {
                OwnerPid::Ended
            } else {
                OwnerPid::MayBeInUse(Pid(42))
            }),
        ),
    };
    let root_path = if g.fallback_root {
        PathBuf::from("/tmp")
    } else {
        PathBuf::from("/tmp/nix-shell.AB")
    };
    let session_path = root_path.join("pytest-of-tester").join(&entry.name);
    let tree: HashSet<DevIno> = [id(SESSION)].into_iter().collect();
    let name = OsString::from(&entry.name);
    let facts = Facts {
        session_name: &name,
        listing: Ok(&listing),
        lock,
        lock_owner,
        now: at(NOW),
        newest_change: Some(at(NOW - g.newest_age)),
        process_list: if g.complete {
            Ok(())
        } else {
            Err("hidepid".into())
        },
        owner: 1000,
        session_path: &session_path,
        session_tree: &tree,
        root_path: &root_path,
        root_id: id(ROOT),
        processes: &g.processes,
    };
    let running = !g.complete
        || g.processes
            .iter()
            .filter(|p| p.uids.is_none_or(|u| u.all().contains(&1000)))
            .any(shows_pytest);
    (verdict(&facts), running)
}

proptest! {
    // No regression file: manual mutation sweeps must leave no seeds
    // (TASK-132), as in liveness::tests.
    #![proptest_config(ProptestConfig { failure_persistence: None, cases: 2048, ..ProptestConfig::default() })]

    /// The session grammar: canonical digits fitting 64 bits parse as
    /// sessions; leading zeros are rejected as sessions but counted in M;
    /// values over 64 bits are neither.
    #[test]
    fn session_grammar_round_trips(n in any::<u64>(), zeros in 1usize..3, big in "[1-9][0-9]{20,25}") {
        let canonical = format!("pytest-{n}");
        prop_assert_eq!(session_number(canonical.as_bytes()), Some(n));
        let padded = format!("pytest-{}{n}", "0".repeat(zeros));
        prop_assert_eq!(session_number(padded.as_bytes()), None);
        prop_assert_eq!(rank_number(padded.as_bytes()), Some(n));
        let over = format!("pytest-{big}");
        prop_assert_eq!(session_number(over.as_bytes()), None);
        prop_assert_eq!(rank_number(over.as_bytes()), None);
        prop_assert_eq!(largest(&[OsString::from(over), OsString::from("pytest-1")]), Some(1));
    }

    /// Row 7's invariant (docs/safety.md, pytest sessions): it never
    /// permits a session that pytest 9.1.1's own next cleanup, with the
    /// default retention, created at or after the check, would keep.
    #[test]
    fn rank_row_never_permits_what_pytests_next_cleanup_keeps(g in gen_case(), later in 0..(2 * DAY)) {
        let (v, _) = judge(&g);
        if v.row == Row::Rank {
            prop_assert_eq!(&v.outcome, &Outcome::Eligible);
            let name = &g.entries[g.pick].name;
            prop_assert!(
                pytest_cleanup_deletes(&g.entries, name, NOW + later),
                "row 7 permitted {} which pytest's cleanup keeps: {:?}", name, g
            );
        }
    }

    /// Every session gets exactly one row, and only rows 7 and 11 permit;
    /// no session a process of the set names is eligible; row 7 never
    /// permits a young lock, nor a young session while a pytest runs.
    #[test]
    fn table_invariants(g in gen_case()) {
        let (v, running) = judge(&g);
        prop_assert!(Row::ALL.contains(&v.row));
        if v.outcome == Outcome::Eligible {
            prop_assert!(matches!(v.row, Row::Rank | Row::UnusedRoot), "{:?}", v);
        }
        if matches!(v.row, Row::Rank | Row::UnusedRoot) {
            prop_assert_eq!(&v.outcome, &Outcome::Eligible);
        }
        let named = g.processes.iter().any(|p| {
            p.uids.is_none_or(|u| u.all().contains(&1000))
                && p.basetemps.iter().any(|(_, _, l)| matches!(l, Lookup::Resolved(c) if c.contains(&id(SESSION))))
        });
        if named {
            prop_assert_ne!(&v.outcome, &Outcome::Eligible);
        }
        if v.row == Row::Rank {
            if let GenLock::File(age) = g.entries[g.pick].lock {
                prop_assert!(age > DEAD_LOCK_AGE_SECS);
            }
            if running {
                prop_assert!(g.newest_age > DEAD_LOCK_AGE_SECS);
            }
        }
        if v.row == Row::UnusedRoot {
            prop_assert!(g.complete);
        }
    }

    /// Normalisation is idempotent and never leaves `.`, `..` or empty
    /// names in an absolute result.
    #[test]
    fn normalize_is_idempotent(text in "/[a./]{0,16}") {
        let once = normalize(text.as_bytes()).expect("absolute");
        let again = normalize(once.as_os_str().as_bytes()).expect("absolute");
        prop_assert_eq!(&once, &again);
        let bytes = once.as_os_str().as_bytes();
        prop_assert!(bytes == b"/" || !bytes.ends_with(b"/"));
        prop_assert!(!once.components().any(|c| matches!(c, std::path::Component::CurDir | std::path::Component::ParentDir)));
    }
}

#[test]
fn the_cleanup_model_reads_names_as_pytest_does() {
    // Sanity of the oracle the invariant rests on.
    assert_eq!(python_int("007"), Some(7));
    assert_eq!(python_int("1_0"), Some(10));
    assert_eq!(python_int(" +3 "), Some(3));
    assert_eq!(python_int("1__0"), None);
    assert_eq!(python_int("x"), None);
    let e = |name: &str| GenEntry {
        name: name.into(),
        symlink: false,
        lock: GenLock::None,
    };
    let entries = [e("pytest-0"), e("pytest-1"), e("pytest-2"), e("pytest-3")];
    // M 3, the new session 4: max_delete 1.
    assert!(pytest_cleanup_deletes(&entries, "pytest-0", NOW));
    assert!(pytest_cleanup_deletes(&entries, "pytest-1", NOW));
    assert!(!pytest_cleanup_deletes(&entries, "pytest-2", NOW));
    let _ = OsStr::new("");
}
