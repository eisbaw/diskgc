//! The strace record: the second, independent view of the deletion oracle
//! (docs/testing.md, Grounding, acceptance signal 1).
//!
//! The code under test runs under
//! `strace -f -ff -qq -y -xx -e trace=unlink,unlinkat,rmdir,rename,renameat,renameat2`.
//! `-ff` writes one file per process, so calls of different processes never
//! interleave; `-y` annotates every fd argument (including
//! `AT_FDCWD`) with the path it refers to; `-xx` hex-escapes every string and
//! path, so parsing never has to guess about quotes or odd bytes. Deletions
//! done inside daemons (Docker, Nix) are invisible here and covered by the
//! filesystem snapshot only.
//!
//! Besides complete calls (`NAME(ARGS) = RET`), a file can hold (strace.c,
//! `printleader` and `droptcb`):
//! - `--- SIG... ---`, `+++ exited with N +++`, `+++ killed by SIG... +++`,
//!   `+++ superseded by execve +++`: not calls;
//! - `NAME(ARGS <unfinished ...>` and later `<... NAME resumed>) = RET`: one
//!   call cut in two when something was printed for the same process in
//!   between (a signal, or a kill after the entry). [`parse_file`] joins
//!   them; a call never resumed may have taken effect (`= ?`);
//! - `NAME(ARGS <detached ...>`: strace detached the process inside the
//!   call (strace ending under load); the call can still complete after the
//!   detach, so it counts as `= ?`, with its paths;
//! - `???( <detached ...>` (any line naming `???`): a call strace could not
//!   name, typically a process dying as strace reads its registers. No name,
//!   no arguments: nothing to attribute to a path. Kept as [`Unnamed`] and
//!   logged; the snapshot view judges every object whatever made the call.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// The syscalls the record covers (the list in docs/testing.md).
pub const TRACED_SYSCALLS: &str = "unlink,unlinkat,rmdir,rename,renameat,renameat2";

/// Starts the path of a call relative to a directory fd that strace printed
/// without its path: `-y` names an fd by reading its `/proc` link, which
/// fails for a path longer than a page (a tree deeper than `PATH_MAX`). The
/// path is then `<fd N>/name`, which no snapshot path can match; the oracle
/// classifies it ([`crate::oracle::Unmatched::UnnamedDirFd`]) and the
/// snapshot view still judges the object.
pub const UNNAMED_FD_PREFIX: &str = "<fd ";

/// How a traced call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// Failed with this errno name (e.g. `ENOENT`).
    Failed(String),
    /// The process died inside the call (`= ?`); it may have taken effect.
    Unknown,
}

/// One traced call, reduced to its effect on paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub pid: u32,
    pub syscall: String,
    /// Paths whose object left that path (unlink and rmdir targets, rename
    /// sources).
    pub removed: Vec<PathBuf>,
    /// Paths whose previous object, if any, was replaced (rename targets).
    pub replaced: Vec<PathBuf>,
    pub outcome: Outcome,
}

impl Event {
    /// Whether the call may have changed the filesystem.
    pub fn took_effect(&self) -> bool {
        matches!(self.outcome, Outcome::Ok | Outcome::Unknown)
    }
}

/// An argument of a traced call as strace printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Arg {
    /// A `-xx` string: `"\x2f\x74"`.
    Str(Vec<u8>),
    /// An fd with its `-y` path: `3<\x2f...>` or `AT_FDCWD<\x2f...>`.
    Fd(Vec<u8>),
    /// Anything else (flags, numbers, fds without a path).
    Other(String),
}

/// A call strace printed as `???`: no name, no arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unnamed {
    pub pid: u32,
    pub line: String,
}

/// What one line of an strace `-ff` file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A complete traced call, or one cut off by a detach (`= ?`).
    Call(Event),
    /// `NAME(ARGS <unfinished ...>`: the entry half (`NAME(ARGS`).
    Unfinished { syscall: String, entry: String },
    /// `<... NAME resumed>REST`: the exit half (`REST`, e.g. `) = 0`).
    Resumed { syscall: String, rest: String },
    /// `???(...`: a call strace could not name.
    Unnamed(Unnamed),
    /// A signal, an exit, a kill, an exec, or an empty line.
    NotACall,
}

const DETACHED: &str = " <detached ...>";
const UNFINISHED: &str = " <unfinished ...>";

/// Classifies one line of an strace `-ff` output file of process `pid`.
/// Anything it does not recognise is an error, never dropped.
pub fn parse_line(pid: u32, line: &str) -> Result<Line, String> {
    let line = line.trim_end();
    if line.is_empty() || line.starts_with("---") || line.starts_with("+++") {
        return Ok(Line::NotACall);
    }
    if line.starts_with("???(") {
        return Ok(Line::Unnamed(Unnamed {
            pid,
            line: line.to_owned(),
        }));
    }
    if let Some(head) = line.strip_suffix(DETACHED) {
        // Detached inside the call: it may complete after the detach.
        return call(pid, &format!("{head}) = ?")).map(Line::Call);
    }
    if let Some(entry) = line.strip_suffix(UNFINISHED) {
        let open = entry
            .find('(')
            .ok_or_else(|| format!("strace line without '(': {line}"))?;
        return Ok(Line::Unfinished {
            syscall: entry[..open].to_owned(),
            entry: entry.to_owned(),
        });
    }
    if let Some(resumed) = line.strip_prefix("<... ") {
        let (syscall, rest) = resumed
            .split_once(" resumed>")
            .ok_or_else(|| format!("strace resumed line without ' resumed>': {line}"))?;
        return Ok(Line::Resumed {
            syscall: syscall.to_owned(),
            rest: rest.to_owned(),
        });
    }
    call(pid, line).map(Line::Call)
}

/// Every call in the text of one strace `-ff` file of process `pid`:
/// complete calls, joined unfinished/resumed halves, and calls cut off
/// (unfinished and never resumed, or detached) as `= ?`.
pub fn parse_file(pid: u32, text: &str) -> Result<Record, String> {
    let mut record = Record::default();
    let mut pending: Option<(String, String)> = None;
    // A call left unfinished may have taken effect.
    let cut_off = |record: &mut Record, entry: String| -> Result<(), String> {
        record.events.push(call(pid, &format!("{entry}) = ?"))?);
        Ok(())
    };
    for line in text.lines() {
        match parse_line(pid, line)? {
            Line::NotACall => {}
            Line::Unnamed(unnamed) => record.unnamed.push(unnamed),
            Line::Call(event) => {
                if let Some((_, entry)) = pending.take() {
                    cut_off(&mut record, entry)?;
                }
                record.events.push(event);
            }
            Line::Unfinished { syscall, entry } => {
                if let Some((_, entry)) = pending.replace((syscall, entry)) {
                    cut_off(&mut record, entry)?;
                }
            }
            Line::Resumed { syscall, rest } => match pending.take() {
                Some((name, entry)) if name == syscall => {
                    record.events.push(call(pid, &format!("{entry}{rest}"))?);
                }
                other => {
                    return Err(format!(
                        "strace line resumes {syscall:?}, but the unfinished call is \
                         {:?}: {line}",
                        other.map(|(name, _)| name)
                    ));
                }
            },
        }
    }
    if let Some((_, entry)) = pending {
        cut_off(&mut record, entry)?;
    }
    Ok(record)
}

/// The calls of every process of one traced run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    pub events: Vec<Event>,
    pub unnamed: Vec<Unnamed>,
}

/// A complete call line: `NAME(ARGS) = RET`.
fn call(pid: u32, line: &str) -> Result<Event, String> {
    let open = line
        .find('(')
        .ok_or_else(|| format!("strace line without '(': {line}"))?;
    let syscall = &line[..open];
    // With -xx no string or path holds a raw '=', so the first one separates
    // the call from its return value; strace pads the call with spaces to
    // align return values.
    let eq = line
        .find('=')
        .ok_or_else(|| format!("strace line without '=': {line}"))?;
    let call = line[..eq]
        .trim_end()
        .strip_suffix(')')
        .filter(|call| call.len() > open)
        .ok_or_else(|| format!("strace line without ') =': {line}"))?;
    let args = split_args(&call[open + 1..])?;
    let ret = line[eq + 1..].trim_start();
    let outcome = if ret.starts_with('0') {
        Outcome::Ok
    } else if ret.starts_with('?') {
        Outcome::Unknown
    } else if let Some(rest) = ret.strip_prefix("-1 ") {
        Outcome::Failed(rest.split(' ').next().unwrap_or("").to_owned())
    } else {
        return Err(format!("unexpected return value in strace line: {line}"));
    };

    let path_at = |dir: usize, name: usize| -> Result<PathBuf, String> {
        resolve(args.get(dir), args.get(name)).map_err(|e| format!("{e} in strace line: {line}"))
    };
    let path = |name: usize| -> Result<PathBuf, String> {
        resolve(None, args.get(name)).map_err(|e| format!("{e} in strace line: {line}"))
    };
    let (removed, replaced) = match syscall {
        "unlink" | "rmdir" => (vec![path(0)?], vec![]),
        "unlinkat" => (vec![path_at(0, 1)?], vec![]),
        "rename" => (vec![path(0)?], vec![path(1)?]),
        "renameat" => (vec![path_at(0, 1)?], vec![path_at(2, 3)?]),
        "renameat2" => {
            let (from, to) = (path_at(0, 1)?, path_at(2, 3)?);
            let exchange =
                matches!(args.get(4), Some(Arg::Other(f)) if f.contains("RENAME_EXCHANGE"));
            if exchange {
                // Both objects stay but swap paths: both paths get a new object.
                (vec![], vec![from, to])
            } else {
                (vec![from], vec![to])
            }
        }
        other => return Err(format!("untraced syscall {other:?} in strace line: {line}")),
    };
    Ok(Event {
        pid,
        syscall: syscall.to_owned(),
        removed,
        replaced,
        outcome,
    })
}

/// Joins a path argument with its directory fd's `-y` path when relative.
fn resolve(dir: Option<&Arg>, name: Option<&Arg>) -> Result<PathBuf, String> {
    let Some(Arg::Str(name)) = name else {
        return Err("missing path argument".to_owned());
    };
    let name = PathBuf::from(OsString::from_vec(name.clone()));
    if name.is_absolute() {
        return Ok(name);
    }
    match dir {
        None => Err(format!(
            "relative path {} without a directory fd (trace with -y)",
            name.display()
        )),
        Some(Arg::Fd(dir)) => Ok(Path::new(&OsString::from_vec(dir.clone())).join(name)),
        Some(Arg::Other(fd)) if !fd.is_empty() && fd.bytes().all(|b| b.is_ascii_digit()) => {
            Ok(Path::new(&format!("{UNNAMED_FD_PREFIX}{fd}>")).join(name))
        }
        Some(other) => Err(format!(
            "relative path {} against an fd without a path: {other:?}",
            name.display()
        )),
    }
}

/// Splits strace's argument list at top-level commas.
fn split_args(text: &str) -> Result<Vec<Arg>, String> {
    let mut args = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let (arg, after) = if let Some(body) = rest.strip_prefix('"') {
            let end = body
                .find('"')
                .ok_or_else(|| format!("unterminated string in {text:?}"))?;
            (Arg::Str(decode_hex(&body[..end])?), &body[end + 1..])
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let token = rest[..end].trim();
            let arg = match token.find('<') {
                Some(lt) if token.ends_with('>') => {
                    Arg::Fd(decode_hex(&token[lt + 1..token.len() - 1])?)
                }
                _ => Arg::Other(token.to_owned()),
            };
            (arg, &rest[end..])
        };
        args.push(arg);
        rest = after.trim_start();
        if let Some(after_comma) = rest.strip_prefix(',') {
            rest = after_comma.trim_start();
        } else if !rest.is_empty() {
            return Err(format!("expected ',' in strace arguments {text:?}"));
        }
    }
    Ok(args)
}

/// Decodes `-xx` output: every byte as `\xHH`.
fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4);
    let mut rest = text;
    while !rest.is_empty() {
        let hex = rest
            .strip_prefix("\\x")
            .and_then(|r| r.get(..2))
            .ok_or_else(|| format!("expected \\xHH escapes (strace -xx) in {text:?}"))?;
        out.push(u8::from_str_radix(hex, 16).map_err(|_| format!("bad hex escape in {text:?}"))?);
        rest = &rest[4..];
    }
    Ok(out)
}

/// Reads every per-process file `<prefix>.<pid>` that strace `-ff -o prefix`
/// wrote into `dir`.
pub fn read_record(dir: &Path, prefix: &str) -> Result<Record, String> {
    let mut record = Record::default();
    let mut files = 0;
    let listing = std::fs::read_dir(dir).map_err(|e| format!("list {}: {e}", dir.display()))?;
    for entry in listing {
        let entry = entry.map_err(|e| format!("list {}: {e}", dir.display()))?;
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(prefix))
            .and_then(|n| n.strip_prefix('.'))
        else {
            continue;
        };
        let pid: u32 = pid
            .parse()
            .map_err(|_| format!("unexpected strace output file {name:?}"))?;
        let text = std::fs::read_to_string(entry.path())
            .map_err(|e| format!("read {}: {e}", entry.path().display()))?;
        let file =
            parse_file(pid, &text).map_err(|e| format!("{}: {e}", entry.path().display()))?;
        record.events.extend(file.events);
        record.unnamed.extend(file.unnamed);
        files += 1;
    }
    if files == 0 {
        return Err(format!(
            "no strace output files {prefix}.<pid> in {}: was the code under test traced?",
            dir.display()
        ));
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The call on `line`, which must be one.
    fn event(line: &str) -> Event {
        match parse_line(1, line).unwrap() {
            Line::Call(event) => event,
            other => panic!("not a call: {other:?}"),
        }
    }

    fn hex(s: &str) -> String {
        s.bytes().map(|b| format!("\\x{b:02x}")).collect()
    }

    #[test]
    fn unlinkat_relative_to_cwd() {
        let line = format!(
            "unlinkat(AT_FDCWD<{}>, \"{}\", 0) = 0",
            hex("/tmp/work"),
            hex("d/a")
        );
        let Line::Call(ev) = parse_line(7, &line).unwrap() else {
            panic!("not a call");
        };
        assert_eq!(ev.removed, vec![PathBuf::from("/tmp/work/d/a")]);
        assert!(ev.replaced.is_empty());
        assert_eq!(ev.outcome, Outcome::Ok);
        assert_eq!(ev.pid, 7);
    }

    #[test]
    fn rmdir_via_unlinkat_with_dir_fd_and_flags() {
        let line = format!(
            "unlinkat(5<{}>, \"{}\", AT_REMOVEDIR) = 0",
            hex("/tmp/a b"),
            hex("sub\"dir")
        );
        let ev = event(&line);
        assert_eq!(ev.removed, vec![PathBuf::from("/tmp/a b/sub\"dir")]);
    }

    #[test]
    fn failed_calls_did_not_take_effect() {
        let line = format!(
            "unlinkat(AT_FDCWD<{}>, \"{}\", 0) = -1 ENOENT (No such file or directory)",
            hex("/"),
            hex("/nope")
        );
        let ev = event(&line);
        assert_eq!(ev.outcome, Outcome::Failed("ENOENT".into()));
        assert!(!ev.took_effect());
        assert_eq!(ev.removed, vec![PathBuf::from("/nope")]);
    }

    #[test]
    fn renames_remove_the_source_and_replace_the_target() {
        let line = format!(
            "renameat(AT_FDCWD<{c}>, \"{}\", AT_FDCWD<{c}>, \"{}\") = 0",
            hex("x.new"),
            hex("x"),
            c = hex("/tmp")
        );
        let ev = event(&line);
        assert_eq!(ev.removed, vec![PathBuf::from("/tmp/x.new")]);
        assert_eq!(ev.replaced, vec![PathBuf::from("/tmp/x")]);

        let plain = format!("rename(\"{}\", \"{}\") = 0", hex("/a"), hex("/b"));
        let ev = event(&plain);
        assert_eq!(ev.removed, vec![PathBuf::from("/a")]);
        assert_eq!(ev.replaced, vec![PathBuf::from("/b")]);
    }

    #[test]
    fn renameat2_exchange_replaces_both_paths() {
        let line = format!(
            "renameat2(AT_FDCWD<{c}>, \"{}\", AT_FDCWD<{c}>, \"{}\", RENAME_EXCHANGE) = 0",
            hex("a"),
            hex("b"),
            c = hex("/t")
        );
        let ev = event(&line);
        assert!(ev.removed.is_empty());
        assert_eq!(
            ev.replaced,
            vec![PathBuf::from("/t/a"), PathBuf::from("/t/b")]
        );
    }

    #[test]
    fn killed_mid_call_counts_as_possibly_effective() {
        let line = format!("unlink(\"{}\") = ?", hex("/tmp/x"));
        let ev = event(&line);
        assert_eq!(ev.outcome, Outcome::Unknown);
        assert!(ev.took_effect());
    }

    #[test]
    fn return_column_padding_is_accepted() {
        let line = format!(
            "unlinkat(3<{}>, \"{}\", 0)                  = 0",
            hex("/tmp/cand"),
            hex("a")
        );
        let ev = event(&line);
        assert_eq!(ev.removed, vec![PathBuf::from("/tmp/cand/a")]);
        assert_eq!(ev.outcome, Outcome::Ok);
    }

    #[test]
    fn signal_and_exit_lines_are_not_events() {
        for line in [
            "--- SIGCHLD {si_signo=SIGCHLD, si_code=CLD_EXITED} ---",
            "+++ exited with 0 +++",
            "+++ killed by SIGKILL +++",
            "+++ killed by SIGKILL (core dumped) +++",
            "+++ superseded by execve +++",
            "",
        ] {
            assert_eq!(parse_line(1, line).unwrap(), Line::NotACall, "{line}");
        }
    }

    /// The QA flake (cargo_traced_build_within_bound under load): a call
    /// strace could not name, cut off by a detach. Kept, not a removal.
    #[test]
    fn an_unnamed_detached_call_is_kept_apart_not_dropped() {
        let line = "???( <detached ...>";
        assert_eq!(
            parse_line(9, line).unwrap(),
            Line::Unnamed(Unnamed {
                pid: 9,
                line: line.to_owned()
            })
        );
        let record = parse_file(9, &format!("{line}\n+++ killed by SIGKILL +++\n")).unwrap();
        assert!(record.events.is_empty(), "{record:?}");
        assert_eq!(record.unnamed.len(), 1);
    }

    /// A named call detached mid-call may complete after the detach: it
    /// counts like `= ?`, with its paths.
    #[test]
    fn a_named_call_detached_mid_call_is_possibly_effective() {
        for (line, removed, replaced) in [
            (
                format!(
                    "unlinkat(AT_FDCWD<{}>, \"{}\", 0 <detached ...>",
                    hex("/t"),
                    hex("a")
                ),
                vec!["/t/a"],
                vec![],
            ),
            (
                format!("unlink(\"{}\" <detached ...>", hex("/t/b")),
                vec!["/t/b"],
                vec![],
            ),
            (
                format!(
                    "renameat2(AT_FDCWD<{0}>, \"{1}\", AT_FDCWD<{0}>, \"{2}\", 0 <detached ...>",
                    hex("/t"),
                    hex("s"),
                    hex("d")
                ),
                vec!["/t/s"],
                vec!["/t/d"],
            ),
        ] {
            let ev = event(&line);
            assert_eq!(ev.outcome, Outcome::Unknown, "{line}");
            assert!(ev.took_effect(), "{line}");
            assert_eq!(
                ev.removed,
                removed.iter().map(PathBuf::from).collect::<Vec<_>>()
            );
            assert_eq!(
                ev.replaced,
                replaced.iter().map(PathBuf::from).collect::<Vec<_>>()
            );
        }
        // Cut off before strace could print the path: an error, not a guess.
        assert!(parse_line(1, "unlinkat(AT_FDCWD<\\x2f>, <detached ...>").is_err());
    }

    /// `<unfinished ...>` and `<... NAME resumed>` in one -ff file are one
    /// call; a signal may come between them.
    #[test]
    fn unfinished_and_resumed_halves_are_joined() {
        let text = format!(
            "unlinkat(AT_FDCWD<{}>, \"{}\", 0 <unfinished ...>\n\
             --- SIGCHLD {{si_signo=SIGCHLD}} ---\n\
             <... unlinkat resumed>) = 0\n\
             unlink(\"{}\" <unfinished ...>\n\
             <... unlink resumed>)   = -1 ENOENT (No such file or directory)\n",
            hex("/t"),
            hex("a"),
            hex("/t/b")
        );
        let record = parse_file(1, &text).unwrap();
        let summary: Vec<(String, Outcome)> = record
            .events
            .iter()
            .map(|e| (e.removed[0].display().to_string(), e.outcome.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("/t/a".to_owned(), Outcome::Ok),
                ("/t/b".to_owned(), Outcome::Failed("ENOENT".into())),
            ]
        );
        // A resumed half that does not match the unfinished call is an error.
        let text = format!(
            "unlink(\"{}\" <unfinished ...>\n<... rmdir resumed>) = 0\n",
            hex("/t/b")
        );
        assert!(parse_file(1, &text).is_err());
        assert!(parse_file(1, "<... unlink resumed>) = 0\n").is_err());
    }

    /// An unfinished call never resumed (killed, detached, or followed by
    /// another call) may have taken effect.
    #[test]
    fn an_unfinished_call_never_resumed_is_possibly_effective() {
        let a = format!("unlink(\"{}\" <unfinished ...>", hex("/t/a"));
        let b = format!("rmdir(\"{}\") = 0", hex("/t/d"));
        for (text, outcomes) in [
            (
                format!("{a}\n+++ killed by SIGKILL +++\n"),
                vec![Outcome::Unknown],
            ),
            (format!("{a}\n"), vec![Outcome::Unknown]),
            (format!("{a}\n{b}\n"), vec![Outcome::Unknown, Outcome::Ok]),
        ] {
            let record = parse_file(1, &text).unwrap();
            let got: Vec<Outcome> = record.events.iter().map(|e| e.outcome.clone()).collect();
            assert_eq!(got, outcomes, "{text}");
            assert_eq!(record.events[0].removed, vec![PathBuf::from("/t/a")]);
        }
    }

    #[test]
    fn unparseable_lines_are_errors_not_silently_dropped() {
        assert!(parse_line(1, "unlink(\"plain\") = 0").is_err());
        assert!(parse_line(1, "unlinkat(AT_FDCWD, \"\\x61\", 0) = 0").is_err());
        assert!(parse_line(1, "unlinkat(F, \"\\x61\", 0) = 0").is_err());
        assert!(parse_line(1, "openat(AT_FDCWD, \"\\x61\", 0) = 3").is_err());
    }

    /// strace prints a bare fd when it cannot read the fd's path (longer
    /// than a page): kept, marked, never dropped.
    #[test]
    fn a_dir_fd_strace_could_not_name_is_marked() {
        let ev = event("unlinkat(4, \"\\x66\", 0) = 0");
        assert_eq!(ev.removed, vec![PathBuf::from("<fd 4>/f")]);
        assert!(ev.took_effect());
    }
}
