//! A small, independently interpreted filesystem selection universe. Unlike
//! `selection_reference`, names, roots, entry types and exclusions vary too.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::{Bytes, DirSpec, FileSpec, HostModel, NodeSpec};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

const ROOTS: [&str; 2] = ["/tmp/sel", "/tmp/other"];
const PATTERNS: [&str; 14] = [
    "*", "a*", "b*", "*0", "*1", "a*0", "*/*", "*/x*", "*/y*", "a*/*", "b*/*", "*0/*1", "a*/x*",
    "b*/y*",
];
const EXCLUDES: [&str; 4] = ["", "a*", "*/x*", "*1"];
const TYPES: [&str; 3] = ["dir", "file", "any"];
const NAMES: [&str; 3] = ["a", "b", "c"];

#[derive(Clone, Debug)]
struct Case {
    // Each root has two outer entries and each outer entry has two children.
    present: u16,
    files: u16,
    tagged: u16,
    digits: [u8; 4],
    patterns: [u8; 3],
    // 14 means this source has one pattern; otherwise it has two.
    extra_patterns: [u8; 3],
    excludes: [u8; 3],
    types: [u8; 3],
    roots: [u8; 3], // 1, 2 or 3: bitset of selected roots
    source_tagged: [bool; 3],
    first: usize,
    second: usize,
    factor: [u8; 2], // none, has TAG, !has TAG
    exclude_c: bool,
    union: bool,
}

fn path(case: &Case, root: usize, slot: usize) -> PathBuf {
    let outer = |which: usize| {
        format!(
            "{}{digit}",
            if which == 0 { 'a' } else { 'b' },
            digit = case.digits[which]
        )
    };
    let relative = if slot < 2 {
        outer(slot)
    } else {
        let parent = (slot - 2) / 2;
        let leaf = (slot - 2) % 2;
        format!(
            "{}/{}{}",
            outer(parent),
            if leaf == 0 { 'x' } else { 'y' },
            case.digits[2 + leaf]
        )
    };
    Path::new(ROOTS[root]).join(relative)
}

fn bit(root: usize, slot: usize) -> u16 {
    1 << (root * 6 + slot)
}

fn exists(case: &Case, root: usize, slot: usize) -> bool {
    case.present & bit(root, slot) != 0
        && (slot < 2
            || (case.present & bit(root, (slot - 2) / 2) != 0
                && case.files & bit(root, (slot - 2) / 2) == 0))
}

fn model(case: &Case) -> HostModel {
    let mut model = diskgc_model::load::parse_yaml(
        "host_model: 1\nclock: 2026-10-15T03:00:00Z\nmounts:\n  - {id: 1, at: /, fs: ext4, dev: '254:1', size: 100G}\n  - {id: 30, at: /tmp, fs: tmpfs, dev: '0:42', size: 1G, other_used: 500M, parent: 1}\n",
    ).expect("base model");
    model.tree.insert(
        PathBuf::from("/run/diskgc/lock"),
        NodeSpec::File(FileSpec::default()),
    );
    for (root, root_path) in ROOTS.iter().enumerate() {
        let mut root_dir = DirSpec::default();
        for parent in 0..2 {
            if !exists(case, root, parent) {
                continue;
            }
            let parent_name = path(case, root, parent)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            let mut entries = DirSpec::default();
            for child in (2 + parent * 2)..(4 + parent * 2) {
                if !exists(case, root, child) {
                    continue;
                }
                let name = path(case, root, child)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                entries
                    .entries
                    .insert(name, node(case, root, child, DirSpec::default()));
            }
            root_dir
                .entries
                .insert(parent_name, node(case, root, parent, entries));
        }
        model
            .tree
            .insert(PathBuf::from(*root_path), NodeSpec::Dir(root_dir));
    }
    model
}

fn node(case: &Case, root: usize, slot: usize, mut dir: DirSpec) -> NodeSpec {
    if case.files & bit(root, slot) != 0 {
        return NodeSpec::File(FileSpec {
            size: Some(Bytes(4096)),
            ..FileSpec::default()
        });
    }
    if case.tagged & bit(root, slot) != 0 {
        dir.entries
            .insert("TAG".to_owned(), NodeSpec::File(FileSpec::default()));
    }
    NodeSpec::Dir(dir)
}

// A deliberately separate glob interpreter: enumerate possible positions of
// each '*' over a single segment. The product uses a compiled matcher.
fn segment_matches(pattern: &str, name: &str) -> bool {
    let Some(star) = pattern.find('*') else {
        return pattern == name;
    };
    let prefix = &pattern[..star];
    if !name.starts_with(prefix) {
        return false;
    }
    let remaining = &name[prefix.len()..];
    (0..=remaining.len()).any(|length| {
        remaining.is_char_boundary(length)
            && segment_matches(&pattern[star + 1..], &remaining[length..])
    })
}

fn path_matches(pattern: &str, relative: &Path) -> bool {
    let names: Vec<_> = relative.iter().map(|part| part.to_str().unwrap()).collect();
    let parts: Vec<_> = pattern.split('/').collect();
    names.len() == parts.len()
        && names
            .iter()
            .zip(parts)
            .all(|(name, pattern)| segment_matches(pattern, name))
}

fn sample_and_exclusion(case: &Case, source: usize) -> Option<(String, &'static str)> {
    let exclusion = EXCLUDES[case.excludes[source] as usize];
    for pattern in source_patterns(case, source) {
        for replacement in ["0", "1", "q", "x", "y", "z"] {
            let sample = pattern.replace('*', replacement);
            if exclusion.is_empty() || !path_matches(exclusion, Path::new(&sample)) {
                return Some((sample, exclusion));
            }
        }
    }
    // This profile cannot carry a valid `examples.match`. The generator
    // rejects it; callers constructing a fixture directly fail loudly.
    None
}

fn source_patterns(case: &Case, source: usize) -> Vec<&'static str> {
    let mut patterns = vec![PATTERNS[case.patterns[source] as usize]];
    if let Some(extra) = PATTERNS.get(case.extra_patterns[source] as usize) {
        patterns.push(extra);
    }
    patterns
}

// Marker files are structural entries: a broad file pattern may select one.
fn facts(case: &Case) -> Vec<(PathBuf, bool, bool)> {
    let mut facts = Vec::new();
    for root in 0..2 {
        for slot in 0..6 {
            if !exists(case, root, slot) {
                continue;
            }
            let candidate = path(case, root, slot);
            let is_file = case.files & bit(root, slot) != 0;
            let has_tag = !is_file && case.tagged & bit(root, slot) != 0;
            facts.push((candidate.clone(), is_file, has_tag));
            if has_tag {
                facts.push((candidate.join("TAG"), true, false));
            }
        }
    }
    facts.sort_by(|a, b| a.0.cmp(&b.0));
    facts
}

fn raw(case: &Case, source: usize) -> BTreeSet<PathBuf> {
    found(case, source, true)
}

fn found(case: &Case, source: usize, ignore_where: bool) -> BTreeSet<PathBuf> {
    let mut result = BTreeSet::new();
    for (candidate, is_file, has_tag) in facts(case) {
        let root = ROOTS
            .iter()
            .position(|root| candidate.starts_with(root))
            .unwrap();
        if case.roots[source] & (1 << root) == 0 {
            continue;
        }
        let relative = candidate.strip_prefix(ROOTS[root]).unwrap();
        if !source_patterns(case, source)
            .iter()
            .any(|pattern| path_matches(pattern, relative))
        {
            continue;
        }
        let (_, exclusion) = sample_and_exclusion(case, source)
            .expect("generated source must admit an examples.match path");
        if !exclusion.is_empty() && path_matches(exclusion, relative) {
            continue;
        }
        if match case.types[source] {
            0 => is_file,
            1 => !is_file,
            _ => false,
        } {
            continue;
        }
        // A successfully admitted outer candidate stops this source's
        // descent. A failed source `where` does not stop it.
        if !ignore_where && case.source_tagged[source] && !has_tag {
            continue;
        }
        if result
            .iter()
            .any(|outer: &PathBuf| candidate.starts_with(outer))
        {
            continue;
        }
        result.insert(candidate);
    }
    result
}

fn expected(case: &Case) -> BTreeSet<PathBuf> {
    let protected = raw(case, 2);
    let mut result = BTreeSet::new();
    for (source, factor, enabled) in [
        (case.first, case.factor[0], true),
        (case.second, case.factor[1], case.union),
    ] {
        if !enabled {
            continue;
        }
        for candidate in found(case, source, false) {
            let known_tag = facts(case)
                .into_iter()
                .find(|fact| fact.0 == candidate)
                .unwrap()
                .2;
            if factor == 1 && !known_tag || factor == 2 && known_tag {
                continue;
            }
            if case.exclude_c
                && protected
                    .iter()
                    .any(|guard| guard.starts_with(&candidate) || candidate.starts_with(guard))
            {
                continue;
            }
            result.insert(candidate);
        }
    }
    result
}

fn profiles(case: &Case) -> String {
    let mut yaml = String::new();
    for source in 0..3 {
        let roots = (0..2)
            .filter(|&root| case.roots[source] & (1 << root) != 0)
            .map(|root| ROOTS[root])
            .collect::<Vec<_>>()
            .join(", ");
        let patterns = source_patterns(case, source)
            .into_iter()
            .map(|pattern| format!("'{pattern}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let (sample, exclusion) = sample_and_exclusion(case, source)
            .expect("generated source must admit an examples.match path");
        let exclude = if exclusion.is_empty() {
            String::new()
        } else {
            format!(", exclude: ['{exclusion}']")
        };
        yaml.push_str(&format!(
            "- name: {}\n  find: {{roots: [{}], paths: [{}], type: {}{}}}\n  action: delete\n  examples: {{match: [{}/{}]}}\n",
            NAMES[source], roots, patterns, TYPES[case.types[source] as usize], exclude,
            ROOTS[case.roots[source].trailing_zeros() as usize], sample,
        ));
        if case.source_tagged[source] {
            yaml.push_str("  where: {has: TAG}\n");
        }
    }
    yaml.push_str("- name: has-tag\n  where: {has: TAG}\n");
    yaml
}

fn expression(case: &Case) -> String {
    let term = |source: usize, factor: u8| {
        let filter = match factor {
            0 => "",
            1 => " && filter:has-tag",
            _ => " && !filter:has-tag",
        };
        format!(
            "profile:{}{}{}",
            NAMES[source],
            filter,
            if case.exclude_c { " && !profile:c" } else { "" }
        )
    };
    if !case.union {
        return term(case.first, case.factor[0]);
    }
    let member = |text: String| {
        if text.contains(" && ") {
            format!("({text})")
        } else {
            text
        }
    };
    format!(
        "{} || {}",
        member(term(case.first, case.factor[0])),
        member(term(case.second, case.factor[1]))
    )
}

/// Saved minimal boundary: an exclusion pattern removes each `x` file, and
/// a negated source directory in the second root vetoes its `y` file by overlap.
fn saved_case() -> Case {
    Case {
        present: 0b0011_0100_1101,
        files: 0b0011_0000_1100,
        tagged: 0,
        digits: [0; 4],
        patterns: [6, 1, 1],
        extra_patterns: [14; 3],
        excludes: [2, 0, 0],
        types: [1, 0, 0],
        roots: [3, 3, 2],
        source_tagged: [false; 3],
        first: 0,
        second: 1,
        factor: [0; 2],
        exclude_c: true,
        union: false,
    }
}

#[test]
fn excluded_leaf_and_other_root_negation_leave_only_first_root_y_file() {
    let case = saved_case();
    let yaml = profiles(&case);
    let specs = parse_profiles(&yaml).expect("saved profiles");
    let library = Library::with_user_profiles(&specs).expect("saved profile library");
    let tier = Tier::select(&expression(&case), &library, None, 1000).expect("saved selection");
    let request = Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").unwrap(),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier,
    };
    let model = model(&case);
    let host = ModelHost::new(&model).expect("saved model");
    let plan = executor::dry_run(&host, &request, &[1000], &mut |_| {});
    assert!(plan.errors.is_empty(), "{plan:#?}");
    let eligible = plan
        .plan
        .iter()
        .filter(|entry| entry.reason.is_none())
        .map(|entry| entry.candidate.clone())
        .collect::<BTreeSet<_>>();
    let only_y = BTreeSet::from([PathBuf::from("/tmp/sel/a0/y0")]);
    assert_eq!(expected(&case), only_y, "oracle fixture changed");
    assert_eq!(eligible, only_y, "{plan:#?}");
}

#[test]
fn an_exclusion_covering_every_positive_match_has_no_valid_example() {
    let mut case = saved_case();
    case.patterns[0] = 1; // a*
    case.extra_patterns[0] = 14;
    case.excludes[0] = 1; // a*
    assert!(sample_and_exclusion(&case, 0).is_none());
    case.extra_patterns[0] = 2; // b* survives the a* exclusion
    assert!(sample_and_exclusion(&case, 0).is_some());
    assert!(sample_and_exclusion(&case, 1).is_some());
}

#[test]
fn source_where_rejecting_outer_match_allows_its_second_pattern_to_find_child() {
    let mut case = saved_case();
    case.present = bit(0, 0) | bit(0, 2);
    case.files = 0;
    case.tagged = bit(0, 2);
    case.patterns[0] = 0; // * matches the untagged outer directory
    case.extra_patterns[0] = 6; // */* matches its tagged child
    case.excludes[0] = 0;
    case.types[0] = 0; // directory
    case.source_tagged[0] = true;
    case.first = 0;
    case.union = false;
    case.exclude_c = false;

    let yaml = profiles(&case);
    let specs = parse_profiles(&yaml).expect("saved profiles");
    let library = Library::with_user_profiles(&specs).expect("saved profile library");
    let tier = Tier::select(&expression(&case), &library, None, 1000).expect("saved selection");
    let request = Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").unwrap(),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier,
    };
    let model = model(&case);
    let host = ModelHost::new(&model).expect("saved model");
    let plan = executor::dry_run(&host, &request, &[1000], &mut |_| {});
    assert!(plan.errors.is_empty(), "{plan:#?}");
    let actual = plan
        .plan
        .iter()
        .filter(|entry| entry.reason.is_none())
        .map(|entry| entry.candidate.clone())
        .collect::<BTreeSet<_>>();
    let child = BTreeSet::from([PathBuf::from("/tmp/sel/a0/x0")]);
    assert_eq!(expected(&case), child, "oracle fixture changed");
    assert_eq!(actual, child, "{plan:#?}");
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("DISKGC_PROPERTY_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(32),
        rng_seed: RngSeed::Fixed(0x16_2027), failure_persistence: None, ..ProptestConfig::default()
    })]

    #[test]
    fn generated_names_roots_types_and_exclusions_match_reference(
        present in 0u16..4096, files in 0u16..4096, tagged in 0u16..4096,
        digits in prop::array::uniform4(0u8..3),
        patterns in prop::array::uniform3(0u8..14),
        extra_patterns in prop::array::uniform3(0u8..15),
        excludes in prop::array::uniform3(0u8..4),
        types in prop::array::uniform3(0u8..3),
        roots in prop::array::uniform3(1u8..4),
        source_tagged in prop::array::uniform3(any::<bool>()),
        first in 0usize..2, second in 0usize..2,
        factor in prop::array::uniform2(0u8..3),
        exclude_c in any::<bool>(), union in any::<bool>(),
    ) {
        let case = Case { present, files, tagged, digits, patterns, extra_patterns, excludes, types,
            roots, source_tagged, first, second, factor, exclude_c, union };
        prop_assume!((0..3).all(|source| sample_and_exclusion(&case, source).is_some()));
        let yaml = profiles(&case);
        let expr = expression(&case);
        let model = model(&case);
        let replay = format!("case: {case:?}\nprofiles:\n{yaml}\nselect: {expr}\n{}", model.to_yaml());
        let specs = parse_profiles(&yaml).map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let library = Library::with_user_profiles(&specs).map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let tier = Tier::select(&expr, &library, None, 1000).map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let request = Request { path: PathBuf::from("/tmp"), min_free: parse_size("100%").unwrap(),
            target_free: None, timeout: Some(Duration::from_secs(600)), tally_budget: None, tier };
        let host = ModelHost::new(&model).map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let plan = executor::dry_run(&host, &request, &[1000], &mut |_| {});
        prop_assert!(plan.errors.is_empty(), "{plan:#?}\n{replay}");
        let actual = plan.plan.iter().filter(|entry| entry.reason.is_none())
            .map(|entry| entry.candidate.clone()).collect::<BTreeSet<_>>();
        prop_assert_eq!(actual, expected(&case), "{:#?}\n{}", plan, replay);
    }
}
