//! The strace record: the second, independent view of the deletion oracle
//! (docs/testing.md, Grounding, acceptance signal 1).
//!
//! The code under test runs under
//! `strace -f -ff -qq -y -xx -e trace=unlink,unlinkat,rmdir,rename,renameat,renameat2`.
//! `-ff` writes one file per process, so no line is ever split into
//! "unfinished"/"resumed" halves; `-y` annotates every fd argument (including
//! `AT_FDCWD`) with the path it refers to; `-xx` hex-escapes every string and
//! path, so parsing never has to guess about quotes or odd bytes. Deletions
//! done inside daemons (Docker, Nix) are invisible here and covered by the
//! filesystem snapshot only.

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

/// Parses one line of an strace `-ff` output file of process `pid`.
/// Returns `Ok(None)` for lines that are not traced calls (signals, exits).
pub fn parse_line(pid: u32, line: &str) -> Result<Option<Event>, String> {
    let line = line.trim_end();
    if line.is_empty() || line.starts_with("---") || line.starts_with("+++") {
        return Ok(None);
    }
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
    Ok(Some(Event {
        pid,
        syscall: syscall.to_owned(),
        removed,
        replaced,
        outcome,
    }))
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
pub fn read_record(dir: &Path, prefix: &str) -> Result<Vec<Event>, String> {
    let mut events = Vec::new();
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
        for line in text.lines() {
            if let Some(event) = parse_line(pid, line)? {
                events.push(event);
            }
        }
        files += 1;
    }
    if files == 0 {
        return Err(format!(
            "no strace output files {prefix}.<pid> in {}: was the code under test traced?",
            dir.display()
        ));
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let ev = parse_line(7, &line).unwrap().unwrap();
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
        let ev = parse_line(1, &line).unwrap().unwrap();
        assert_eq!(ev.removed, vec![PathBuf::from("/tmp/a b/sub\"dir")]);
    }

    #[test]
    fn failed_calls_did_not_take_effect() {
        let line = format!(
            "unlinkat(AT_FDCWD<{}>, \"{}\", 0) = -1 ENOENT (No such file or directory)",
            hex("/"),
            hex("/nope")
        );
        let ev = parse_line(1, &line).unwrap().unwrap();
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
        let ev = parse_line(1, &line).unwrap().unwrap();
        assert_eq!(ev.removed, vec![PathBuf::from("/tmp/x.new")]);
        assert_eq!(ev.replaced, vec![PathBuf::from("/tmp/x")]);

        let plain = format!("rename(\"{}\", \"{}\") = 0", hex("/a"), hex("/b"));
        let ev = parse_line(1, &plain).unwrap().unwrap();
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
        let ev = parse_line(1, &line).unwrap().unwrap();
        assert!(ev.removed.is_empty());
        assert_eq!(
            ev.replaced,
            vec![PathBuf::from("/t/a"), PathBuf::from("/t/b")]
        );
    }

    #[test]
    fn killed_mid_call_counts_as_possibly_effective() {
        let line = format!("unlink(\"{}\") = ?", hex("/tmp/x"));
        let ev = parse_line(1, &line).unwrap().unwrap();
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
        let ev = parse_line(1, &line).unwrap().unwrap();
        assert_eq!(ev.removed, vec![PathBuf::from("/tmp/cand/a")]);
        assert_eq!(ev.outcome, Outcome::Ok);
    }

    #[test]
    fn signal_and_exit_lines_are_not_events() {
        assert_eq!(
            parse_line(1, "--- SIGCHLD {si_signo=SIGCHLD} ---").unwrap(),
            None
        );
        assert_eq!(parse_line(1, "+++ exited with 0 +++").unwrap(), None);
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
        let ev = parse_line(1, "unlinkat(4, \"\\x66\", 0) = 0")
            .unwrap()
            .unwrap();
        assert_eq!(ev.removed, vec![PathBuf::from("<fd 4>/f")]);
        assert!(ev.took_effect());
    }
}
