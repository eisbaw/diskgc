//! Pure profile example checks. The fixture is an in-memory path, its
//! entry type and the names of its direct regular-file and directory children. This
//! uses the same compiled `fs` patterns as discovery; it opens no path.
//!
//! YAML's path-only examples cannot state an entry type, contents or an
//! external source anchor. For
//! them the fixture uses the declared type (a directory for `any`) and
//! includes the source's required `has` and `has_dir` markers. Golden/model fixtures
//! separately pin type and `where` behavior on real layouts.

use std::path::{Path, PathBuf};

use super::{Body, EntryType, Finds, Library, Roots, Source};
use crate::fs_kind::{Find, path_matches_except};

/// One candidate in the example model. `files` are regular files directly
/// inside a directory; none of them are looked up on the host.
pub struct Entry<'a> {
    pub path: &'a Path,
    pub entry_type: EntryType,
    pub files: &'a [&'a str],
    pub dirs: &'a [&'a str],
}

/// Whether an `fs` source would find `entry` by path, type and its own
/// `where`. A required-root source may pin an illustrative `examples.root`;
/// otherwise any absolute ancestor may supply the root.
pub fn matches_entry(source: &Source, uid: u32, entry: &Entry<'_>) -> bool {
    let Some(find) = Find::of(source, uid) else {
        return false;
    };
    if find.entry_type != EntryType::Any && find.entry_type != entry.entry_type {
        return false;
    }
    let roots: Option<Vec<PathBuf>> = match &source.roots {
        Roots::Fixed(roots) => Some(
            roots
                .iter()
                .map(|root| PathBuf::from(root.replace("{uid}", &uid.to_string())))
                .collect(),
        ),
        Roots::Required => source
            .examples
            .root
            .as_ref()
            .map(|root| vec![PathBuf::from(root.replace("{uid}", &uid.to_string()))]),
    };
    let root_refs: Option<Vec<&Path>> = roots
        .as_ref()
        .map(|roots| roots.iter().map(PathBuf::as_path).collect());
    path_matches_except(
        root_refs.as_deref(),
        &find.patterns,
        &find.exclude,
        entry.path,
    ) && find
        .has
        .as_deref()
        .is_none_or(|name| entry.files.contains(&name))
        && find
            .has_dir
            .as_deref()
            .is_none_or(|name| entry.dirs.contains(&name))
        && find
            .has_not
            .as_deref()
            .is_none_or(|name| !entry.files.contains(&name))
        && find
            .lacks_entry
            .as_deref()
            .is_none_or(|name| !entry.files.contains(&name))
}

/// Each violated `match` or `no_match` example, with the owning profile.
/// The abstract uid is stable so `profiles test` gives the same result on
/// every machine, including rootless test containers.
pub fn failures(library: &Library) -> Vec<String> {
    const EXAMPLE_UID: u32 = 1000;
    let mut failures = Vec::new();
    for profile in library.profiles() {
        let Body::Source(source) = &profile.body else {
            continue;
        };
        let Finds::Paths { entry_type, .. } = source.finds else {
            continue;
        };
        let model_type = if entry_type == EntryType::File {
            EntryType::File
        } else {
            EntryType::Dir
        };
        let files: Vec<&str> = source.where_.has.as_deref().into_iter().collect();
        let dirs: Vec<&str> = source.where_.has_dir.as_deref().into_iter().collect();
        for (label, paths, expected) in [
            ("match", &source.examples.matches, true),
            ("no_match", &source.examples.no_match, false),
        ] {
            for path in paths {
                let model_path = PathBuf::from(path.replace("{uid}", &EXAMPLE_UID.to_string()));
                let entry = Entry {
                    path: &model_path,
                    entry_type: model_type,
                    files: &files,
                    dirs: &dirs,
                };
                if matches_entry(source, EXAMPLE_UID, &entry) != expected {
                    failures.push(format!(
                        "{}: {label} example {path} {}",
                        profile.name,
                        if expected {
                            "was not found"
                        } else {
                            "was found"
                        }
                    ));
                }
            }
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Body, Library, parse_profiles};

    #[test]
    fn examples_check_path_type_and_where_in_core() {
        let yaml = "- name: scratch\n  find: {roots: [/tmp], paths: ['scratch-*'], type: dir}\n  where: {has: marker, has_not: keep}\n  action: delete\n  examples: {match: [/tmp/scratch-ok], no_match: [/tmp/other]}\n";
        let library = Library::with_user_profiles(&parse_profiles(yaml).unwrap()).unwrap();
        assert!(failures(&library).is_empty());
        let Body::Source(source) = &library.get("scratch").unwrap().body else {
            panic!("source")
        };
        let entry = |path, entry_type, files| Entry {
            path,
            entry_type,
            files,
            dirs: &[],
        };
        assert!(matches_entry(
            source,
            1000,
            &entry(Path::new("/tmp/scratch-ok"), EntryType::Dir, &["marker"])
        ));
        assert!(!matches_entry(
            source,
            1000,
            &entry(Path::new("/tmp/scratch-ok"), EntryType::File, &["marker"])
        ));
        assert!(!matches_entry(
            source,
            1000,
            &entry(Path::new("/tmp/scratch-ok"), EntryType::Dir, &[])
        ));
        assert!(!matches_entry(
            source,
            1000,
            &entry(
                Path::new("/tmp/scratch-ok"),
                EntryType::Dir,
                &["marker", "keep"]
            )
        ));
        assert!(!matches_entry(
            source,
            1000,
            &entry(Path::new("/tmp/other"), EntryType::Dir, &["marker"])
        ));
    }

    #[test]
    fn a_changed_pattern_makes_a_match_example_fail() {
        let yaml = "- name: scratch\n  find: {roots: [/tmp], paths: ['scratch-*'], type: dir}\n  action: delete\n  examples: {match: [/tmp/scratch-ok], no_match: [/tmp/other]}\n";
        let good = Library::with_user_profiles(&parse_profiles(yaml).unwrap()).unwrap();
        assert!(failures(&good).is_empty());
        let changed = yaml.replace("scratch-*", "scratch-new-*");
        let bad = Library::with_user_profiles(&parse_profiles(&changed).unwrap()).unwrap();
        assert_eq!(
            failures(&bad),
            ["scratch: match example /tmp/scratch-ok was not found"]
        );
    }

    #[test]
    fn required_root_examples_apply_exclusions_at_the_same_depth() {
        let yaml = "- name: scratch\n  find: {roots: required, paths: [a/b], exclude: [b], type: dir}\n  action: delete\n  examples: {match: [/tmp/a/b]}\n";
        let library = Library::with_user_profiles(&parse_profiles(yaml).unwrap()).unwrap();
        assert!(failures(&library).is_empty());
    }

    #[test]
    fn required_root_examples_can_pin_the_depth_they_claim() {
        let yaml = "- name: build\n  find: {roots: required, paths: ['*', '*/*'], type: dir}\n  action: delete\n  examples: {root: /home/mark/src, match: [/home/mark/src/build, /home/mark/src/a/build], no_match: [/home/mark/src/a/b/build]}\n";
        let library = Library::with_user_profiles(&parse_profiles(yaml).unwrap()).unwrap();
        assert!(failures(&library).is_empty(), "{:?}", failures(&library));
        let without_root = yaml.replace("root: /home/mark/src, ", "");
        let library = Library::with_user_profiles(&parse_profiles(&without_root).unwrap()).unwrap();
        assert_eq!(
            failures(&library),
            ["build: no_match example /home/mark/src/a/b/build was found"]
        );
    }
}
