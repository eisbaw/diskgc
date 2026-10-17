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
//! - `walk`: `I <type> <path>` per entry, or `I err <tag> <path> -- <message>`;
//! - `delete`: `I unlinked <path> ino=<inode from the identity statx>` per
//!   entry, or `I err <tag> <path> -- <message>`;
//!
//! where `<path>` is Rust's `{:?}` of the path relative to the operation's
//! directory (`walk`) or to its parent (`delete`), and `<tag>` is the
//! errno name, `invalid` or `sandbox_gate` ([`cause_tag`]), or the skip
//! reason of a failed traversal. Control lines ([`ControlLine`]) come in
//! between; the last line is `done`. Exit code 0 when every entry
//! succeeded and every planned point was reached, otherwise 1 (2: usage).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use diskgc_core::host::{Access, Base, Delete, FileType, Host, Unlink};
use diskgc_core::resolve::{ResolveError, Want, open_beneath, resolve};
use diskgc_linux::LinuxHost;

use crate::controlled::{Action, ControlLine, Controlled, Plan, Point, stdio_control};
use crate::exit;
use crate::host_probe::cause_tag;

/// The driver's usage. Also a needle of the release check.
pub const USAGE: &str = "\
usage: diskgc-e2e-driver [--pause-at POINT]... [--kill-at POINT]... OPERATION ARGS...
  POINT: K (the K-th port call, from 1) or OP:N (the N-th call of port
  operation OP, e.g. unlinkat:2); the action runs before that call
  operations:
    walk DIR     every entry below DIR, depth first, names sorted
    delete DIR   DIR and everything below it, depth first, names sorted,
                 each entry statx-ed right before its unlinkat
Runs only inside a diskgc test container.";

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub plan: Plan,
    pub operation: String,
    pub args: Vec<String>,
}

/// Parses `[--pause-at P | --kill-at P]... OPERATION ARGS...`.
pub fn parse(args: &[String]) -> Result<Invocation, String> {
    let mut plan = Plan::new();
    let mut rest = args;
    loop {
        let action = match rest.first().map(String::as_str) {
            Some("--pause-at") => Action::Pause,
            Some("--kill-at") => Action::Kill,
            Some(flag) if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            _ => break,
        };
        let point = rest
            .get(1)
            .ok_or_else(|| format!("{} needs a POINT", rest[0]))?;
        plan = plan.at(Point::parse(point)?, action);
        rest = &rest[2..];
    }
    let (operation, args) = rest.split_first().ok_or("no operation given")?;
    Ok(Invocation {
        plan,
        operation: operation.clone(),
        args: args.to_vec(),
    })
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
    let linux = match LinuxHost::new() {
        Ok(host) => host,
        Err(e) => {
            eprintln!("diskgc-e2e-driver: host setup: {e}");
            return exit::FAIL;
        }
    };
    let host = Controlled::new(linux, invocation.plan, Box::new(stdio_control));
    let outcome = run(
        &host,
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

/// Runs `operation` with `args` on `host`, handing each result line to
/// `emit`. `Err` when the operation could not start (bad arguments, its
/// directory unreachable).
pub fn run<H: Delete>(
    host: &H,
    operation: &str,
    args: &[String],
    emit: &mut dyn FnMut(String),
) -> Result<Outcome, String> {
    let mut out = Lines {
        emit,
        next: 0,
        failed: 0,
    };
    match (operation, args) {
        ("walk", [dir]) => walk(host, Path::new(dir), &mut out)?,
        ("delete", [dir]) => delete(host, Path::new(dir), &mut out)?,
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
    let mut names = host.read_dir(fd)?;
    names.sort();
    Ok(names)
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

/// `delete DIR`: DIR and everything below it, depth first. Each entry is
/// statx-ed right before its `unlinkat` (the identity check's position,
/// docs/safety.md, Deletion), whose inode the result line reports. Not the
/// deletion engine (task-5): no comparison with a walk record, no
/// ownership or in-use checks.
fn delete<H: Delete>(host: &H, dir: &Path, out: &mut Lines<'_>) -> Result<(), String> {
    let (parent, name) = dir
        .parent()
        .zip(dir.file_name())
        .ok_or_else(|| format!("{} has no parent directory", dir.display()))?;
    let parent = resolve(host, parent, Want::Directory).map_err(|e| e.to_string())?;
    delete_entry(
        host,
        &parent.fd,
        parent.mount.id,
        name.to_owned(),
        PathBuf::from(name),
        out,
    );
    Ok(())
}

fn delete_entry<H: Delete>(
    host: &H,
    dir: &H::Fd,
    mount_id: u64,
    name: OsString,
    path: PathBuf,
    out: &mut Lines<'_>,
) {
    let first = match host.statx(Base::Dir(dir), Path::new(&name)) {
        Ok(stat) => stat,
        Err(e) => return out.err(&cause_tag(&e), &path, e),
    };
    let what = if first.file_type == Some(FileType::Directory) {
        match open_beneath(
            host,
            dir,
            mount_id,
            Path::new(&name),
            Access::Path,
            Want::Directory,
        ) {
            Ok((sub, _)) => match sorted_names(host, &sub) {
                Ok(names) => {
                    for child in names {
                        let child_path = path.join(&child);
                        delete_entry(host, &sub, mount_id, child, child_path, out);
                    }
                }
                Err(e) => out.err(&cause_tag(&e), &path, e),
            },
            Err(e) => return out.resolve_err(&path, &e),
        }
        Unlink::Dir
    } else {
        Unlink::NonDir
    };
    // The identity check right before the unlink (for a file, the first
    // statx was the call right before).
    let checked = if what == Unlink::Dir {
        match host.statx(Base::Dir(dir), Path::new(&name)) {
            Ok(stat) => stat,
            Err(e) => return out.err(&cause_tag(&e), &path, e),
        }
    } else {
        first
    };
    match host.unlinkat(dir, &name, what) {
        Ok(()) => out.ok(format!(
            "unlinked {path:?} ino={}",
            checked
                .ino
                .map_or("unanswered".to_owned(), |i| i.to_string())
        )),
        Err(e) => out.err(&cause_tag(&e), &path, e),
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
        let outcome = run(host, op, &[dir.to_owned()], &mut |l| got.push(l));
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
    fn delete_removes_a_tree_depth_first_with_an_identity_check_before_each_unlink() {
        let model = ModelHost::from_yaml(&MODEL.replace("    mnt: {.: {}}\n", "").replace(
            "  - {id: 31, at: /tmp/t/mnt, fs: tmpfs, dev: \"0:43\", size: 1M, parent: 30}\n",
            "",
        ))
        .unwrap_or_else(|e| panic!("{e}"));
        let (outcome, got) = lines(&model, "delete", "/tmp/t");
        let names: Vec<&str> = got
            .iter()
            .map(|l| l.split(' ').nth(2).unwrap_or(""))
            .collect();
        assert_eq!(
            names,
            [
                "\"t/a",
                "\"t/b\"",
                "\"t/link\"",
                "\"t/sub/c\"",
                "\"t/sub/deeper/d\"",
                "\"t/sub/deeper\"",
                "\"t/sub\"",
                "\"t\""
            ],
            "{got:?}"
        );
        assert!(got.iter().all(|l| l.contains(" unlinked ")), "{got:?}");
        assert_eq!(outcome, Ok(Outcome { failed: 0 }));
        assert!(model.stat_path(Path::new("/tmp/t")).is_none());
        assert!(model.stat_path(Path::new("/tmp/keep")).is_some());
    }

    #[test]
    fn a_failed_entry_is_reported_and_the_rest_goes_on() {
        // The mount below t cannot be entered: it is reported, and t itself
        // is then not empty.
        let (outcome, got) = lines(&model(), "delete", "/tmp/t");
        let errors: Vec<&String> = got.iter().filter(|l| l.contains(" err ")).collect();
        assert_eq!(errors.len(), 2, "{got:?}");
        assert!(
            errors[0].contains("err unsafe_path \"t/mnt\""),
            "{errors:?}"
        );
        assert!(errors[1].contains("err ENOTEMPTY \"t\""), "{errors:?}");
        assert_eq!(outcome, Ok(Outcome { failed: 2 }));
    }

    #[test]
    fn the_command_line_takes_points_then_the_operation() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let parsed = parse(&args(&[
            "--pause-at",
            "unlinkat:2",
            "--kill-at",
            "9",
            "delete",
            "/tmp/x",
        ]))
        .expect("parses");
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
            &[],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?} accepted");
        }
        let model = model();
        assert!(run(&model, "walk", &[], &mut |_| {}).is_err());
        assert!(run(&model, "rm", &["/tmp/t".to_owned()], &mut |_| {}).is_err());
    }
}
