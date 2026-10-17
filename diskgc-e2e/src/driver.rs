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
//!   diskgc-core `inuse::delete_unused` (lock, walk, in-use check,
//!   delete): also `0 skip in_use|locked|contains_ipc ...`, and `0 failed
//!   <tag> <dir> -- <message>` for an error of the run; the scan's notes
//!   (unreadable processes and why they do or do not block) go to stderr;
//! - `held`: `I held pid=<pid> command=<command> bytes=<bytes>` per process
//!   holding deleted files open on DIR's filesystem;
//!
//! where `<path>` is Rust's `{:?}` of the path, and `<tag>` is the errno
//! name, `invalid` or `sandbox_gate` ([`cause_tag`]), or the skip reason of
//! a failed traversal. Control lines ([`ControlLine`]) come in between; the
//! last line is `done`. Exit code 0 when every entry succeeded (`walk`) or
//! the candidate was removed (`delete`) and every planned point was
//! reached, otherwise 1 (2: usage).

use std::ffi::OsString;
use std::path::Path;

use diskgc_core::engine::{
    self, Candidate, DEFAULT_MAX_ENTRIES, DeleteOptions, Deletion, End, Rules, WalkStop,
};
use diskgc_core::host::{Access, Base, Delete, DirListing, FileType, Host};
use diskgc_core::inuse::{self, Checker, Options, REUSE_LIMIT, Stop, delete_unused};
use diskgc_core::resolve::{ResolveError, Want, open_beneath, resolve};
use diskgc_linux::LinuxHost;

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
    let rules = Rules {
        owners: &[owner],
        block_ipc: true,
        max_entries: DEFAULT_MAX_ENTRIES,
    };
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
/// (`inuse::delete_unused`): lock DIR, walk it for the user `owner`, check
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
        owners: &owners,
        block_ipc: !ipc_live,
        max_entries: DEFAULT_MAX_ENTRIES,
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
