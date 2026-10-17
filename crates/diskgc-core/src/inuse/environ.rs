//! The initial-environment heuristic (docs/safety.md, In use): a process
//! whose environment at `exec` names a path inside a candidate (`TMPDIR`
//! of a dormant `nix develop` shell, `CARGO_TARGET_DIR`, `PWD`) uses it.
//!
//! Paths are text in the process's mount namespace, so this is a heuristic
//! by nature: a symlinked or relative path, a path of another mount
//! namespace and a variable changed after `exec` are not recognised.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::host::Environ;

/// The absolute paths named by `env`: every variable's value split at `:`
/// (so `PATH`-like lists count component by component), absolute
/// components only, byte for byte, each with its variable's name.
pub(crate) fn paths(env: &Environ) -> Vec<(OsString, PathBuf)> {
    env.vars
        .iter()
        .flat_map(|(name, value)| {
            value
                .as_bytes()
                .split(|b| *b == b':')
                .filter(|part| part.first() == Some(&b'/'))
                .map(move |part| (name.clone(), PathBuf::from(OsStr::from_bytes(part))))
        })
        .collect()
}

/// Whether `path` is `candidate` or below it, compared by components as
/// written and also with `.` and `..` resolved lexically (a `..` can climb
/// into the candidate: `/tmp/x/../c/f` names `/tmp/c/f`). Lexical
/// resolution can be wrong where a symlink precedes the `..`; checking both
/// forms only adds matches, the conservative side.
pub(crate) fn inside(path: &Path, candidate: &Path) -> bool {
    path.starts_with(candidate) || lexical(path).starts_with(candidate)
}

fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_components_of_every_value_are_paths() {
        let env = Environ {
            vars: vec![
                ("TMPDIR".into(), "/tmp/c".into()),
                ("PATH".into(), "/tmp/c/bin:relative:/usr/bin".into()),
                ("WORD".into(), "hello".into()),
                ("EMPTY".into(), "".into()),
            ],
            without_equals: vec!["/tmp/c".into()],
        };
        let got: Vec<(String, String)> = paths(&env)
            .into_iter()
            .map(|(n, p)| (n.to_string_lossy().into(), p.display().to_string()))
            .collect();
        let want: Vec<(String, String)> = [
            ("TMPDIR", "/tmp/c"),
            ("PATH", "/tmp/c/bin"),
            ("PATH", "/usr/bin"),
        ]
        .iter()
        .map(|(a, b)| ((*a).into(), (*b).into()))
        .collect();
        assert_eq!(got, want);
        // Non-UTF-8 values are kept byte for byte.
        let raw = Environ {
            vars: vec![("X".into(), OsStr::from_bytes(b"/tmp/\xff").into())],
            without_equals: vec![],
        };
        assert_eq!(paths(&raw)[0].1.as_os_str().as_bytes(), b"/tmp/\xff");
    }

    #[test]
    fn inside_means_the_candidate_or_below_by_components() {
        let c = Path::new("/tmp/c");
        for yes in [
            "/tmp/c",
            "/tmp/c/",
            "/tmp/c/sub/f",
            "/tmp//c/f",
            "/tmp/./c/f",
            "/tmp/x/../c/f",
            "/tmp/c/../x",
        ] {
            assert!(inside(Path::new(yes), c), "{yes}");
        }
        for no in ["/tmp/cc", "/tmp", "/tmp/x/c", "/", "/tmp/x/../cc"] {
            assert!(!inside(Path::new(no), c), "{no}");
        }
    }
}
