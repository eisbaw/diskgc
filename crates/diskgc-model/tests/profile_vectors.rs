//! The vector corpus (docs/testing.md, Vector corpus): every YAML file
//! below `tests/models/profiles` and `tests/vectors` runs through the one
//! runner, and the corpus as a whole must cover every skip reason and every
//! built-in source. Adding a vector does not require another Rust test.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use diskgc_core::profile::{Action, Body, Library, parse_profiles};
use diskgc_model::profile_vector::{Checked, ProfileVector, corpus_files};

fn read(path: &Path) -> ProfileVector {
    let yaml = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    ProfileVector::parse(&yaml).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The corpus roots. `models/profiles` predates the corpus and is embedded
/// by the container replays (diskgc-e2e), so it stays where it is.
fn corpus_roots() -> Vec<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    vec![
        manifest.join("tests/models/profiles"),
        manifest.join("tests/vectors"),
    ]
}

/// Every vector's result, run once per test binary: `Ok` with what it
/// produced, or the file's failure.
fn corpus() -> &'static [(PathBuf, Result<Checked, String>)] {
    static CORPUS: OnceLock<Vec<(PathBuf, Result<Checked, String>)>> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let roots = corpus_roots();
        let files = corpus_files(&roots).expect("corpus layout");
        // A root that lost its files (a moved directory) fails here; which
        // vectors must exist is the completeness tests' question.
        for root in &roots {
            assert!(
                files.iter().any(|file| file.starts_with(root)),
                "no vectors below {}",
                root.display()
            );
        }
        files
            .into_iter()
            .map(|path| {
                let result = fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|yaml| ProfileVector::parse(&yaml))
                    .and_then(|vector| vector.check_builtin());
                (path, result)
            })
            .collect()
    })
}

/// Every failing file is named, not only the first.
#[test]
fn every_vector_in_the_corpus_passes() {
    let failures: Vec<String> = corpus()
        .iter()
        .filter_map(|(path, result)| {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"));
            let name = path.strip_prefix(root).unwrap_or(path).display();
            result.as_ref().err().map(|e| format!("{name}: {e}"))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} vectors failed:\n\n{}",
        failures.len(),
        corpus().len(),
        failures.join("\n\n")
    );
}

#[test]
fn every_modelable_builtin_source_has_a_checked_in_vector() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models/profiles");
    let library = Library::builtin().expect("built-in profiles");
    let mut covered = BTreeSet::new();
    for path in fs::read_dir(&directory).expect("profile vector directory") {
        let path = path.expect("directory entry").path();
        if path.extension().is_some_and(|ext| ext == "yaml") {
            let vector = read(&path);
            if !vector.user_profiles.is_empty() {
                continue;
            }
            let name = vector
                .profile_name()
                .expect("a models/profiles vector names its profile");
            let profile = library.get(name).expect("shipped profile");
            assert!(
                matches!(&profile.body, Body::Source(source) if !matches!(source.action, Action::Command { .. })),
                "{} cannot assert a command cleaner's filesystem effect in ModelHost",
                path.display()
            );
            covered.insert(name.to_owned());
        }
    }
    let modelable: BTreeSet<_> = library
        .profiles()
        .filter_map(|profile| match &profile.body {
            Body::Source(source)
                if matches!(
                    source.action,
                    Action::Delete { .. } | Action::CargoLockProtocol
                ) =>
            {
                Some(profile.name.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(covered, modelable, "built-in source vector coverage drift");
}

#[test]
fn wrong_python_matcher_is_rejected_by_the_checked_in_vector() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/models/profiles/python-pycache-checkout.yaml");
    let vector = read(&path);
    let library = Library::builtin().expect("built-in profiles");
    vector.check(&library).expect("unmodified matcher passes");

    // A user override takes the same path as the product's profile loading.
    // Mutate only the distinguishing path segment; the unchanged host and
    // expected plan must reject a matcher that misses __pycache__.
    let builtin_yaml = include_str!("../../diskgc-core/src/profile/builtin.yaml");
    let mut source = parse_profiles(builtin_yaml)
        .expect("built-in profile YAML")
        .into_iter()
        .find(|spec| spec.name == "python-pycache")
        .expect("python-pycache source");
    source.r#override = Some(true);
    source.verified_with = None;
    let find = source.find.as_mut().expect("find");
    for pattern in find.paths.as_mut().expect("paths") {
        *pattern = pattern.replace("__pycache__", "__wrong_cache__");
    }
    let mutant = Library::with_user_profiles(&[source]).expect("valid mutant override");
    let error = vector.check(&mutant).expect_err("wrong matcher must fail");
    assert!(
        error.contains("dry-run plan"),
        "unexpected failure: {error}"
    );
}

#[test]
fn expected_filesystem_postconditions_must_exist_before_reclaim() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/models/profiles/python-pycache-checkout.yaml");
    let library = Library::builtin().expect("built-in profiles");

    let mut removed = read(&path);
    let action = removed.expect.actions.first().expect("vector action");
    removed.expect.removed.push(action.join("never-existed"));
    let error = removed
        .check(&library)
        .expect_err("an absent path cannot count as removed");
    assert!(error.contains("was absent before reclaim"), "{error}");

    let mut preserved = read(&path);
    preserved
        .expect
        .preserved
        .push("/home/tester/never-existed".into());
    let error = preserved
        .check(&library)
        .expect_err("an absent path cannot count as preserved");
    assert!(error.contains("was absent before reclaim"), "{error}");
}
