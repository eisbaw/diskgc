//! `find.paths` patterns (docs/reference.md, Profiles: `find.paths`):
//! relative to a root, one directory level per `/`-separated segment, no
//! `**`. Within a segment, `*` matches any run of bytes (none included,
//! a leading `.` included); every other byte matches itself. `?`, `[`,
//! `]` and `\` are reserved: the loader refuses them, so giving them a
//! meaning later cannot change what an existing pattern matches.
//!
//! Matching is on the bytes of a name (`OsStr`), never on a lossy UTF-8
//! form: a name that is not UTF-8 matches only through `*`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

/// The bytes a pattern may not contain (reserved for a later glob syntax).
pub const RESERVED: &[char] = &['?', '[', ']', '\\'];

/// One `find.paths` pattern, `{uid}` replaced, split into its segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    text: String,
    segments: Vec<Segment>,
}

/// One segment: a literal name, or a glob (the literal pieces between its
/// `*`s, so `a*b*c` is `["a", "b", "c"]` and `*` is `["", ""]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(Vec<u8>),
    Glob(Vec<Vec<u8>>),
}

impl Pattern {
    /// `text` (checked by the loader: relative, no empty, `.`, `..` or
    /// `**` segment, no reserved byte) with `{uid}` replaced by `uid`.
    pub fn compile(text: &str, uid: u32) -> Self {
        let text = text.replace("{uid}", &uid.to_string());
        let segments = text
            .split('/')
            .map(|segment| {
                if segment.contains('*') {
                    Segment::Glob(segment.split('*').map(|p| p.as_bytes().to_vec()).collect())
                } else {
                    Segment::Literal(segment.as_bytes().to_vec())
                }
            })
            .collect();
        Self { text, segments }
    }

    /// The pattern as written, `{uid}` replaced.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// How many directory levels below the root it reaches.
    pub fn depth(&self) -> usize {
        self.segments.len()
    }

    /// The segment at `level` (0: the names directly in the root).
    pub fn segment(&self, level: usize) -> Option<&Segment> {
        self.segments.get(level)
    }

    /// Whether `names` (the path below a root, one name per level) match
    /// the pattern: as many names as segments, each matching its segment.
    pub fn matches_names(&self, names: &[&OsStr]) -> bool {
        names.len() == self.segments.len()
            && self
                .segments
                .iter()
                .zip(names)
                .all(|(segment, name)| segment.matches(name))
    }
}

impl Segment {
    /// Whether the single name `name` matches this segment.
    pub fn matches(&self, name: &OsStr) -> bool {
        let name = name.as_bytes();
        match self {
            Self::Literal(literal) => name == literal.as_slice(),
            Self::Glob(pieces) => glob_matches(pieces, name),
        }
    }

    /// The literal name, when the segment has no `*`: it is looked up
    /// directly, without listing the directory.
    pub fn literal(&self) -> Option<&OsStr> {
        match self {
            Self::Literal(literal) => Some(OsStr::from_bytes(literal)),
            Self::Glob(_) => None,
        }
    }
}

/// `pieces` are the literals around the `*`s (at least two). The first
/// must be a prefix, the last a suffix that does not overlap it, and the
/// middle ones appear in order in between; taking each middle piece at its
/// first occurrence is enough (any later choice leaves less room).
fn glob_matches(pieces: &[Vec<u8>], name: &[u8]) -> bool {
    let (Some(first), Some(last)) = (pieces.first(), pieces.last()) else {
        return false;
    };
    if name.len() < first.len() + last.len() || !name.starts_with(first) || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for piece in &pieces[1..pieces.len() - 1] {
        match find(rest, piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    true
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Whether the absolute `path` is found by `patterns` below one of
/// `roots` (absolute, `{uid}` replaced): what a profile's `examples` claim
/// (docs/reference.md, Profiles: `examples`). With `roots` `None` (`roots:
/// required`), any root will do: the path's last names match a pattern.
/// Paths only: the entry type is not part of an example.
pub fn path_matches(roots: Option<&[&Path]>, patterns: &[Pattern], path: &Path) -> bool {
    let Some(path) = names_of(path) else {
        return false;
    };
    let below = |rel: &[&OsStr]| patterns.iter().any(|p| p.matches_names(rel));
    match roots {
        Some(roots) => roots.iter().any(|root| {
            names_of(root).is_some_and(|root| {
                path.len() > root.len() && path.starts_with(&root) && below(&path[root.len()..])
            })
        }),
        // A root is a directory, so at least one name stays above it.
        None => (1..path.len()).any(|start| below(&path[start..])),
    }
}

/// The names of an absolute, normal path (`None` for anything else).
fn names_of(path: &Path) -> Option<Vec<&OsStr>> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return None;
    }
    components
        .map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect()
}
