//! The vector corpus (docs/testing.md, Vector corpus): every YAML file
//! below `tests/models/profiles` and `tests/vectors` runs through the one
//! runner, and the corpus as a whole must cover every skip reason and every
//! built-in source. Adding a vector does not require another Rust test.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use diskgc_core::engine::Reason;
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

/// Every failing file is named, not only the first. `VECTOR_TRACE=<part
/// of a file name>` prints the matching vectors' reclaim call traces.
#[test]
fn every_vector_in_the_corpus_passes() {
    if let Ok(wanted) = std::env::var("VECTOR_TRACE") {
        for (path, _) in corpus() {
            if path.to_string_lossy().contains(&wanted) {
                let trace = read(path).reclaim_trace().unwrap_or_else(|e| vec![e]);
                eprintln!("{}:\n  {}", path.display(), trace.join("\n  "));
            }
        }
    }
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

/// The skip reasons of the JSON contract (docs/reference.md, Output): the
/// one `json` block's `skipped[0].reason` alternatives.
fn documented_skip_reasons() -> BTreeSet<String> {
    let reference = include_str!("../../../docs/reference.md");
    let output = &reference[reference.find("\n## Output").expect("Output section")..];
    let start = output.find("```json\n").expect("json block") + "```json\n".len();
    let end = start + output[start..].find("\n```").expect("end of json block");
    let doc: serde_json::Value = serde_json::from_str(&output[start..end]).expect("JSON parses");
    doc["skipped"][0]["reason"]
        .as_str()
        .expect("reason enum")
        .split('|')
        .map(|reason| reason.trim().to_owned())
        .collect()
}

/// Skip reasons no vector can show, each with why and where it is covered
/// instead. A reason listed here that a vector does show fails the test:
/// the exemption is then stale.
const REASONS_WITHOUT_VECTOR: [(&str, &str); 4] = [
    (
        "too_many_entries",
        "the budget is the product constant DEFAULT_MAX_ENTRIES (2,000,000) and ModelHost \
         lookups are linear; engine.rs more_entries_than_the_budget_is_too_many_entries and \
         cargo_reclaim.rs cover it with a test budget",
    ),
    (
        "unsupported_store",
        "documented, but no code path produces it (TASK-127)",
    ),
    (
        "has_not",
        "documented, but no code path produces it (TASK-127)",
    ),
    (
        "stale",
        "an index reason (docs/architecture.md); no index exists yet (TASK-127)",
    ),
];

/// AC 2: every skip reason the code can report (`Reason::ALL`) and every
/// one the JSON contract documents is shown by at least one vector, as a
/// dry-run plan reason or a reclaim skip that matched its expectation.
#[test]
fn every_skip_reason_has_a_vector() {
    let mut shown = BTreeSet::new();
    for (_, result) in corpus() {
        if let Ok(checked) = result {
            shown.extend(checked.dry_run.plan.iter().filter_map(|row| row.reason));
            shown.extend(checked.reclaim.skipped.iter().map(|row| row.reason));
        }
    }
    let mut required = documented_skip_reasons();
    required.extend(Reason::ALL.iter().map(|reason| reason.as_str().to_owned()));
    let exempt: BTreeSet<&str> = REASONS_WITHOUT_VECTOR.iter().map(|(r, _)| *r).collect();
    let stale: Vec<_> = exempt.iter().filter(|r| shown.contains(*r)).collect();
    assert!(stale.is_empty(), "exempt but shown by a vector: {stale:?}");
    let missing: Vec<_> = required
        .iter()
        .filter(|reason| !shown.contains(reason.as_str()) && !exempt.contains(reason.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "skip reasons without a vector: {missing:?}"
    );
}

/// What a built-in source's vectors must show (AC 3), by the result: an
/// action of that profile; a skip of it as in use (or locked); one as
/// protected or excluded by a negated profile; one kept by a nested guard
/// (a keep marker, a checkout, or nested special content).
const CATEGORIES: [(&str, &[&str]); 4] = [
    ("match", &[]),
    ("in_use", &["in_use", "locked"]),
    ("protected_or_negated", &["protected", "excluded"]),
    (
        "nested_guard",
        &["kept", "contains_checkout", "contains_special"],
    ),
];

/// Categories that cannot apply to a source, by its action, and why.
fn not_applicable(action: &Action) -> &'static [&'static str] {
    match action {
        // Docker prunes act on a daemon's own store, whose protections are
        // Docker's (running containers); protect and !profile take only fs
        // and cargo-target sources, and no walk runs (docs/safety.md,
        // Docker kinds).
        Action::DockerBuilderPrune | Action::DockerImagePrune { .. } => {
            &["in_use", "protected_or_negated", "nested_guard"]
        }
        Action::Delete { .. } | Action::CargoLockProtocol | Action::Command { .. } => &[],
    }
}

/// AC 3: every built-in source has a vector per category, judged from the
/// results of vectors that ran the unchanged built-in.
#[test]
fn every_builtin_source_has_match_in_use_guarded_and_nested_vectors() {
    let library = Library::builtin().expect("built-in profiles");
    let mut shown: BTreeSet<(String, &str)> = BTreeSet::new();
    for (_, result) in corpus() {
        let Ok(checked) = result else { continue };
        let builtin = |profile: &str| checked.builtin.contains(profile);
        for action in checked
            .reclaim
            .actions
            .iter()
            .filter(|a| builtin(&a.profile))
        {
            shown.insert((action.profile.clone(), "match"));
        }
        for skip in checked
            .reclaim
            .skipped
            .iter()
            .filter(|s| builtin(&s.profile))
        {
            for (category, reasons) in CATEGORIES {
                if reasons.contains(&skip.reason) {
                    shown.insert((skip.profile.clone(), category));
                }
            }
        }
    }
    let mut missing = Vec::new();
    for profile in library.profiles() {
        let Body::Source(source) = &profile.body else {
            continue;
        };
        for (category, _) in CATEGORIES {
            if not_applicable(&source.action).contains(&category) {
                continue;
            }
            if !shown.contains(&(profile.name.clone(), category)) {
                missing.push(format!("{} {category}", profile.name));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{} built-in source categories without a vector:\n{}",
        missing.len(),
        missing.join("\n")
    );
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
