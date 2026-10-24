//! The initial-environment heuristic (docs/safety.md, In use): a process
//! whose environment at `exec` names a path inside a candidate (`TMPDIR`
//! of a dormant `nix develop` shell, `CARGO_TARGET_DIR`, `PWD`) uses it.
//!
//! Paths are text in the process's mount namespace, so this is a heuristic
//! by nature: a symlinked or relative path, a path of another mount
//! namespace and a variable changed after `exec` are not recognised.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use super::EnvPath;
use crate::host::Environ;

/// An index of the first environment path inside each possible candidate.
/// A scan can contain thousands of paths, while a large read-only plan can
/// check hundreds of thousands of candidates against the same scan. Keep
/// the source paths in their original order and index their ancestors once.
/// Both the written and lexically resolved forms are indexed, matching
/// [`inside`]'s conservative treatment of `.` and `..`.
#[derive(Debug, Clone, Default)]
pub(super) struct EnvPathIndex(HashMap<PathBuf, usize>);

impl EnvPathIndex {
    pub(super) fn new(paths: &[EnvPath]) -> Self {
        let mut first = HashMap::new();
        for (i, entry) in paths.iter().enumerate() {
            insert_ancestors(&mut first, &entry.path, i);
            // The lexical form differs only where `.` or `..` is a component
            // (`components` already folds `//` and inner `.`), and building
            // it costs an allocation per path: a desktop's scan has ~10^5.
            if entry
                .path
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
            {
                insert_ancestors(&mut first, &lexical(&entry.path), i);
            }
        }
        Self(first)
    }

    pub(super) fn first_inside(&self, candidate: &Path) -> Option<usize> {
        self.0.get(candidate).copied()
    }

    /// A conservative allocation lower bound for diagnostics. HashMap's
    /// control bytes and the spare capacity of path buffers are omitted.
    pub(super) fn minimum_bytes(&self) -> usize {
        self.0.capacity() * std::mem::size_of::<(PathBuf, usize)>()
            + self
                .0
                .keys()
                .map(|path| path.as_os_str().len())
                .sum::<usize>()
    }
}

/// Indexes `path` and every ancestor of it under `i`, unless an earlier
/// path already did. Ancestors come deepest first, and every key's
/// ancestors are keys already (each insertion goes up to the root or to a
/// present key, whose ancestors are present by the same argument), so the
/// walk stops at the first present one: nothing above it would change, and
/// no owned key is made for it.
fn insert_ancestors(first: &mut HashMap<PathBuf, usize>, path: &Path, i: usize) {
    for ancestor in path.ancestors() {
        if first.contains_key(ancestor) {
            return;
        }
        first.insert(ancestor.to_owned(), i);
    }
}

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
    use proptest::prelude::*;

    /// A path of up to six components drawn from a small alphabet, with
    /// `.`, `..` and empty components (`//`), so that paths share
    /// ancestors, repeat, and differ only lexically.
    fn path() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop::sample::select(vec!["a", "b", "c", ".", "..", ""]),
            0..6,
        )
        .prop_map(|parts| format!("/{}", parts.join("/")))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]

        /// TASK-115: the index stops at the first ancestor already present
        /// and builds the lexical form only for `.` and `..`; it must still
        /// answer every candidate as the plain search over every path in
        /// order does.
        #[test]
        fn indexed_lookup_is_the_first_path_inside(
            paths in prop::collection::vec(path(), 0..12),
            candidates in prop::collection::vec(path(), 1..8),
        ) {
            let entries: Vec<EnvPath> = paths
                .iter()
                .enumerate()
                .map(|(i, path)| EnvPath {
                    pid: crate::host::Pid(u32::try_from(i).expect("small")),
                    var: "V".into(),
                    path: path.into(),
                })
                .collect();
            let index = EnvPathIndex::new(&entries);
            for candidate in &candidates {
                let candidate = Path::new(candidate);
                // Candidates are walked paths: absolute, no `.` or `..`.
                if candidate
                    .components()
                    .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
                {
                    continue;
                }
                let expected = entries.iter().position(|e| inside(&e.path, candidate));
                prop_assert_eq!(index.first_inside(candidate), expected, "{}", candidate.display());
            }
        }
    }

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

    #[test]
    fn indexed_lookup_agrees_with_the_written_and_lexical_prefix_rules() {
        let paths = [
            "/tmp/a/../c/file",
            "/tmp/c/../x",
            "/tmp//c/f",
            "/tmp/./d/f",
            "/tmp/candidate-sibling/f",
            "/tmp/x/../../var/tmp/e",
            "/home/u/project/target",
            "/tmp/\u{00e9}/f",
        ];
        let entries: Vec<EnvPath> = paths
            .iter()
            .enumerate()
            .map(|(i, path)| EnvPath {
                pid: crate::host::Pid(i as u32),
                var: "TMPDIR".into(),
                path: (*path).into(),
            })
            .collect();
        let index = EnvPathIndex::new(&entries);
        for candidate in [
            "/",
            "/tmp",
            "/tmp/a",
            "/tmp/c",
            "/tmp/c/..",
            "/tmp/x",
            "/tmp/candidate",
            "/tmp/candidate-sibling",
            "/tmp/d",
            "/var/tmp/e",
            "/home/u/project",
            "/home/u/project/target",
            "/home/u/project/targets",
            "/tmp/\u{00e9}",
            "/missing",
        ] {
            let candidate = Path::new(candidate);
            let expected = entries.iter().position(|e| inside(&e.path, candidate));
            assert_eq!(
                index.first_inside(candidate),
                expected,
                "{}",
                candidate.display()
            );
        }
    }
}
