//! Golden match sets for the wave-1 built-ins. These expected sets are
//! literal data: changing a profile's meaning under its name turns a
//! test red and requires a new name.

use std::path::PathBuf;

use diskgc_core::cargo_target;
use diskgc_core::host::Timestamp;
use diskgc_core::profile::{Body, Idleness, Library, Origin, Roots};
use diskgc_core::profile::{EntryType, examples::Entry, examples::matches_entry, parse_profiles};
use diskgc_model::ModelHost;

const CARGO_TREE: &str = r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}
  - {id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}
tree:
  /tmp/work/old/target:
    CACHEDIR.TAG: {cachedir_tag: true}
    debug: {.cargo-lock: {}, deps: {old: {size: 1M}}}
  /tmp/work/with-tag-only/target:
    CACHEDIR.TAG: {cachedir_tag: true}
    debug: {deps: {file: {}}}
  /tmp/work/with-lock-only/target:
    CACHEDIR.TAG: {}
    debug: {.cargo-lock: {}}
  /tmp/work/.agent/project/target:
    CACHEDIR.TAG: {cachedir_tag: true}
    release: {.cargo-lock: {}, deps: {new: {size: 1M}}}
"#;

#[test]
fn cargo_target_golden_match_set() {
    let library = Library::builtin().unwrap();
    let profile = library.get("cargo-target").expect("named built-in");
    assert_eq!(profile.origin, Origin::BuiltIn);
    let Body::Source(source) = &profile.body else {
        panic!("cargo-target is a source")
    };
    assert_eq!(source.roots, Roots::Required);
    assert_eq!(source.kind().name(), "cargo-target");
    let host = ModelHost::from_yaml(CARGO_TREE).unwrap();
    let found = cargo_target::discover(&host, &[PathBuf::from("/tmp/work")], None).unwrap();
    let actual: Vec<String> = found
        .targets
        .iter()
        .map(|t| t.path.display().to_string())
        .collect();
    assert_eq!(
        actual,
        ["/tmp/work/.agent/project/target", "/tmp/work/old/target",]
    );
}

/// The fixture ages are exact boundaries and near misses. This pins
/// every built-in filter's match set under its name, including equality.
#[test]
fn idle_filter_golden_match_sets() {
    let library = Library::builtin().unwrap();
    let now = Timestamp {
        secs: 4_000_000,
        nanos: 0,
    };
    let ages_hours = [0_u64, 1, 23, 24, 48, 72, 168, 240, 720, 744];
    let fixtures: Vec<Timestamp> = ages_hours
        .iter()
        .map(|hours| Timestamp {
            secs: now.secs - (*hours as i64 * 3600),
            nanos: 0,
        })
        .collect();
    let expected = [
        ("idle-1h", vec![1, 23, 24, 48, 72, 168, 240, 720, 744]),
        ("idle-1d", vec![24, 48, 72, 168, 240, 720, 744]),
        ("idle-2d", vec![48, 72, 168, 240, 720, 744]),
        ("idle-3d", vec![72, 168, 240, 720, 744]),
        ("idle-7d", vec![168, 240, 720, 744]),
        ("idle-10d", vec![240, 720, 744]),
        ("idle-30d", vec![720, 744]),
    ];
    for (name, golden) in expected {
        let profile = library.get(name).expect("named built-in");
        assert_eq!(profile.origin, Origin::BuiltIn, "{name}");
        let Body::Filter(where_) = &profile.body else {
            panic!("{name} is a filter")
        };
        let idle = where_.idle_for.expect("idle_for");
        let actual: Vec<u64> = ages_hours
            .iter()
            .zip(&fixtures)
            .filter_map(|(hours, time)| {
                (idle.verdict(Some(*time), now) == Idleness::Idle).then_some(*hours)
            })
            .collect();
        assert_eq!(actual, golden, "{name} changed its match set");
    }
}

/// Scratch proof of the fs golden mechanism: changing the distinguishing
/// pattern changes the match set against unchanged fixture paths.
#[test]
fn a_scratch_pattern_mutation_changes_its_golden_set() {
    let yaml = "- name: scratch\n  find: {roots: [/tmp], paths: ['scratch-*'], type: dir}\n  action: delete\n  examples: {match: [/tmp/scratch-old]}\n";
    let fixture = ["/tmp/scratch-old", "/tmp/scratch-new", "/tmp/unrelated"];
    let matched = |definition: &str| {
        let specs = parse_profiles(definition).unwrap();
        let library = Library::with_user_profiles(&specs).unwrap();
        let Body::Source(source) = &library.get("scratch").unwrap().body else {
            panic!("source")
        };
        fixture
            .iter()
            .filter(|path| {
                matches_entry(
                    source,
                    1000,
                    &Entry {
                        path: std::path::Path::new(path),
                        entry_type: EntryType::Dir,
                        files: &[],
                        dirs: &[],
                    },
                )
            })
            .copied()
            .collect::<Vec<_>>()
    };
    let golden = vec!["/tmp/scratch-old", "/tmp/scratch-new"];
    assert_eq!(matched(yaml), golden);
    let mutant = yaml.replace("scratch-*", "scratch-new");
    assert_ne!(
        matched(&mutant),
        golden,
        "the pattern mutation must be caught"
    );
}
