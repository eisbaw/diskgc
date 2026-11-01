//! The vector corpus (docs/testing.md, Vector corpus): every YAML file
//! below `tests/models/profiles` and `tests/vectors` runs through the one
//! runner, and the corpus as a whole must cover every skip reason and every
//! built-in source. Adding a vector does not require another Rust test.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use diskgc_core::engine::{ProcessEvidence, RESERVED_REASONS, Reason};
use diskgc_core::profile::{Action, Body, Library, parse_profiles};
use diskgc_model::profile_vector::{Checked, ProfileVector, VectorCatalog, corpus_files};

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

/// The `release` area is exactly the vectors that run the 0.1 catalog: one put there without `catalog:` would silently run the
/// development catalog and still pass.
#[test]
fn release_vectors_are_exactly_the_release_catalog_ones() {
    let release = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/release");
    let files = corpus_files(&corpus_roots()).expect("corpus layout");
    let mut in_area = 0;
    for path in &files {
        let under = path.starts_with(&release);
        in_area += usize::from(under);
        let release_catalog = read(path).catalog == VectorCatalog::Release01;
        assert_eq!(
            under,
            release_catalog,
            "{}: below tests/vectors/release iff `catalog: release-0.1`",
            path.display()
        );
    }
    assert!(in_area > 0, "no vectors below {}", release.display());
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

/// AC 2: every skip reason the report can carry (`Reason::ALL`) is shown
/// by at least one vector, as a dry-run plan reason or a reclaim skip that
/// matched its expectation. The report's reason is a `Reason`, so ALL is
/// every value the code can produce; diskgc-core's
/// `every_status_and_reason_is_a_documented_value` pins that ALL plus
/// `RESERVED_REASONS` is exactly reference.md's enum. A reserved reason
/// (documented, no producer yet) is exempt, and the exemption shrinks
/// itself: this fails once one enters `Reason::ALL` or a vector shows it.
#[test]
fn every_skip_reason_has_a_vector() {
    let mut shown = BTreeSet::new();
    for (_, result) in corpus() {
        if let Ok(checked) = result {
            shown.extend(
                checked
                    .dry_run
                    .plan
                    .iter()
                    .filter_map(|row| row.reason.map(Reason::as_str)),
            );
            shown.extend(
                checked
                    .reclaim
                    .skipped
                    .iter()
                    .map(|row| row.reason.as_str()),
            );
        }
    }
    let in_enum: BTreeSet<&str> = Reason::ALL.iter().map(|reason| reason.as_str()).collect();
    for (reason, task) in RESERVED_REASONS {
        assert!(
            !in_enum.contains(reason),
            "`{reason}` is reserved ({task}: no producer) but is now in Reason::ALL: \
             write its vector and drop it from RESERVED_REASONS"
        );
        assert!(
            !shown.contains(reason),
            "`{reason}` is reserved ({task}) but a vector shows it: drop it from RESERVED_REASONS"
        );
    }
    let missing: Vec<_> = in_enum.difference(&shown).collect();
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
        &["keep_marker", "contains_checkout", "contains_special"],
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
                if reasons.contains(&skip.reason.as_str()) {
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
    source.liveness_class = None;
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

/// The runner's own checks can fail: each perturbation of a passing
/// vector's expectations or events is rejected with its own message.
#[test]
fn runner_invariants_reject_perturbed_vectors() {
    let vectors = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
    let text =
        |rel: &str| fs::read_to_string(vectors.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let fails = |vector: ProfileVector, wanted: &str| {
        let error = vector
            .check_builtin()
            .err()
            .unwrap_or_else(|| panic!("perturbed vector passed; wanted {wanted:?}"));
        assert!(error.contains(wanted), "wanted {wanted:?} in: {error}");
    };
    let walk =
        || ProfileVector::parse(&text("walk/empty-and-zero-size-candidates.yaml")).expect("parses");
    walk()
        .check_builtin()
        .expect("the unperturbed vector passes");

    let mut missing_action = walk();
    missing_action.expect.actions.pop();
    fails(missing_action, "reclaim actions");

    let mut no_postcondition = walk();
    let kept = no_postcondition.expect.removed.remove(0);
    let others = no_postcondition.expect.removed.clone();
    no_postcondition.expect.removed = others;
    fails(
        no_postcondition,
        &format!("action {} has no explicit", kept.display()),
    );

    let mut wrong_exit = walk();
    wrong_exit.expect.reclaim_exit = 0;
    fails(wrong_exit, "reclaim exit 1, expected 0");

    let unfired = text("races/new-keep-marker-after-planning.yaml").replace("nth: 1", "nth: 99");
    let mut never = ProfileVector::parse(&unfired).expect("parses");
    // Without its race the candidate is deleted; expect that, so only the
    // unfired event is wrong.
    never.expect.actions = vec!["/tmp/zephyr-a".into()];
    never.expect.removed = vec!["/tmp/zephyr-a".into()];
    never.expect.skipped.clear();
    never.expect.preserved.clear();
    fails(never, "unfired events");

    let mut wrong_command = ProfileVector::parse(&text("builtins/pip-cache.yaml")).expect("parses");
    wrong_command.expect.commands.as_mut().expect("commands")[0].cwd = "/".into();
    fails(wrong_command, "commands");

    let mut wrong_blocker =
        ProfileVector::parse(&text("inuse/each-kind-of-use-blocks-its-candidate.yaml"))
            .expect("parses");
    wrong_blocker.expect.skipped[0]
        .process_blockers
        .as_mut()
        .expect("blockers")[0]
        .evidence = "cwd".into();
    fails(wrong_blocker, "reclaim skipped");
}

/// Every `process_blockers` evidence value of the contract
/// (`ProcessEvidence::ALL`, the same set as reference.md's enum) is shown
/// by a vector whose expectations matched.
#[test]
fn every_blocker_evidence_has_a_vector() {
    let mut shown = BTreeSet::new();
    for (_, result) in corpus() {
        if let Ok(checked) = result {
            for skip in &checked.reclaim.skipped {
                for blocker in skip.process_blockers.iter().flatten() {
                    shown.insert(blocker.evidence.as_str());
                }
            }
        }
    }
    let missing: Vec<_> = ProcessEvidence::ALL
        .iter()
        .map(|e| e.as_str())
        .filter(|e| !shown.contains(e))
        .collect();
    assert!(
        missing.is_empty(),
        "blocker evidence without a vector: {missing:?}"
    );
}
