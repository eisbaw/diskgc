//! Every YAML file in models/profiles is run through the same planner and
//! reclaim contract. Adding a fixture does not require another Rust test.

use std::fs;
use std::path::Path;

use diskgc_core::profile::{Library, parse_profiles};
use diskgc_model::profile_vector::ProfileVector;

fn read(path: &Path) -> ProfileVector {
    let yaml = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    ProfileVector::parse(&yaml).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn every_checked_in_profile_vector_matches_plan_and_reclaim() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models/profiles");
    let mut paths: Vec<_> = fs::read_dir(&directory)
        .expect("profile vector directory")
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no profile vectors in {}",
        directory.display()
    );
    let library = Library::builtin().expect("built-in profiles");
    for path in paths {
        let vector = read(&path);
        vector
            .check(&library)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
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
