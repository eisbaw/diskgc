//! The pytest contract's verdict (docs/safety.md, Producer liveness:
//! pytest sessions): one pure function, [`verdict`], over the facts of the
//! ordered decision table, returning the row that decided as well as the
//! verdict, so tests can assert which rule permitted. Everything impure
//! (listing `P`, reading the `.lock`, Owner PID's lookup, the process
//! facts and the temp-root lookups) is gathered by the caller
//! (`fs_kind::liveness`) and the in-use scan ([`crate::inuse::Scan`]).
//!
//! The pieces the scan also needs to decide what to look up are here too
//! ([`temp_root_values`], [`basetemp_values`], [`normalize`]), so the
//! values it resolves are exactly the values the verdict reasons about.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::host::{DevIno, Environ, Ids, Pid, Timestamp};
use crate::liveness::OwnerPid;

/// pytest's default `tmp_path_retention_count`: its exit cleanup keeps the
/// sessions numbered above `M - 3` (docs/safety.md, pytest sessions).
pub const RETENTION: u64 = 3;

/// pytest counts a lock dead at three days (`LOCK_TIMEOUT` in
/// `_pytest/pathlib.py`, measured from a new session's mtime); the extra
/// minute covers coarse filesystem timestamps and a new session's clock
/// being a little behind diskgc's (docs/safety.md, dead-lock age).
pub const DEAD_LOCK_AGE_SECS: i64 = 3 * 24 * 60 * 60 + 60;

/// The execnet worker bootstrap (`popen_bootstrapline` in execnet 2.1.2's
/// `gateway_io.py`), which pytest-xdist's workers run as `python -u -c
/// <this>`.
pub const EXECNET_BOOTSTRAP: &[u8] = b"import sys;exec(eval(sys.stdin.readline()))";

/// The variables pytest resolves a temp root from (`PYTEST_DEBUG_TEMPROOT`,
/// then Python's `tempfile.gettempdir()`: `TMPDIR`, `TEMP`, `TMP`).
pub const TEMP_ROOT_VARS: [&str; 4] = ["TMPDIR", "TEMP", "TMP", "PYTEST_DEBUG_TEMPROOT"];

/// Python's fallback temp roots, always counted as used when they are `R`.
pub const FALLBACK_ROOTS: [&str; 3] = ["/tmp", "/var/tmp", "/usr/tmp"];

/// The rows of the ordered decision table (docs/safety.md, pytest
/// sessions: *The verdict*). Every session gets exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Row {
    /// 1: not a session name.
    NotSession = 1,
    /// 2: the listing of `P` failed.
    ListingFailed = 2,
    /// 3: a `.lock` that is not a regular file of at most 16 bytes holding
    /// a pid.
    LockMalformed = 3,
    /// 4: the lock's pid may be in use.
    LockOwnerMayBeInUse = 4,
    /// 5: the lock's pid cannot be answered.
    LockOwnerUnknown = 5,
    /// 6: a process names `S`.
    Named = 6,
    /// 7: the rank rule (producer-equivalent or stricter).
    Rank = 7,
    /// 8: whether a process names `S` cannot be decided.
    NamingUndecidable = 8,
    /// 9: whether a process could use `R` cannot be decided.
    RootUseUndecidable = 9,
    /// 10: a process could use `R` as its temp root.
    RootUsed = 10,
    /// 11: the unused-root rule (heuristic).
    UnusedRoot = 11,
}

impl Row {
    pub const ALL: [Self; 11] = [
        Self::NotSession,
        Self::ListingFailed,
        Self::LockMalformed,
        Self::LockOwnerMayBeInUse,
        Self::LockOwnerUnknown,
        Self::Named,
        Self::Rank,
        Self::NamingUndecidable,
        Self::RootUseUndecidable,
        Self::RootUsed,
        Self::UnusedRoot,
    ];

    /// Its number in the table.
    pub fn number(self) -> u8 {
        self as u8
    }

    /// The rule it belongs to (the table's last column).
    pub fn rule(self) -> &'static str {
        match self {
            Self::NotSession => "matching",
            Self::ListingFailed => "listing",
            Self::LockMalformed | Self::LockOwnerMayBeInUse | Self::LockOwnerUnknown => "lock",
            Self::Named | Self::NamingUndecidable => "named",
            Self::Rank => "rank",
            Self::RootUseUndecidable | Self::RootUsed | Self::UnusedRoot => "unused root",
        }
    }
}

/// What `S` holds as `.lock` (`None` of [`Facts::lock`]: no `.lock`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lock {
    /// Not a regular file, larger than the marker bound, unreadable or
    /// not a pid (Owner PID step 1): row 3. The text says which.
    Malformed(String),
    /// A regular file holding `pid`, last modified at `mtime`.
    Pid { pid: u32, mtime: Timestamp },
}

/// How one value a process carries was looked up in its root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Not absolute: never resolved (Python made it absolute against a
    /// working directory the process may since have left).
    Relative,
    /// Absolute, but the lookup failed (missing, a loop, a refusal).
    Unresolvable(String),
    /// The object's (device, inode), then (for `--basetemp`) those of its
    /// ancestors by `..`, up to the fixpoint.
    Resolved(Vec<DevIno>),
}

/// Where a `--basetemp` value came from: it decides the evidence of a
/// naming (`command_line` or `environment`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BasetempSource {
    CommandLine,
    /// `PYTEST_ADDOPTS` in the initial environment.
    Addopts,
}

/// What the scan knows of one process for the pytest contract. Only
/// processes that may belong to the process set are given: readable ones,
/// and unreadable ones that are not excused at a privilege boundary
/// (docs/safety.md, A process's paths). The verdict filters by owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFacts {
    pub pid: Pid,
    /// Its four uids (`None`: they could not be read).
    pub uids: Option<Ids>,
    /// Its command line (`None`: unreadable).
    pub cmdline: Option<Vec<OsString>>,
    /// Its initial environment (`None`: unreadable and not excused).
    pub environ: Option<Environ>,
    /// The (device, inode) of each of its tasks' working directories.
    pub cwds: Vec<DevIno>,
    /// Each non-empty value of [`TEMP_ROOT_VARS`] in its environment, with
    /// how it was looked up in its root.
    pub temp_roots: Vec<(OsString, Lookup)>,
    /// The (device, inode) of each of [`FALLBACK_ROOTS`] that exists in
    /// its root (a container's `/tmp` can be any directory of diskgc's
    /// view). One that does not exist (`ENOENT`, `ENOTDIR`) is not a root
    /// Python can use.
    pub fallback_roots: Vec<DevIno>,
    /// Why a fallback root's lookup failed otherwise (a refusal, `EAGAIN`
    /// exhausted, an I/O error): whether the process could use `R` through
    /// it cannot be decided (row 9).
    pub fallback_unresolved: Vec<String>,
    /// Each non-empty `--basetemp` value it carries, with where it came
    /// from and how it was looked up (climbed).
    pub basetemps: Vec<(BasetempSource, OsString, Lookup)>,
}

impl ProcessFacts {
    /// A process of which nothing could be read (its uids included): it
    /// is in every process set and undecidable everywhere.
    pub fn unreadable(pid: Pid) -> Self {
        Self {
            pid,
            uids: None,
            cmdline: None,
            environ: None,
            cwds: Vec::new(),
            temp_roots: Vec::new(),
            fallback_roots: Vec::new(),
            fallback_unresolved: Vec::new(),
            basetemps: Vec::new(),
        }
    }
}

/// The paths a process's strings name, for row 6: each absolute command
/// line argument and initial environment value, and each absolute
/// `:`-separated part of one, normalised ([`normalize`]), as bytes.
/// Derived from [`ProcessFacts`] once per scan instead of once per
/// session: re-deriving them for every session was most of a plan's CPU
/// on a host with thousands of sessions (TASK-184).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct NamedPaths {
    command_line: Vec<Vec<u8>>,
    environment: Vec<Vec<u8>>,
}

impl NamedPaths {
    fn of(process: &ProcessFacts) -> Self {
        let mut out = Self::default();
        for arg in process.cmdline.iter().flatten() {
            push_named(&mut out.command_line, arg.as_bytes());
        }
        for (_, value) in process.environ.iter().flat_map(Environ::vars) {
            push_named(&mut out.environment, value.as_bytes());
        }
        out
    }
}

/// The paths `text` names: itself and each `:`-separated part, the
/// absolute ones normalised. Only a text holding [`SESSION_PREFIX`] can
/// name a session: normalising drops `.` and empty names and folds `..`
/// but copies every name it keeps, so a path at or below `S` holds `S`'s
/// own name, `pytest-<N>` (row 1), verbatim in the text. The filter keeps
/// the per-scan cost a byte search over the environments instead of a
/// normalisation of every value (a laptop: ~330 processes, ~4.5 MB).
fn push_named(out: &mut Vec<Vec<u8>>, text: &[u8]) {
    if !text
        .windows(SESSION_PREFIX.len())
        .any(|w| w == SESSION_PREFIX)
    {
        return;
    }
    let normal = |text: &[u8]| normalize(text).map(|p| p.into_os_string().into_vec());
    out.extend(normal(text));
    if text.contains(&b':') {
        out.extend(text.split(|b| *b == b':').filter_map(normal));
    }
}

/// Whether the normalised path `named` is `sought` or lies below it, both
/// in normal form ([`normalize`]): `Path::starts_with` for such paths,
/// compared as bytes.
fn is_at_or_below(named: &[u8], sought: &[u8]) -> bool {
    named.starts_with(sought)
        && (named.len() == sought.len() || sought == b"/" || named[sought.len()] == b'/')
}

/// The process facts of one scan, with what row 6 compares derived once
/// ([`NamedPaths`]). Built only by [`Processes::new`], so the derived
/// part always belongs to the facts it was derived from.
#[derive(Debug, Clone, Default)]
pub struct Processes {
    facts: Vec<ProcessFacts>,
    named: Vec<NamedPaths>,
}

impl Processes {
    pub fn new(facts: Vec<ProcessFacts>) -> Self {
        let named = facts.iter().map(NamedPaths::of).collect();
        Self { facts, named }
    }

    /// The facts, in the order given.
    pub fn facts(&self) -> &[ProcessFacts] {
        &self.facts
    }

    fn iter(&self) -> impl Iterator<Item = (&ProcessFacts, &NamedPaths)> {
        self.facts.iter().zip(&self.named)
    }
}

/// Everything the table decides from, for one session `S = P/pytest-<N>`
/// in temp root `R`.
#[derive(Debug, Clone)]
pub struct Facts<'a> {
    /// `S`'s own name.
    pub session_name: &'a OsStr,
    /// The names in `P`, or why the listing failed or was cut short.
    pub listing: Result<&'a [OsString], String>,
    /// `S`'s `.lock`, if it holds one.
    pub lock: Option<Lock>,
    /// Owner PID's answer for the lock's pid (given exactly when `lock` is
    /// [`Lock::Pid`]).
    pub lock_owner: Option<OwnerPid>,
    /// The wall clock now (the scale of file times).
    pub now: Timestamp,
    /// The newest mtime or ctime anywhere in `S`'s tree, from the
    /// eligibility walk (`None`: some time was not reported: never old).
    pub newest_change: Option<Timestamp>,
    /// `Err`: an Owner PID precondition other than the filesystem one
    /// failed (the PID namespace, `hidepid`): the process list is not
    /// complete.
    pub process_list: Result<(), String>,
    /// The owner uid `U` of `P`.
    pub owner: u32,
    /// `S`'s path as diskgc sees it.
    pub session_path: &'a Path,
    /// The (device, inode) of every object the walk of `S` checked: a
    /// `--basetemp` value resolved into `S` meets one of them.
    pub session_tree: &'a HashSet<DevIno>,
    /// `R`'s path as diskgc sees it, and its (device, inode).
    pub root_path: &'a Path,
    pub root_id: DevIno,
    /// Every process the scan gave facts for.
    pub processes: &'a Processes,
}

/// The evidence of one process in a verdict's `process_blockers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Evidence {
    OwnerPid,
    CommandLine,
    Environment,
    TempRoot,
    UnresolvedPath,
    Unreadable,
}

/// What the table decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Eligible,
    Filtered,
    Unreadable,
    /// `producer_unknown`, with one entry per process that caused it,
    /// sorted by pid (empty: no single process did).
    ProducerUnknown(Vec<(Pid, Evidence)>),
    /// `in_use`, with one entry per process that blocks it, sorted by pid.
    InUse(Vec<(Pid, Evidence)>),
}

/// The deciding row, the outcome, and a human detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub row: Row,
    pub outcome: Outcome,
    pub detail: String,
}

impl Verdict {
    fn new(row: Row, outcome: Outcome, detail: impl Into<String>) -> Self {
        Self {
            row,
            outcome,
            detail: detail.into(),
        }
    }
}

/// What every session name starts with (row 1).
const SESSION_PREFIX: &[u8] = b"pytest-";

/// A session name's number: `pytest-` followed by ASCII digits without a
/// leading zero (`0` itself excepted), fitting 64 bits. Anything else is
/// not a session (row 1), though it may count toward `M`
/// ([`rank_number`]).
pub fn session_number(name: &[u8]) -> Option<u64> {
    let digits = name.strip_prefix(SESSION_PREFIX)?;
    if digits.len() > 1 && digits[0] == b'0' {
        return None;
    }
    rank_number(name)
}

/// A name's number for `M`: `pytest-` followed by ASCII digits, leading
/// zeros allowed, fitting 64 bits. Every such name is one pytest's
/// `int()` also reads, so diskgc's `M` is never above pytest's.
pub fn rank_number(name: &[u8]) -> Option<u64> {
    let digits = name.strip_prefix(b"pytest-")?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    digits.iter().try_fold(0u64, |acc, digit| {
        acc.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
    })
}

/// `M`: the largest [`rank_number`] in a listing of `P`.
pub fn largest(names: &[OsString]) -> Option<u64> {
    names.iter().filter_map(|n| rank_number(n.as_bytes())).max()
}

/// Whether `N` is below the rank: `N ≤ M − 3`.
pub fn below_rank(n: u64, m: Option<u64>) -> bool {
    m.is_some_and(|m| n.checked_add(RETENTION).is_some_and(|reach| reach <= m))
}

/// Whether `t` is older than the dead-lock age at `now` (a time in the
/// future is not old).
pub fn older_than_dead_lock_age(t: Timestamp, now: Timestamp) -> bool {
    // Compare (secs, nanos) without overflow: t + age < now.
    match t.secs.checked_add(DEAD_LOCK_AGE_SECS) {
        None => false,
        Some(reach) => (reach, t.nanos) < (now.secs, now.nanos),
    }
}

/// Lexical normalisation as Python's `os.path.abspath` does for an
/// absolute path (`posixpath.normpath`): `.`, empty names and `..` folded,
/// `..` at the root staying there. A relative string is not made
/// absolute: `None`.
pub fn normalize(text: &[u8]) -> Option<PathBuf> {
    if text.first() != Some(&b'/') {
        return None;
    }
    let mut names: Vec<&[u8]> = Vec::new();
    for name in text.split(|b| *b == b'/') {
        match name {
            b"" | b"." => {}
            b".." => {
                names.pop();
            }
            name => names.push(name),
        }
    }
    let mut out = Vec::with_capacity(text.len());
    for name in &names {
        out.push(b'/');
        out.extend_from_slice(name);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    Some(PathBuf::from(OsString::from_vec(out)))
}

/// The non-empty values of [`TEMP_ROOT_VARS`] in `environ`, every
/// occurrence (a duplicate name counts each value: the superset of what
/// any reader of the block takes). An empty value is ignored, as Python
/// ignores it.
pub fn temp_root_values(environ: &Environ) -> Vec<OsString> {
    environ
        .vars()
        .filter(|(name, value)| TEMP_ROOT_VARS.iter().any(|var| name == var) && !value.is_empty())
        .map(|(_, value)| value.to_owned())
        .collect()
}

/// The non-empty `--basetemp` values on a command line and in a
/// `PYTEST_ADDOPTS` (split as `shlex.split` does), as pytest's argparse
/// reads them: `--basetemp=DIR` or `--basetemp DIR` (pytest 9.1.1 turns
/// argparse's abbreviations off). Any process's, not only a pytest's.
pub fn basetemp_values(
    cmdline: Option<&[OsString]>,
    environ: Option<&Environ>,
) -> Vec<(BasetempSource, OsString)> {
    let mut out = Vec::new();
    let mut scan = |args: &[Vec<u8>], source: BasetempSource| {
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let value = if arg.as_slice() == b"--basetemp" {
                args.next().cloned()
            } else {
                arg.strip_prefix(b"--basetemp=").map(<[u8]>::to_vec)
            };
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                out.push((source, OsString::from_vec(value)));
            }
        }
    };
    if let Some(cmdline) = cmdline {
        let args: Vec<Vec<u8>> = cmdline.iter().map(|a| a.as_bytes().to_vec()).collect();
        scan(&args, BasetempSource::CommandLine);
    }
    for value in environ
        .into_iter()
        .flat_map(Environ::vars)
        .filter(|(name, _)| *name == "PYTEST_ADDOPTS")
        .map(|(_, value)| value)
    {
        scan(&shlex_split(value.as_bytes()), BasetempSource::Addopts);
    }
    out
}

/// What a process's facts look up in its own root, given its environment
/// and command line: every absolute (normalised) value of
/// [`TEMP_ROOT_VARS`] and Python's [`FALLBACK_ROOTS`] (not climbed), and
/// every absolute `--basetemp` value (climbed by `..`), each once.
/// Relative and empty values are never looked up.
pub fn wanted_lookups(
    environ: Option<&Environ>,
    cmdline: Option<&[OsString]>,
) -> Vec<(PathBuf, bool)> {
    let mut wanted: Vec<(PathBuf, bool)> = Vec::new();
    let mut add = |path: PathBuf, climb: bool| {
        if !wanted.contains(&(path.clone(), climb)) {
            wanted.push((path, climb));
        }
    };
    for value in environ.map(temp_root_values).unwrap_or_default() {
        if let Some(path) = normalize(value.as_bytes()) {
            add(path, false);
        }
    }
    for root in FALLBACK_ROOTS {
        add(PathBuf::from(root), false);
    }
    for (_, value) in basetemp_values(cmdline, environ) {
        if let Some(path) = normalize(value.as_bytes()) {
            add(path, true);
        }
    }
    wanted
}

/// POSIX `shlex.split` (Python's, `posix=True`, no comments): words
/// separated by whitespace, `'...'` literal, `"..."` with `\` escaping
/// `\`, `"`, `$` and `` ` ``, and `\` escaping any byte outside quotes. An
/// unclosed quote (where Python raises, and pytest does not start) ends
/// the last word at the end of the text: any reading only adds values.
pub fn shlex_split(text: &[u8]) -> Vec<Vec<u8>> {
    let mut words = Vec::new();
    let mut word: Option<Vec<u8>> = None;
    let mut bytes = text.iter().copied().peekable();
    while let Some(b) = bytes.next() {
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => {
                if let Some(done) = word.take() {
                    words.push(done);
                }
            }
            b'\'' => {
                let w = word.get_or_insert_with(Vec::new);
                for c in bytes.by_ref() {
                    if c == b'\'' {
                        break;
                    }
                    w.push(c);
                }
            }
            b'"' => {
                let w = word.get_or_insert_with(Vec::new);
                while let Some(c) = bytes.next() {
                    match c {
                        b'"' => break,
                        b'\\' => match bytes.peek() {
                            Some(&next @ (b'\\' | b'"' | b'$' | b'`')) => {
                                w.push(next);
                                bytes.next();
                            }
                            _ => w.push(b'\\'),
                        },
                        c => w.push(c),
                    }
                }
            }
            b'\\' => {
                let w = word.get_or_insert_with(Vec::new);
                if let Some(c) = bytes.next() {
                    w.push(c);
                }
            }
            b => word.get_or_insert_with(Vec::new).push(b),
        }
    }
    if let Some(done) = word {
        words.push(done);
    }
    words
}

/// Whether a process shows a pytest running (docs/safety.md, *A pytest is
/// running*): an argument containing `pytest` or `py.test`; the execnet
/// worker bootstrap (`-c` then [`EXECNET_BOOTSTRAP`] exactly); one of
/// `PYTEST_ADDOPTS`, `PYTEST_CURRENT_TEST`, `PYTEST_VERSION` or a
/// `PYTEST_XDIST_*` variable in its initial environment; or a command
/// line or environment that cannot be read.
pub fn shows_pytest(process: &ProcessFacts) -> bool {
    let (Some(cmdline), Some(environ)) = (&process.cmdline, &process.environ) else {
        return true;
    };
    let contains = |arg: &[u8], needle: &[u8]| arg.windows(needle.len()).any(|w| w == needle);
    let by_argument = cmdline
        .iter()
        .any(|a| contains(a.as_bytes(), b"pytest") || contains(a.as_bytes(), b"py.test"));
    let by_bootstrap = cmdline
        .windows(2)
        .any(|pair| pair[0] == "-c" && pair[1].as_bytes() == EXECNET_BOOTSTRAP);
    let by_environment = environ.vars().any(|(name, _)| {
        let name = name.as_bytes();
        name == b"PYTEST_ADDOPTS"
            || name == b"PYTEST_CURRENT_TEST"
            || name == b"PYTEST_VERSION"
            || name.starts_with(b"PYTEST_XDIST_")
    });
    by_argument || by_bootstrap || by_environment
}

/// How a process names `S`, if it does (row 6): an argument or an
/// environment value whose string names `S`'s path ([`NamedPaths`]), or a
/// `--basetemp` value resolved into `S`'s tree.
fn naming(
    process: &ProcessFacts,
    named: &NamedPaths,
    sought: &[u8],
    facts: &Facts<'_>,
) -> Option<Evidence> {
    let names = |paths: &[Vec<u8>]| paths.iter().any(|p| is_at_or_below(p, sought));
    if names(&named.command_line) {
        return Some(Evidence::CommandLine);
    }
    let resolved_into = |source: BasetempSource| {
        process.basetemps.iter().any(|(from, _, lookup)| {
            *from == source
                && matches!(lookup, Lookup::Resolved(chain)
                    if chain.iter().any(|id| facts.session_tree.contains(id)))
        })
    };
    if resolved_into(BasetempSource::CommandLine) {
        return Some(Evidence::CommandLine);
    }
    if names(&named.environment) || resolved_into(BasetempSource::Addopts) {
        return Some(Evidence::Environment);
    }
    None
}

/// Why a process makes the naming undecidable (row 8), if it does: an
/// unreadable command line or environment, or a relative or unresolvable
/// `--basetemp` value.
fn naming_undecidable(process: &ProcessFacts) -> Option<Evidence> {
    if process.cmdline.is_none() || process.environ.is_none() {
        return Some(Evidence::Unreadable);
    }
    process
        .basetemps
        .iter()
        .any(|(_, _, lookup)| !matches!(lookup, Lookup::Resolved(_)))
        .then_some(Evidence::UnresolvedPath)
}

/// Whether a process could use `R` as its temp root (row 10): a
/// temp-directory value resolving to `R`; `R` being one of Python's
/// fallbacks, by diskgc's path or in the process's own root (always
/// counted, whatever its variables say: an over-approximation); or its
/// working directory being `R` (an extra that can only retain).
fn uses_root(process: &ProcessFacts, facts: &Facts<'_>) -> bool {
    let fallback = FALLBACK_ROOTS
        .iter()
        .any(|root| facts.root_path.as_os_str() == *root)
        || process.fallback_roots.contains(&facts.root_id);
    fallback
        || process.cwds.contains(&facts.root_id)
        || process.temp_roots.iter().any(
            |(_, lookup)| matches!(lookup, Lookup::Resolved(chain) if chain.first() == Some(&facts.root_id)),
        )
}

/// Why a process makes the root use undecidable (row 9), if it does.
fn root_use_undecidable(process: &ProcessFacts) -> Option<Evidence> {
    if process.environ.is_none() {
        return Some(Evidence::Unreadable);
    }
    let unresolved = process
        .temp_roots
        .iter()
        .any(|(_, lookup)| !matches!(lookup, Lookup::Resolved(_)));
    (unresolved || !process.fallback_unresolved.is_empty()).then_some(Evidence::UnresolvedPath)
}

/// The pytest contract's ordered decision table (docs/safety.md, pytest
/// sessions): the first row that applies decides. Pure.
pub fn verdict(facts: &Facts<'_>) -> Verdict {
    // Row 1: the name.
    let Some(n) = session_number(facts.session_name.as_bytes()) else {
        return Verdict::new(
            Row::NotSession,
            Outcome::Filtered,
            "not a session name (pytest-<N>, N without a leading zero, fitting 64 bits)",
        );
    };
    // Row 2: the listing of P.
    let names = match facts.listing {
        Ok(names) => names,
        Err(ref why) => {
            return Verdict::new(
                Row::ListingFailed,
                Outcome::Unreadable,
                format!("the pytest-of directory could not be listed: {why}"),
            );
        }
    };
    let m = largest(names);
    // Rows 3 to 5: only with a lock; an ended owner continues at row 6.
    let mut lock_old = true;
    match &facts.lock {
        None => {}
        Some(Lock::Malformed(why)) => {
            return Verdict::new(
                Row::LockMalformed,
                Outcome::ProducerUnknown(Vec::new()),
                format!("its .lock is no pid marker: {why}"),
            );
        }
        Some(Lock::Pid { pid, mtime }) => {
            match facts
                .lock_owner
                .as_ref()
                .expect("a lock holding a pid comes with Owner PID's answer")
            {
                OwnerPid::MayBeInUse(carrier) => {
                    return Verdict::new(
                        Row::LockOwnerMayBeInUse,
                        Outcome::InUse(vec![(*carrier, Evidence::OwnerPid)]),
                        format!(
                            "pid {carrier} carries the pid {pid} its .lock records (a number \
                             match, not proof of identity)"
                        ),
                    );
                }
                OwnerPid::Unknown(unknown) => {
                    return Verdict::new(
                        Row::LockOwnerUnknown,
                        Outcome::ProducerUnknown(
                            unknown
                                .blockers
                                .iter()
                                .map(|pid| (*pid, Evidence::Unreadable))
                                .collect(),
                        ),
                        format!(
                            "whether the pid {pid} its .lock records ended cannot be decided: {}",
                            unknown.rule
                        ),
                    );
                }
                OwnerPid::Ended => lock_old = older_than_dead_lock_age(*mtime, facts.now),
            }
        }
    }
    let in_set = |p: &ProcessFacts| p.uids.is_none_or(|uids| uids.all().contains(&facts.owner));
    let set: Vec<&ProcessFacts> = facts
        .processes
        .facts()
        .iter()
        .filter(|p| in_set(p))
        .collect();
    let complete = facts.process_list.is_ok();
    // Row 6: a process names S.
    // S's path as diskgc found it is in normal form (roots refuse `.`,
    // `..`, repeated and trailing slashes; names are appended): a named
    // path compares with it as bytes.
    let sought = facts.session_path.as_os_str().as_bytes();
    assert_eq!(
        facts.session_path.file_name(),
        Some(facts.session_name),
        "the session path ends in another name than the session's"
    );
    assert!(
        normalize(sought).is_some_and(|p| p.as_os_str().as_bytes() == sought),
        "the session path {} is not absolute and normal",
        facts.session_path.display()
    );
    let named: Vec<(Pid, Evidence)> = facts
        .processes
        .iter()
        .filter(|(p, _)| in_set(p))
        .filter_map(|(p, paths)| naming(p, paths, sought, facts).map(|e| (p.pid, e)))
        .collect();
    if !named.is_empty() {
        return Verdict::new(
            Row::Named,
            Outcome::InUse(sorted(named)),
            "a process names the session in its command line or initial environment",
        );
    }
    // Row 7: the rank rule.
    let running = !complete || set.iter().any(|p| shows_pytest(p));
    let old_session = facts
        .newest_change
        .is_some_and(|t| older_than_dead_lock_age(t, facts.now));
    if below_rank(n, m) && lock_old && (!running || old_session) {
        return Verdict::new(
            Row::Rank,
            Outcome::Eligible,
            format!(
                "below pytest's retention (N {n} <= M {} - {RETENTION}), {}, and {}",
                m.expect("below the rank has an M"),
                if facts.lock.is_some() {
                    "its .lock's owner ended and the lock is past the dead-lock age"
                } else {
                    "no .lock"
                },
                if running {
                    "older than the dead-lock age while a pytest of the owner runs"
                } else {
                    "no pytest of the owner runs"
                }
            ),
        );
    }
    // Row 8: naming undecidable.
    if let Err(why) = &facts.process_list {
        return Verdict::new(
            Row::NamingUndecidable,
            Outcome::ProducerUnknown(Vec::new()),
            format!("the process list is not complete: {why}"),
        );
    }
    let undecided: Vec<(Pid, Evidence)> = set
        .iter()
        .filter_map(|p| naming_undecidable(p).map(|e| (p.pid, e)))
        .collect();
    if !undecided.is_empty() {
        return Verdict::new(
            Row::NamingUndecidable,
            Outcome::ProducerUnknown(sorted(undecided)),
            "whether a process names the session cannot be decided (an unreadable command \
             line or environment, or a relative or unresolvable --basetemp)",
        );
    }
    // Rows 9 and 10: root use (exclusive: undecidable only when unused).
    let using: Vec<(Pid, Evidence)> = set
        .iter()
        .filter(|p| uses_root(p, facts))
        .map(|p| (p.pid, Evidence::TempRoot))
        .collect();
    if using.is_empty() {
        let undecided: Vec<(Pid, Evidence)> = set
            .iter()
            .filter_map(|p| root_use_undecidable(p).map(|e| (p.pid, e)))
            .collect();
        if !undecided.is_empty() {
            return Verdict::new(
                Row::RootUseUndecidable,
                Outcome::ProducerUnknown(sorted(undecided)),
                format!(
                    "whether a process could use the temp root {} cannot be decided (a \
                     relative or unresolvable temp-directory value, or an unreadable \
                     environment)",
                    facts.root_path.display()
                ),
            );
        }
        return Verdict::new(
            Row::UnusedRoot,
            Outcome::Eligible,
            format!(
                "no process of the owner could use the temp root {}",
                facts.root_path.display()
            ),
        );
    }
    Verdict::new(
        Row::RootUsed,
        Outcome::InUse(sorted(using)),
        format!(
            "a process of the owner could use the temp root {}",
            facts.root_path.display()
        ),
    )
}

fn sorted(mut blockers: Vec<(Pid, Evidence)>) -> Vec<(Pid, Evidence)> {
    blockers.sort();
    blockers.dedup_by_key(|(pid, _)| *pid);
    blockers
}

#[cfg(test)]
mod tests;
