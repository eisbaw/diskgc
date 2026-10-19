//! The e2e test driver (`diskgc-e2e-driver`): single diskgc operations on a
//! container fixture, over `LinuxHost` wrapped in [`Controlled`], so the
//! harness can pause or kill it at a chosen port call (docs/testing.md,
//! Controlled pause and kill points).
//!
//! It exists for the container tests that need a diskgc process stopped at
//! an exact call before the `ensure` command can do the real thing (race
//! tests 5 and 6, the replace-between-walk-and-unlink test). Each operation
//! is one subcommand, generic over the port so unit tests run it on
//! `ModelHost`; later tasks add theirs (in-use check, Cargo discovery and
//! protocol) to [`run`].
//!
//! It deletes only through the port's [`Delete`] methods, so on `LinuxHost`
//! through the pre-release sandbox gate, and it calls the interlock first:
//! outside a diskgc test container it does nothing.
//!
//! Output (stdout), one line per result, indexed from 0 in output order:
//!
//! - `walk`: `I <type> <path>` per entry, or `I err <tag> <path> -- <message>`,
//!   `<path>` relative to the walked directory;
//! - `delete`: one line, the deletion engine's outcome (diskgc-core
//!   `engine`): `0 removed entries=N bytes=B` when the candidate is gone,
//!   `0 skip <reason> <path> -- <detail>` when the eligibility walk refused
//!   it, `0 failed <tag> <path> -- <message>` when the walk hit an error of
//!   the run (e.g. `EMFILE`), `0 changed <path> entries=N -- <detail>` or `0 err <tag> <path>
//!   entries=N -- <message>` when the delete stopped (N entries removed
//!   before), `<path>` absolute;
//! - `reclaim`: one line like `delete`'s, for the whole deletion step of
//!   the test support `diskgc_model::support::delete_unused` (lock, walk, in-use check,
//!   delete): also `0 skip in_use|locked|contains_ipc ...`, and `0 failed
//!   <tag> <dir> -- <message>` for an error of the run; the scan's notes
//!   (unreadable processes and why they do or do not block) go to stderr;
//! - `held`: `I held pid=<pid> command=<command> bytes=<bytes>` per process
//!   holding deleted files open on DIR's filesystem;
//! - `cargo-targets`: Cargo discovery and eligibility (diskgc-core
//!   `cargo_target`), nothing locked or deleted: `I refused <reason> <path>
//!   -- <detail>` per root or directory discovery refused, `I duplicate
//!   <path>` per target found again through another root, and per target
//!   `I target <path> <profile>=<layout>:<lock>,<lock> ...` (layout
//!   `classic`, `per-unit` or `unknown`) followed by its verdict, `I
//!   eligible <path> last_build=<secs|unknown> entries=N bytes=B` or `I
//!   skip <reason> <path> -- <where>: <detail>` (an in-use detail lists
//!   every piece of evidence, `; `-separated). Skips are results, not
//!   failures; only an error of the run is (`I failed <tag> <path> --
//!   <message>`);
//! - `cargo-reclaim`: the Cargo reclaim protocol (diskgc-core
//!   `cargo_target::reclaim`) on every target discovery finds: `I refused
//!   ...` and `I duplicate ...` as for `cargo-targets`, then per target its
//!   `I target ...` line and, with `--after-eligibility`, a refusal by the
//!   eligibility check (`I skip ...`, the protocol not run), otherwise the
//!   protocol's outcome: `I reclaimed <path> entries=N bytes=B
//!   held_us=<µs|none>`, `I skip <reason> <path> entries=N
//!   held_us=<µs|none> -- <where>: <detail>` (N entries deleted before it
//!   stopped), or, counted as failures, `I failed <tag> <path> entries=N
//!   -- <message>` (an error for the target, the sandbox gate, or an error
//!   of the run), `held_us` being how long the target's locks were held;
//! - `host-lock GATE`: the host lock (diskgc-core `hostlock::acquire`:
//!   `/run/diskgc/lock`, exclusive, inheritable by children), then a child
//!   (`/bin/sh`) that waits until the file GATE exists, then the driver
//!   closes its own descriptor and exits without waiting for the child:
//!   `0 locked waited_ms=N`, `1 child pid=P`, `2 closed`. The child holds
//!   the inherited copy, so the lock stays held until it exits
//!   (docs/safety.md, Host lock);
//!
//! where `<path>` is Rust's `{:?}` of the path, and `<tag>` is the errno
//! name, `invalid` or `sandbox_gate` ([`cause_tag`]), or the skip reason of
//! a failed traversal. Control lines ([`ControlLine`]) come in between; the
//! last line is `done`. Exit code 0 when every entry succeeded (`walk`) or
//! the candidate was removed (`delete`) and every planned point was
//! reached, otherwise 1 (2: usage).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use diskgc_core::cargo_target::{self, Layout, ReclaimEnd};
use diskgc_core::engine::{
    self, Candidate, DEFAULT_MAX_ENTRIES, DeleteOptions, Deletion, End, Rules, WalkStop,
};
use diskgc_core::host::{Access, Base, Delete, DirListing, FileType, Host};
use diskgc_core::inuse::{self, Checker, Options, REUSE_LIMIT};
use diskgc_core::profile::{IdleFor, Library};
use diskgc_core::resolve::{ResolveError, Want, open_beneath, resolve};
use diskgc_linux::LinuxHost;
use diskgc_model::support::{Stop, delete_unused};

use crate::controlled::{Action, ControlLine, Controlled, Plan, Point, stdio_control};
use crate::exit;
use crate::host_probe::cause_tag;

/// The driver's usage. Also a needle of the release check.
pub const USAGE: &str = "\
usage: diskgc-e2e-driver [--pause-at POINT]... [--kill-at POINT]... [--max-open-files N] OPERATION ARGS...
  POINT: K (the K-th port call, from 1) or OP:N (the N-th call of port
  operation OP, e.g. unlinkat:2); the action runs before that call
  --max-open-files N: set RLIMIT_NOFILE to N first
  operations:
    walk DIR                      every entry below DIR, depth first, names sorted
    delete [--make-writable] DIR  the deletion engine on DIR: its eligibility
                                  walk (owned by this user, any socket or FIFO
                                  keeps it), then its identity-checked delete
    reclaim [--ipc-live] DIR      the deletion step: flock DIR (locked if
                                  someone holds one), the walk, the in-use
                                  check of every visible process, the delete;
                                  --ipc-live: only live sockets and FIFOs keep it
    held DIR                      processes holding deleted files open on
                                  DIR's filesystem, with their bytes
    cargo-targets [--idle-for DUR] ROOT...
                                  Cargo targets below the ROOTs and whether
                                  each is eligible now (idle for DUR, e.g.
                                  1d); locks and deletes nothing
    cargo-reclaim [--idle-for DUR] [--after-eligibility] ROOT...
                                  the Cargo reclaim protocol on every target
                                  below the ROOTs: lock every lock file,
                                  recheck under the locks, delete inside the
                                  locked profile directories, release;
                                  --after-eligibility: judge each target
                                  first, as a plan would, reclaim only an
                                  eligible one and pass the protocol the
                                  last build it measured
    host-lock GATE                take the host lock, start a child that
                                  inherits it and waits for the file GATE,
                                  close the driver's copy, exit
Runs only inside a diskgc test container.";

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub plan: Plan,
    /// `--max-open-files`: the `RLIMIT_NOFILE` to run under.
    pub max_open_files: Option<u64>,
    pub operation: String,
    pub args: Vec<String>,
}

/// Parses `[--pause-at P | --kill-at P | --max-open-files N]... OPERATION
/// ARGS...`.
pub fn parse(args: &[String]) -> Result<Invocation, String> {
    let mut plan = Plan::new();
    let mut max_open_files = None;
    let mut rest = args;
    loop {
        let flag = match rest.first().map(String::as_str) {
            Some(flag @ ("--pause-at" | "--kill-at" | "--max-open-files")) => flag,
            Some(flag) if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            _ => break,
        };
        let value = rest.get(1).ok_or_else(|| format!("{flag} needs a value"))?;
        match flag {
            "--pause-at" => plan = plan.at(Point::parse(value)?, Action::Pause),
            "--kill-at" => plan = plan.at(Point::parse(value)?, Action::Kill),
            _ => {
                let n = value
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("--max-open-files {value:?}: not a positive number"))?;
                max_open_files = Some(n);
            }
        }
        rest = &rest[2..];
    }
    let (operation, args) = rest.split_first().ok_or("no operation given")?;
    Ok(Invocation {
        plan,
        max_open_files,
        operation: operation.clone(),
        args: args.to_vec(),
    })
}

/// Sets `RLIMIT_NOFILE` (soft and hard) to `n`.
fn limit_open_files(n: u64) -> Result<(), String> {
    let limit = rustix::process::Rlimit {
        current: Some(n),
        maximum: Some(n),
    };
    rustix::process::setrlimit(rustix::process::Resource::Nofile, limit)
        .map_err(|e| format!("setrlimit RLIMIT_NOFILE {n}: {e}"))
}

/// Entry point of `diskgc-e2e-driver` (after the interlock); returns the
/// exit code.
pub fn main(args: &[String]) -> i32 {
    let invocation = match parse(args) {
        Ok(invocation) => invocation,
        Err(e) => {
            eprintln!("diskgc-e2e-driver: {e}\n{USAGE}");
            return exit::USAGE;
        }
    };
    if let Some(n) = invocation.max_open_files
        && let Err(e) = limit_open_files(n)
    {
        eprintln!("diskgc-e2e-driver: {e}");
        return exit::FAIL;
    }
    let linux = match LinuxHost::new() {
        Ok(host) => host,
        Err(e) => {
            eprintln!("diskgc-e2e-driver: host setup: {e}");
            return exit::FAIL;
        }
    };
    let host = Controlled::new(linux, invocation.plan, Box::new(stdio_control));
    let owner = rustix::process::geteuid().as_raw();
    let outcome = run(
        &host,
        owner,
        &invocation.operation,
        &invocation.args,
        &mut |line| println!("{line}"),
    );
    println!(
        "{}",
        ControlLine::Done {
            calls: host.calls()
        }
    );
    // Every reason to fail is reported, not only the first.
    let mut failed = false;
    match outcome {
        Ok(Outcome { failed: 0 }) => {}
        Ok(Outcome { failed: n }) => {
            eprintln!("diskgc-e2e-driver: {n} entr(y/ies) failed (see the err lines)");
            failed = true;
        }
        Err(e) => {
            eprintln!("diskgc-e2e-driver: {e}");
            failed = true;
        }
    }
    let unreached = host.unreached();
    if !unreached.is_empty() {
        eprintln!(
            "diskgc-e2e-driver: planned point(s) never reached in {} port calls: {unreached:?}",
            host.calls()
        );
        failed = true;
    }
    if failed { exit::FAIL } else { exit::PASS }
}

/// How an operation went: how many entries failed (each reported on its
/// own line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub failed: usize,
}

/// Runs `operation` with `args` on `host`, for the user `owner` (the user
/// the driver runs as), handing each result line to `emit`. `Err` when the
/// operation could not start (bad arguments, its directory unreachable).
pub fn run<H: Delete>(
    host: &H,
    owner: u32,
    operation: &str,
    args: &[String],
    emit: &mut dyn FnMut(String),
) -> Result<Outcome, String> {
    let mut out = Lines {
        emit,
        next: 0,
        failed: 0,
    };
    let options = |make_writable| DeleteOptions {
        make_writable,
        deadline: None,
    };
    match (operation, args) {
        ("walk", [dir]) => walk(host, Path::new(dir), &mut out)?,
        ("delete", [dir]) => delete(host, owner, Path::new(dir), options(false), &mut out)?,
        ("delete", [flag, dir]) if flag == "--make-writable" => {
            delete(host, owner, Path::new(dir), options(true), &mut out)?;
        }
        ("reclaim", [dir]) => reclaim(host, owner, Path::new(dir), false, &mut out)?,
        ("reclaim", [flag, dir]) if flag == "--ipc-live" => {
            reclaim(host, owner, Path::new(dir), true, &mut out)?;
        }
        ("held", [dir]) => held(host, owner, Path::new(dir), &mut out)?,
        ("cargo-targets", [flag, idle, roots @ ..])
            if flag == "--idle-for" && !roots.is_empty() =>
        {
            let idle = diskgc_core::units::parse_duration(idle)?;
            cargo_targets(host, owner, Some(IdleFor(idle)), roots, &mut out)?;
        }
        ("cargo-targets", roots) if !roots.is_empty() && !roots[0].starts_with("--") => {
            cargo_targets(host, owner, None, roots, &mut out)?;
        }
        ("cargo-reclaim", args) => {
            let (flags, roots) = ReclaimFlags::parse(args)?;
            cargo_reclaim(host, owner, flags, roots, &mut out)?;
        }
        ("host-lock", [gate]) => host_lock(host, Path::new(gate), &mut out)?,
        _ => return Err(format!("bad operation {operation:?} {args:?}")),
    }
    Ok(Outcome { failed: out.failed })
}

/// Numbered result lines.
struct Lines<'a> {
    emit: &'a mut dyn FnMut(String),
    next: usize,
    failed: usize,
}

impl Lines<'_> {
    fn ok(&mut self, text: String) {
        (self.emit)(format!("{} {text}", self.next));
        self.next += 1;
    }

    fn err(&mut self, tag: &str, path: &Path, message: impl std::fmt::Display) {
        self.failed += 1;
        self.ok(format!("err {tag} {path:?} -- {message}"));
    }

    fn resolve_err(&mut self, path: &Path, e: &ResolveError) {
        self.err(e.reason(), path, e);
    }

    /// A result line that counts as a failure.
    fn err_line(&mut self, text: String) {
        self.failed += 1;
        self.ok(text);
    }
}

fn type_name(file_type: Option<FileType>) -> &'static str {
    match file_type {
        Some(FileType::Regular) => "file",
        Some(FileType::Directory) => "dir",
        Some(FileType::Symlink) => "symlink",
        Some(FileType::Socket) => "socket",
        Some(FileType::Fifo) => "fifo",
        Some(FileType::CharDevice | FileType::BlockDevice) => "device",
        Some(FileType::Unknown(_)) => "unknown",
        None => "unanswered",
    }
}

/// The names of the open directory `fd`, sorted (the kernel's order is the
/// filesystem's, and tests address calls by position).
fn sorted_names<H: Host>(
    host: &H,
    fd: &H::Fd,
) -> Result<Vec<OsString>, diskgc_core::host::HostError> {
    match host.read_dir(fd, DEFAULT_MAX_ENTRIES)? {
        DirListing::Names(mut names) => {
            names.sort();
            Ok(names)
        }
        DirListing::TooMany => Err(diskgc_core::host::HostError::invalid(
            "read_dir",
            format!("{fd:?}"),
            format!("more than {DEFAULT_MAX_ENTRIES} entries"),
        )),
    }
}

/// `walk DIR`: every entry below DIR, depth first, each statx-ed (not
/// followed); directories are entered with `open_beneath`, so symlinks and
/// mount crossings are refused like everywhere else.
fn walk<H: Host>(host: &H, dir: &Path, out: &mut Lines<'_>) -> Result<(), String> {
    let root = resolve(host, dir, Want::Directory).map_err(|e| e.to_string())?;
    walk_below(host, &root.fd, root.mount.id, Path::new(""), out);
    Ok(())
}

fn walk_below<H: Host>(host: &H, fd: &H::Fd, mount_id: u64, rel: &Path, out: &mut Lines<'_>) {
    let names = match sorted_names(host, fd) {
        Ok(names) => names,
        Err(e) => return out.err(&cause_tag(&e), rel, e),
    };
    for name in names {
        let path = rel.join(&name);
        let stat = match host.statx(Base::Dir(fd), Path::new(&name)) {
            Ok(stat) => stat,
            Err(e) => {
                out.err(&cause_tag(&e), &path, e);
                continue;
            }
        };
        out.ok(format!("{} {path:?}", type_name(stat.file_type)));
        if stat.file_type == Some(FileType::Directory) {
            match open_beneath(
                host,
                fd,
                mount_id,
                Path::new(&name),
                Access::Path,
                Want::Directory,
            ) {
                Ok((sub, _)) => walk_below(host, &sub, mount_id, &path, out),
                Err(e) => out.resolve_err(&path, &e),
            }
        }
    }
}

/// `delete DIR`: the deletion engine on DIR, as `fs` candidates will use
/// it (the in-use check, task-6, is not run): the eligibility walk for the
/// user `owner` with the default IPC policy (any socket or FIFO keeps the
/// candidate), then the identity-checked delete of what it recorded.
/// Prints one result line (see the module docs).
fn delete<H: Delete>(
    host: &H,
    owner: u32,
    dir: &Path,
    options: DeleteOptions,
    out: &mut Lines<'_>,
) -> Result<(), String> {
    let parent = parent_of(host, dir)?;
    let candidate = Candidate::new(&parent, dir)
        .ok_or_else(|| format!("{} does not end in a name", dir.display()))?;
    let owners = [owner];
    let rules = Rules::new(&owners);
    let record = match engine::walk(host, &candidate, &rules, None) {
        Ok(record) => record,
        Err(stop) => {
            walk_stopped(stop, out);
            return Ok(());
        }
    };
    deleted(engine::delete(host, &candidate, &record, options), out);
    Ok(())
}

/// The result line of a walk that ended without a record.
fn walk_stopped(stop: WalkStop, out: &mut Lines<'_>) {
    match stop {
        WalkStop::Skipped(skip) => out.err_line(format!(
            "skip {} {:?} -- {}",
            skip.reason, skip.path, skip.detail
        )),
        WalkStop::DeadlinePassed { path } => out.err_line(format!("deadline {path:?}")),
        WalkStop::Failed { path, error } => {
            out.err_line(format!("failed {} {path:?} -- {error}", cause_tag(&error)));
        }
    }
}

/// The result line of a delete.
fn deleted(done: Deletion, out: &mut Lines<'_>) {
    let n = done.removed_entries;
    match done.end {
        End::Complete => out.ok(format!("removed entries={n} bytes={}", done.removed_bytes)),
        End::Changed { path, detail } => {
            out.err_line(format!("changed {path:?} entries={n} -- {detail}"));
        }
        End::DeadlinePassed { path } => out.err_line(format!("deadline {path:?} entries={n}")),
        End::Failed { path, error } => out.err_line(format!(
            "err {} {path:?} entries={n} -- {error}",
            cause_tag(&error)
        )),
        End::GateRefused { path, error } => out.err_line(format!(
            "err {} {path:?} entries={n} -- {error}",
            cause_tag(&error)
        )),
    }
}

/// The parent of `dir`, resolved, for a candidate at `dir`.
fn parent_of<H: Host>(
    host: &H,
    dir: &Path,
) -> Result<diskgc_core::resolve::Resolved<H::Fd>, String> {
    let parent_path = dir
        .parent()
        .filter(|_| dir.file_name().is_some())
        .ok_or_else(|| format!("{} has no parent directory", dir.display()))?;
    resolve(host, parent_path, Want::Directory).map_err(|e| e.to_string())
}

/// `reclaim [--ipc-live] DIR`: the deletion step of diskgc-core
/// (`diskgc_model::support::delete_unused`): lock DIR, walk it for the user `owner`, check
/// it against a scan of every visible process, delete it. With
/// `ipc_live`, sockets and FIFOs keep it only when live. Prints one result
/// line, and the scan's notes on stderr.
fn reclaim<H: Delete>(
    host: &H,
    owner: u32,
    dir: &Path,
    ipc_live: bool,
    out: &mut Lines<'_>,
) -> Result<(), String> {
    let parent = parent_of(host, dir)?;
    let candidate = Candidate::new(&parent, dir)
        .ok_or_else(|| format!("{} does not end in a name", dir.display()))?;
    let owners = [owner];
    let rules = Rules {
        block_ipc: !ipc_live,
        ..Rules::new(&owners)
    };
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let result = delete_unused(
        host,
        &mut checker,
        &candidate,
        &rules,
        DeleteOptions::default(),
    );
    if let Some(scan) = checker.latest() {
        eprintln!(
            "diskgc-e2e-driver: scan of {} processes ({} listings, {} gone) took {:?}",
            scan.processes(),
            scan.listings(),
            scan.gone(),
            scan.took()
        );
        for note in scan.notes() {
            eprintln!("diskgc-e2e-driver: note: {note}");
        }
    }
    match result {
        Ok(done) => deleted(done, out),
        Err(Stop::Skipped(skip)) => out.err_line(format!(
            "skip {} {:?} -- {}",
            skip.reason, skip.path, skip.detail
        )),
        Err(Stop::Walk(stop)) => walk_stopped(stop, out),
        Err(Stop::Failed(error)) => {
            out.err_line(format!("failed {} {dir:?} -- {error}", cause_tag(&error)));
        }
    }
    Ok(())
}

/// `held DIR`: the processes holding deleted files open on DIR's
/// filesystem (`held_by_deleted_files`), one line each.
fn held<H: Host>(host: &H, owner: u32, dir: &Path, out: &mut Lines<'_>) -> Result<(), String> {
    let at = resolve(host, dir, Want::Directory).map_err(|e| e.to_string())?;
    let scan = inuse::scan(
        host,
        &Options {
            users: &[owner],
            sockets: false,
        },
    )
    .map_err(|e| format!("scan: {e}"))?;
    for h in scan.held_by_deleted_files(at.stat.dev) {
        out.ok(format!(
            "held pid={} command={} bytes={}",
            h.pid,
            h.command.to_string_lossy(),
            h.bytes
        ));
    }
    Ok(())
}

/// The walk rules the executor builds for a `profile:cargo-target` tier
/// run by `owners`: the defaults ([`Rules::new`]) and the built-in
/// profile's `checkouts` setting, read from the library as the executor
/// does ([`Library::checkouts`]), never stated here.
fn cargo_target_rules(owners: &[u32]) -> Result<Rules<'_>, String> {
    let library =
        Library::builtin().map_err(|e| format!("the built-in profiles do not load: {e}"))?;
    let checkouts = library
        .checkouts("cargo-target")
        .ok_or("no built-in source profile `cargo-target`")?;
    Ok(Rules {
        checkouts,
        ..Rules::new(owners)
    })
}

/// `cargo-targets [--idle-for DUR] ROOT...`: discovery below the roots,
/// then the eligibility of every target found, for the user `owner` (see
/// the module docs for the lines). Locks and deletes nothing.
fn cargo_targets<H: Host>(
    host: &H,
    owner: u32,
    idle_for: Option<IdleFor>,
    roots: &[String],
    out: &mut Lines<'_>,
) -> Result<(), String> {
    let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
    let found = match cargo_target::discover(host, &roots, None) {
        Ok(found) => found,
        Err(stop) => {
            cargo_stop(&stop, out);
            return Ok(());
        }
    };
    for skip in &found.refused {
        out.ok(format!(
            "refused {} {:?} -- {}",
            skip.reason, skip.path, skip.detail
        ));
    }
    for path in &found.duplicates {
        out.ok(format!("duplicate {path:?}"));
    }
    let owners = [owner];
    let rules = cargo_target_rules(&owners)?;
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    for target in &found.targets {
        out.ok(format!(
            "target {:?} {}",
            target.path,
            profiles_text(target)
        ));
        match cargo_target::eligibility(host, target, &rules, idle_for, &mut checker, None) {
            Ok(Ok(e)) => out.ok(format!(
                "eligible {:?} last_build={} entries={} bytes={}",
                target.path,
                e.last_build
                    .map_or("unknown".to_owned(), |t| t.secs.to_string()),
                e.entries,
                e.bytes
            )),
            Ok(Err(skip)) => out.ok(format!(
                "skip {} {:?} -- {}: {}",
                skip.reason,
                target.path,
                skip.path.display(),
                skip.detail
            )),
            Err(stop) => cargo_stop(&stop, out),
        }
    }
    if let Some(scan) = checker.latest() {
        for note in scan.notes() {
            eprintln!("diskgc-e2e-driver: note: {note}");
        }
    }
    Ok(())
}

/// `host-lock GATE` (see the module docs): what the executor's host lock
/// gives a child. The child is started as diskgc will start prune, GC and
/// command children (no fd closing of its own), and is left running.
fn host_lock<H: Host>(host: &H, gate: &Path, out: &mut Lines<'_>) -> Result<(), String> {
    let lock = diskgc_core::hostlock::acquire(host, None).map_err(|e| e.to_string())?;
    out.ok(format!("locked waited_ms={}", lock.waited.as_millis()));
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", "while [ ! -e \"$1\" ]; do sleep 0.1; done", "sh"])
        .arg(gate)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn the child: {e}"))?;
    out.ok(format!("child pid={}", child.id()));
    // Closing is the only release (the port has none other); the child's
    // inherited copy keeps the lock.
    drop(lock);
    out.ok("closed".to_owned());
    Ok(())
}

/// The flags of `cargo-reclaim`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ReclaimFlags {
    idle_for: Option<IdleFor>,
    after_eligibility: bool,
}

impl ReclaimFlags {
    /// `[--idle-for DUR] [--after-eligibility] ROOT...`: the flags and the
    /// roots (at least one).
    fn parse(args: &[String]) -> Result<(Self, &[String]), String> {
        let mut flags = Self::default();
        let mut rest = args;
        loop {
            match rest {
                [flag, value, more @ ..] if flag == "--idle-for" => {
                    flags.idle_for = Some(IdleFor(diskgc_core::units::parse_duration(value)?));
                    rest = more;
                }
                [flag, more @ ..] if flag == "--after-eligibility" => {
                    flags.after_eligibility = true;
                    rest = more;
                }
                [flag, ..] if flag.starts_with("--") => {
                    return Err(format!("cargo-reclaim: bad flag {flag:?}"));
                }
                [] => return Err("cargo-reclaim: no ROOT given".to_owned()),
                roots => return Ok((flags, roots)),
            }
        }
    }
}

/// `cargo-reclaim [--idle-for DUR] [--after-eligibility] ROOT...`:
/// discovery below the roots, then the reclaim protocol on every target
/// found, for the user `owner` (see the module docs for the lines).
fn cargo_reclaim<H: Delete>(
    host: &H,
    owner: u32,
    flags: ReclaimFlags,
    roots: &[String],
    out: &mut Lines<'_>,
) -> Result<(), String> {
    let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
    let found = match cargo_target::discover(host, &roots, None) {
        Ok(found) => found,
        Err(stop) => {
            cargo_stop(&stop, out);
            return Ok(());
        }
    };
    for skip in &found.refused {
        out.ok(format!(
            "refused {} {:?} -- {}",
            skip.reason, skip.path, skip.detail
        ));
    }
    for path in &found.duplicates {
        out.ok(format!("duplicate {path:?}"));
    }
    let owners = [owner];
    let rules = cargo_target_rules(&owners)?;
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    for target in &found.targets {
        out.ok(format!(
            "target {:?} {}",
            target.path,
            profiles_text(target)
        ));
        let judged = if flags.after_eligibility {
            match cargo_target::eligibility(
                host,
                target,
                &rules,
                flags.idle_for,
                &mut checker,
                None,
            ) {
                Ok(Ok(eligible)) => eligible.last_build,
                Ok(Err(skip)) => {
                    out.ok(format!(
                        "skip {} {:?} -- {}: {}",
                        skip.reason,
                        target.path,
                        skip.path.display(),
                        skip.detail
                    ));
                    continue;
                }
                Err(stop) => {
                    cargo_stop(&stop, out);
                    continue;
                }
            }
        } else {
            None
        };
        let done = cargo_target::reclaim(
            host,
            target,
            judged,
            &rules,
            flags.idle_for,
            &mut checker,
            None,
        );
        let held = done
            .locks_held
            .map_or("none".to_owned(), |d| d.as_micros().to_string());
        let n = done.removed_entries;
        match done.end {
            ReclaimEnd::Complete => out.ok(format!(
                "reclaimed {:?} entries={n} bytes={} held_us={held}",
                target.path, done.removed_bytes
            )),
            ReclaimEnd::Skipped(skip) => out.ok(format!(
                "skip {} {:?} entries={n} held_us={held} -- {}: {}",
                skip.reason,
                target.path,
                skip.path.display(),
                skip.detail
            )),
            ReclaimEnd::Failed { path, error } | ReclaimEnd::GateRefused { path, error } => out
                .err_line(format!(
                    "failed {} {path:?} entries={n} -- {error}",
                    cause_tag(&error)
                )),
            ReclaimEnd::Stopped(cargo_target::Stop::DeadlinePassed { path }) => {
                out.err_line(format!(
                    "failed deadline {path:?} entries={n} -- deadline passed"
                ));
            }
            ReclaimEnd::Stopped(cargo_target::Stop::Failed { path, error }) => {
                out.err_line(format!(
                    "failed {} {path:?} entries={n} -- {error}",
                    cause_tag(&error)
                ))
            }
        }
    }
    if let Some(scan) = checker.latest() {
        for note in scan.notes() {
            eprintln!("diskgc-e2e-driver: note: {note}");
        }
    }
    Ok(())
}

/// `debug=classic:.cargo-lock,.cargo-build-lock ...` for a target's
/// profile directories.
fn profiles_text(target: &cargo_target::Target) -> String {
    let profiles: Vec<String> = target
        .inspected
        .profiles
        .iter()
        .map(|p| {
            let layout = match p.layout() {
                Some(Layout::Classic) => "classic",
                Some(Layout::PerUnit) => "per-unit",
                None if p.is_empty() => "empty",
                None => "unknown",
            };
            let locks: Vec<String> = p
                .locks
                .iter()
                .map(|l| l.name.to_string_lossy().into_owned())
                .collect();
            format!("{}={layout}:{}", p.rel.display(), locks.join(","))
        })
        .collect();
    profiles.join(" ")
}

fn cargo_stop(stop: &cargo_target::Stop, out: &mut Lines<'_>) {
    match stop {
        cargo_target::Stop::DeadlinePassed { path } => out.err_line(format!("deadline {path:?}")),
        cargo_target::Stop::Failed { path, error } => {
            out.err_line(format!("failed {} {path:?} -- {error}", cause_tag(error)));
        }
    }
}

#[cfg(test)]
mod tests {
    //! The operations on `ModelHost`, which also fails a test on a call the
    //! safety rules forbid (an `unlinkat` without the identity check right
    //! before it, `openat2` without the `RESOLVE_*` flags).

    use super::*;
    use diskgc_model::ModelHost;

    const MODEL: &str = r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}
  - {id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}
  - {id: 31, at: /tmp/t/mnt, fs: tmpfs, dev: "0:43", size: 1M, parent: 30}
tree:
  /tmp/t:
    b: {}
    a b: {}
    sub:
      c: {}
      deeper: {d: {}}
    link: {symlink: sub}
    mnt: {.: {}}
  /tmp/keep: {}
"#;

    fn lines<H: Delete>(host: &H, op: &str, dir: &str) -> (Result<Outcome, String>, Vec<String>) {
        let mut got = Vec::new();
        let outcome = run(host, 1000, op, &[dir.to_owned()], &mut |l| got.push(l));
        (outcome, got)
    }

    fn model() -> ModelHost {
        ModelHost::from_yaml(MODEL).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn walk_lists_every_entry_depth_first_and_refuses_mount_crossings() {
        let (outcome, got) = lines(&model(), "walk", "/tmp/t");
        assert_eq!(got.len(), 9, "{got:?}");
        assert_eq!(
            got[..4],
            [
                "0 file \"a b\"",
                "1 file \"b\"",
                "2 symlink \"link\"",
                "3 dir \"mnt\""
            ]
        );
        // The mount on mnt is refused, not entered.
        assert!(
            got[4].starts_with("4 err unsafe_path \"mnt\" -- "),
            "{}",
            got[4]
        );
        assert_eq!(
            got[5..],
            [
                "5 dir \"sub\"",
                "6 file \"sub/c\"",
                "7 dir \"sub/deeper\"",
                "8 file \"sub/deeper/d\""
            ]
        );
        assert_eq!(outcome, Ok(Outcome { failed: 1 }));
    }

    #[test]
    fn delete_runs_the_engine_and_reports_its_outcome() {
        let model = ModelHost::from_yaml(&MODEL.replace("    mnt: {.: {}}\n", "").replace(
            "  - {id: 31, at: /tmp/t/mnt, fs: tmpfs, dev: \"0:43\", size: 1M, parent: 30}\n",
            "",
        ))
        .unwrap_or_else(|e| panic!("{e}"));
        let (outcome, got) = lines(&model, "delete", "/tmp/t");
        // t, "a b", b, link, sub, sub/c, sub/deeper, sub/deeper/d; three
        // directories of 4 KiB in the model.
        assert_eq!(got, ["0 removed entries=8 bytes=12288"]);
        assert_eq!(outcome, Ok(Outcome { failed: 0 }));
        let unlinked: Vec<String> = model
            .trace()
            .into_iter()
            .filter(|c| c.op == diskgc_model::spec::Op::Unlinkat)
            .map(|c| c.path.expect("a path").display().to_string())
            .collect();
        assert_eq!(
            unlinked,
            [
                "/tmp/t/a b",
                "/tmp/t/b",
                "/tmp/t/link",
                "/tmp/t/sub/c",
                "/tmp/t/sub/deeper/d",
                "/tmp/t/sub/deeper",
                "/tmp/t/sub",
                "/tmp/t"
            ]
        );
        assert!(model.stat_path(Path::new("/tmp/t")).is_none());
        assert!(model.stat_path(Path::new("/tmp/keep")).is_some());
    }

    #[test]
    fn a_refused_candidate_is_reported_and_nothing_is_deleted() {
        let model = model();
        let (outcome, got) = lines(&model, "delete", "/tmp/t");
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0].starts_with("0 skip contains_mount \"/tmp/t/mnt\" -- "),
            "{got:?}"
        );
        assert_eq!(outcome, Ok(Outcome { failed: 1 }));
        assert!(
            model
                .trace()
                .iter()
                .all(|c| c.op != diskgc_model::spec::Op::Unlinkat)
        );
        // Another user's candidate: not_owner.
        let mut got = Vec::new();
        let outcome = run(
            &model,
            1001,
            "delete",
            &["/tmp/keep".to_owned()],
            &mut |l| got.push(l),
        );
        assert_eq!(outcome, Ok(Outcome { failed: 1 }));
        assert!(
            got[0].starts_with("0 skip not_owner \"/tmp/keep\""),
            "{got:?}"
        );
    }

    /// `reclaim` runs the deletion step: a candidate a process uses is
    /// skipped `in_use` (nothing unlinked), a free one is removed; `held`
    /// lists a process holding a deleted file on the filesystem.
    #[test]
    fn reclaim_checks_in_use_and_held_lists_deleted_open_files() {
        let model = format!(
            "{MODEL}  /tmp/gone: {{size: 8K}}
processes:
  - {{pid: 42, exe: /tmp/keep, maps: [/tmp/keep], environ: [A=1], cwd: /tmp/t/sub,
     fds: [{{path: /tmp/gone, deleted: true}}]}}
"
        );
        let host = ModelHost::from_yaml(&model).unwrap_or_else(|e| panic!("{e}"));
        let (outcome, got) = lines(&host, "reclaim", "/tmp/t/sub");
        assert_eq!(outcome, Ok(Outcome { failed: 1 }));
        assert!(
            got[0].starts_with(
                "0 skip in_use \"/tmp/t/sub\" -- pid 42 (keep) has it as its \
                                working directory"
            ),
            "{got:?}"
        );
        let (outcome, got) = lines(&host, "reclaim", "/tmp/t/a b");
        assert_eq!(outcome, Ok(Outcome { failed: 0 }));
        assert_eq!(got, ["0 removed entries=1 bytes=0"]);
        let (outcome, got) = lines(&host, "held", "/tmp");
        assert_eq!(outcome, Ok(Outcome { failed: 0 }));
        assert_eq!(got, ["0 held pid=42 command=keep bytes=8192"]);
    }

    /// `cargo-targets` lists refused roots, duplicates, every target with
    /// its profiles, and each verdict; a skip is a result, not a failure.
    #[test]
    fn cargo_targets_reports_discovery_and_eligibility() {
        let model = ModelHost::from_yaml(
            r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}
  - {id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}
tree:
  /tmp/w/p/target:
    CACHEDIR.TAG: {cachedir_tag: true}
    debug: {.cargo-lock: {}, deps: {d: {size: 4K}}}
  /tmp/w/.hidden/q/target:
    CACHEDIR.TAG: {cachedir_tag: true}
    debug: {.cargo-lock: {}, .cargo-new-lock: {}, build: {.: {}}}
events:
  - {at: {call: 1, when: before}, advance: 2d}
"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let args: Vec<String> = ["--idle-for", "1d", "/tmp/w", "/tmp/w/p", "/tmp/nope"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let mut got = Vec::new();
        let outcome = run(&model, 1000, "cargo-targets", &args, &mut |l| got.push(l));
        assert_eq!(outcome, Ok(Outcome { failed: 0 }), "{got:#?}");
        assert!(
            got[0].starts_with("0 refused missing \"/tmp/nope\" -- "),
            "{got:#?}"
        );
        assert_eq!(got[1], "1 duplicate \"/tmp/w/p/target\"");
        assert_eq!(
            got[2],
            "2 target \"/tmp/w/.hidden/q/target\" debug=per-unit:.cargo-lock,.cargo-new-lock"
        );
        assert!(
            got[3].starts_with(
                "3 skip unsupported_layout \"/tmp/w/.hidden/q/target\" -- \
                 /tmp/w/.hidden/q/target/debug/.cargo-new-lock: an unknown Cargo lock file name"
            ),
            "{got:#?}"
        );
        assert_eq!(
            got[4],
            "4 target \"/tmp/w/p/target\" debug=classic:.cargo-lock"
        );
        assert_eq!(
            got[5],
            "5 eligible \"/tmp/w/p/target\" last_build=1792033200 entries=2 bytes=8192"
        );
        assert_eq!(got.len(), 6, "{got:#?}");
        // Nothing locked or deleted.
        assert!(model.trace().iter().all(|c| !matches!(
            c.op,
            diskgc_model::spec::Op::Unlinkat
                | diskgc_model::spec::Op::Fchmod
                | diskgc_model::spec::Op::Flock
        )));
        let bad = ["--idle-for".to_owned(), "1d".to_owned()];
        assert!(run(&model, 1000, "cargo-targets", &bad, &mut |_| {}).is_err());
        assert!(run(&model, 1000, "cargo-targets", &[], &mut |_| {}).is_err());
    }

    /// `cargo-reclaim` runs the protocol on every target found: one is
    /// reclaimed (every entry but its lock file, which stays), one whose
    /// lock another process holds is `locked` and keeps everything; with
    /// `--after-eligibility` a target the eligibility check refuses is not
    /// locked at all.
    #[test]
    fn cargo_reclaim_runs_the_protocol_on_every_target() {
        let model = ModelHost::from_yaml(
            r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}
  - {id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}
tree:
  /bin/sh: {}
  /tmp/w/p/Cargo.toml: {}
  /tmp/w/p/target:
    CACHEDIR.TAG: {cachedir_tag: true}
    debug: {.cargo-lock: {}, .fingerprint: {f: {}}, deps: {d: {size: 4K}}}
  /tmp/w/q/Cargo.toml: {}
  /tmp/w/q/target:
    CACHEDIR.TAG: {cachedir_tag: true}
    debug: {.cargo-lock: {}, deps: {d: {}}}
locks:
  - {path: /tmp/w/q/target/debug/.cargo-lock, holder: 42, mode: shared}
processes:
  - {pid: 42, exe: /bin/sh, cwd: /, maps: [/bin/sh], environ: [A=1]}
events:
  - {at: {call: 1, when: before}, advance: 2d}
"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let mut got = Vec::new();
        let args = ["/tmp/w".to_owned()];
        let outcome = run(&model, 1000, "cargo-reclaim", &args, &mut |l| got.push(l));
        assert_eq!(outcome, Ok(Outcome { failed: 0 }), "{got:#?}");
        assert_eq!(
            got[0],
            "0 target \"/tmp/w/p/target\" debug=classic:.cargo-lock"
        );
        assert!(
            got[1].starts_with("1 reclaimed \"/tmp/w/p/target\" entries=4 bytes=12288 held_us="),
            "{got:#?}"
        );
        assert_eq!(
            got[2],
            "2 target \"/tmp/w/q/target\" debug=classic:.cargo-lock"
        );
        assert!(
            got[3].starts_with("3 skip locked \"/tmp/w/q/target\" entries=0 held_us=none -- "),
            "{got:#?}"
        );
        assert_eq!(got.len(), 4);
        assert!(
            model
                .stat_path(Path::new("/tmp/w/p/target/debug/.cargo-lock"))
                .is_some()
        );
        assert!(
            model
                .stat_path(Path::new("/tmp/w/p/target/debug/deps"))
                .is_none()
        );
        assert!(
            model
                .stat_path(Path::new("/tmp/w/q/target/debug/deps/d"))
                .is_some()
        );

        // The eligibility check first: q's lock holder holds the lock
        // file open, so q is in use and the protocol never locks it.
        let flocks = |m: &ModelHost| {
            m.trace()
                .iter()
                .filter(|c| c.op == diskgc_model::spec::Op::Flock)
                .count()
        };
        let before = flocks(&model);
        let mut got = Vec::new();
        let args = ["--after-eligibility".to_owned(), "/tmp/w/q".to_owned()];
        let outcome = run(&model, 1000, "cargo-reclaim", &args, &mut |l| got.push(l));
        assert_eq!(outcome, Ok(Outcome { failed: 0 }), "{got:#?}");
        assert!(
            got[1].starts_with("1 skip in_use \"/tmp/w/q/target\" -- "),
            "{got:#?}"
        );
        assert_eq!(flocks(&model), before);
        for bad in [
            &[][..],
            &["--idle-for"],
            &["--idle-for", "1d"],
            &["--force", "/tmp/w"],
        ] {
            let bad: Vec<String> = bad.iter().map(|s| (*s).to_owned()).collect();
            assert!(
                run(&model, 1000, "cargo-reclaim", &bad, &mut |_| {}).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_command_line_takes_points_then_the_operation() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let parsed = parse(&args(&[
            "--pause-at",
            "unlinkat:2",
            "--max-open-files",
            "32",
            "--kill-at",
            "9",
            "delete",
            "/tmp/x",
        ]))
        .expect("parses");
        assert_eq!(parsed.max_open_files, Some(32));
        assert_eq!(
            parsed.plan,
            Plan::new()
                .at(
                    Point::Nth {
                        op: "unlinkat",
                        nth: 2
                    },
                    Action::Pause
                )
                .at(Point::Call(9), Action::Kill)
        );
        assert_eq!(parsed.operation, "delete");
        assert_eq!(parsed.args, ["/tmp/x"]);
        for bad in [
            &["--pause-at"][..],
            &["--stop-at", "1", "walk", "/tmp"],
            &["--kill-at", "0", "walk", "/tmp"],
            &["--max-open-files", "0", "walk", "/tmp"],
            &["--max-open-files", "many", "walk", "/tmp"],
            &[],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?} accepted");
        }
        let model = model();
        assert!(run(&model, 1000, "walk", &[], &mut |_| {}).is_err());
        assert!(run(&model, 1000, "rm", &["/tmp/t".to_owned()], &mut |_| {}).is_err());
        let bad_flag = ["--force".to_owned(), "/tmp/t".to_owned()];
        assert!(run(&model, 1000, "delete", &bad_flag, &mut |_| {}).is_err());
    }
}
