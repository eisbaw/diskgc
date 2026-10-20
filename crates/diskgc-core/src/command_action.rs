//! Read-only executable preflight and argv construction for trusted command
//! actions. Paths are resolved before any cleaner runs; the execution port
//! receives an absolute program path and an already checked directory fd.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::host::{CommandRun, Host, HostError, MonoTime};

/// The first executable and every additional tool a known `env` wrapper
/// would invoke. Nothing here is a cached filesystem verdict: the deleting
/// port still reports an exec failure if the executable changes later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub program: PathBuf,
    pub executables: Vec<PathBuf>,
    /// Position in the original argv of the program invoked by `env`.
    pub env_program_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    Ready(Ready),
    Missing(String),
}

/// Resolve every executable of one command tier before its first action.
/// A bare program uses the host's PATH; a path with a slash is made absolute
/// from diskgc's original cwd, before a child fchdir to its candidate.
/// `env` with assignments is the supported explicit wrapper form: its inner
/// program is resolved against an assigned PATH if there is one.
pub fn preflight<H: Host>(host: &H, argv: &[String]) -> Result<Preflight, HostError> {
    let Some(first) = argv.first() else {
        return Err(HostError::invalid(
            "preflight executable",
            "command",
            "the argv list is empty",
        ));
    };
    let Some(program) = resolve_program(host, first, &host.command_path())? else {
        return Ok(Preflight::Missing(first.clone()));
    };
    let mut executables = vec![program.clone()];
    let mut env_program_index = None;
    if Path::new(first).file_name() == Some(OsStr::new("env")) {
        let mut path = host.command_path();
        for (index, word) in argv.iter().enumerate().skip(1) {
            // GNU env accepts one option terminator before assignments.
            // A later or repeated `--` is ambiguous with its command word;
            // refusing it keeps preflight and execution on the same argv.
            if index == 1 && word == "--" {
                continue;
            }
            if word == "--" {
                return Err(HostError::invalid(
                    "preflight executable",
                    first,
                    "env has an unsupported or repeated `--` terminator",
                ));
            }
            if let Some((key, value)) = word.split_once('=') {
                if key.is_empty() || key.starts_with('-') {
                    return Err(HostError::invalid(
                        "preflight executable",
                        first,
                        format!("unsupported env assignment {word:?}"),
                    ));
                }
                if key == "PATH" {
                    path = OsString::from(value);
                }
                continue;
            }
            if word.starts_with('-') {
                return Err(HostError::invalid(
                    "preflight executable",
                    first,
                    format!("env option {word:?} cannot be preflighted"),
                ));
            }
            match resolve_program(host, word, &path)? {
                Some(inner) => {
                    executables.push(inner);
                    env_program_index = Some(index);
                }
                None => return Ok(Preflight::Missing(word.clone())),
            }
            break;
        }
    }
    Ok(Preflight::Ready(Ready {
        program,
        executables,
        env_program_index,
    }))
}

fn resolve_program<H: Host>(
    host: &H,
    program: &str,
    search_path: &OsStr,
) -> Result<Option<PathBuf>, HostError> {
    if program.is_empty() || program.as_bytes().contains(&0) {
        return Err(HostError::invalid(
            "preflight executable",
            program,
            "the program name is empty or contains NUL",
        ));
    }
    let named = Path::new(program);
    if program.contains('/') {
        let path = if named.is_absolute() {
            named.to_owned()
        } else {
            host.command_origin()?.join(named)
        };
        return host.executable_file(&path).map(|yes| yes.then_some(path));
    }
    for entry in std::env::split_paths(search_path) {
        let root = if entry.is_absolute() {
            entry
        } else {
            host.command_origin()?.join(entry)
        };
        let path = root.join(program);
        if host.executable_file(&path)? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Substitute `{path}` as bytes, preserving non-UTF-8 candidate names and
/// each argv boundary. No shell is involved.
pub fn substitute(word: &str, path: &Path) -> OsString {
    const MARKER: &[u8] = b"{path}";
    let bytes = word.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + path.as_os_str().as_bytes().len());
    let mut at = 0;
    while let Some(offset) = bytes[at..]
        .windows(MARKER.len())
        .position(|window| window == MARKER)
    {
        let end = at + offset;
        out.extend_from_slice(&bytes[at..end]);
        out.extend_from_slice(path.as_os_str().as_bytes());
        at = end + MARKER.len();
    }
    out.extend_from_slice(&bytes[at..]);
    OsString::from_vec(out)
}

/// Per-candidate values added to the preflighted argv.
pub struct Context<'a> {
    pub candidate: &'a Path,
    pub need_bytes: u64,
    pub mount: &'a Path,
    pub profile: &'a str,
    pub label: String,
    pub deadline: MonoTime,
}

/// Build the exact child request once the candidate and current need are
/// known. `Ready` came from environmental preflight, and the caller supplies
/// the open, checked cwd fd separately to `Delete::run_command`.
pub fn request(ready: &Ready, argv: &[String], context: Context<'_>) -> CommandRun {
    CommandRun {
        executable: ready.program.clone(),
        arguments: argv[1..]
            .iter()
            .enumerate()
            .map(|(index, word)| {
                if ready.env_program_index == Some(index + 1) {
                    ready.executables[1].as_os_str().to_owned()
                } else {
                    substitute(word, context.candidate)
                }
            })
            .collect(),
        environment: vec![
            (
                OsString::from("DISKGC_PATH"),
                context.candidate.as_os_str().to_owned(),
            ),
            (
                OsString::from("DISKGC_NEED_BYTES"),
                OsString::from(context.need_bytes.to_string()),
            ),
            (
                OsString::from("DISKGC_MOUNT"),
                context.mount.as_os_str().to_owned(),
            ),
            (
                OsString::from("DISKGC_PROFILE"),
                OsString::from(context.profile),
            ),
        ],
        output_prefix: context.label,
        deadline: context.deadline,
    }
}

#[cfg(test)]
mod tests {
    use super::substitute;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    #[test]
    fn path_substitution_keeps_non_utf8_bytes_and_argument_boundaries() {
        let path = Path::new(OsStr::from_bytes(b"/tmp/a\xff"));
        assert_eq!(
            substitute("x{path}y{path}", path).as_bytes(),
            b"x/tmp/a\xffy/tmp/a\xff"
        );
    }
}
