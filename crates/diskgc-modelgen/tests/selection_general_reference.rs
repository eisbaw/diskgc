//! A small, independently interpreted filesystem selection universe. Unlike
//! `selection_reference`, names, roots, entry types, exclusions and marker
//! evidence vary too.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::config;
use diskgc_core::engine::Reason;
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
    flagged: u16,
    // A present TAG whose statx type cannot be answered is unknown evidence.
    unknown_tag: u16,
    unknown_flag: u16,
    digits: [u8; 4],
    patterns: [u8; 3],
    // 14 means this source has one pattern; otherwise it has two.
    extra_patterns: [u8; 3],
    excludes: [u8; 3],
    types: [u8; 3],
    roots: [u8; 3],        // 1, 2 or 3: bitset of selected roots
    source_where: [u8; 3], // none, TAG/FLAG checks, or their conjunction
    first: usize,
    second: usize,
    factor: [u8; 2], // none, four TAG polarities, mixed TAG/FLAG and its complement
    exclude_c: bool,
    protect_c: bool,
    union: bool,
    // A union written in the factored surface form
    // `(profile:A || profile:B) && FACTOR...`; both terms then share factor[0].
    grouped: bool,
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
    if case.flagged & bit(root, slot) != 0 {
        dir.entries
            .insert("FLAG".to_owned(), NodeSpec::File(FileSpec::default()));
    }
    NodeSpec::Dir(dir)
}

fn inject_unknown_tags(host: &ModelHost, case: &Case) {
    for root in 0..2 {
        for slot in 0..6 {
            if exists(case, root, slot)
                && case.files & bit(root, slot) == 0
                && case.tagged & case.unknown_tag & bit(root, slot) != 0
            {
                host.omit_statx_type_at(path(case, root, slot).join("TAG"));
            }
            if exists(case, root, slot)
                && case.files & bit(root, slot) == 0
                && case.flagged & case.unknown_flag & bit(root, slot) != 0
            {
                host.omit_statx_type_at(path(case, root, slot).join("FLAG"));
            }
        }
    }
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
// If statx omits a marker's type, it is neither a candidate nor known `has`
// evidence for its parent.
#[derive(Clone)]
struct Fact {
    path: PathBuf,
    is_file: bool,
    tag: Option<bool>,
    flag: Option<bool>,
}

fn facts(case: &Case) -> Vec<Fact> {
    let mut facts = Vec::new();
    for root in 0..2 {
        for slot in 0..6 {
            if !exists(case, root, slot) {
                continue;
            }
            let candidate = path(case, root, slot);
            let is_file = case.files & bit(root, slot) != 0;
            let has_tag = !is_file && case.tagged & bit(root, slot) != 0;
            let has_flag = !is_file && case.flagged & bit(root, slot) != 0;
            let tag_unknown = has_tag && case.unknown_tag & bit(root, slot) != 0;
            let flag_unknown = has_flag && case.unknown_flag & bit(root, slot) != 0;
            facts.push(Fact {
                path: candidate.clone(),
                is_file,
                tag: (!tag_unknown).then_some(has_tag),
                flag: (!flag_unknown).then_some(has_flag),
            });
            for (name, present, unknown) in [
                ("TAG", has_tag, tag_unknown),
                ("FLAG", has_flag, flag_unknown),
            ] {
                if present && !unknown {
                    facts.push(Fact {
                        path: candidate.join(name),
                        is_file: true,
                        tag: Some(false),
                        flag: Some(false),
                    });
                }
            }
        }
    }
    facts.sort_by(|a, b| a.path.cmp(&b.path));
    facts
}

fn raw(case: &Case, source: usize) -> BTreeSet<PathBuf> {
    found(case, source, true)
}

fn marker_passes(choice: u8, tag: Option<bool>, flag: Option<bool>) -> bool {
    match choice {
        0 => true,
        1 | 4 => tag == Some(true),
        2 | 3 => tag == Some(false),
        5 => tag == Some(true) && flag == Some(false),
        6 => tag.is_some() && flag.is_some() && !(tag == Some(true) && flag == Some(false)),
        _ => unreachable!(),
    }
}

fn source_where_passes(choice: u8, tag: Option<bool>, flag: Option<bool>) -> bool {
    match choice {
        0 => true,
        1 => tag == Some(true),
        2 => tag == Some(false),
        3 => flag == Some(true),
        4 => flag == Some(false),
        5 => tag == Some(true) && flag == Some(false),
        _ => unreachable!(),
    }
}

// Eligibility scans the entire candidate tree. An entry whose type cannot
// be established makes that walk incomplete, even when selection predicates
// do not inspect the entry.
fn walk_known(case: &Case, candidate: &Path) -> bool {
    !(0..2).any(|root| {
        (0..6).any(|slot| {
            exists(case, root, slot)
                && case.files & bit(root, slot) == 0
                && [
                    ("TAG", case.tagged & case.unknown_tag),
                    ("FLAG", case.flagged & case.unknown_flag),
                ]
                .into_iter()
                .any(|(name, bits)| {
                    bits & bit(root, slot) != 0
                        && path(case, root, slot).join(name).starts_with(candidate)
                })
        })
    })
}

fn found(case: &Case, source: usize, ignore_where: bool) -> BTreeSet<PathBuf> {
    let mut result = BTreeSet::new();
    for Fact {
        path: candidate,
        is_file,
        tag,
        flag,
    } in facts(case)
    {
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
        if !ignore_where && !source_where_passes(case.source_where[source], tag, flag) {
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
            if !walk_known(case, &candidate) {
                continue;
            }
            let evidence = facts(case)
                .into_iter()
                .find(|fact| fact.path == candidate)
                .unwrap();
            if !marker_passes(factor, evidence.tag, evidence.flag) {
                continue;
            }
            if (case.exclude_c || case.protect_c)
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
        match case.source_where[source] {
            0 => {}
            1 => yaml.push_str("  where: {has: TAG}\n"),
            2 => yaml.push_str("  where: {has_not: TAG}\n"),
            3 => yaml.push_str("  where: {has: FLAG}\n"),
            4 => yaml.push_str("  where: {has_not: FLAG}\n"),
            5 => yaml.push_str("  where: {has: TAG, has_not: FLAG}\n"),
            _ => unreachable!(),
        }
    }
    yaml.push_str(
        "- name: has-tag\n  where: {has: TAG}\n- name: lacks-tag\n  where: {has_not: TAG}\n- name: tagged-unflagged\n  where: {has: TAG, has_not: FLAG}\n",
    );
    yaml
}

fn expression(case: &Case) -> String {
    let term = |source: usize, factor: u8| {
        let filter = match factor {
            0 => "",
            1 => " && filter:has-tag",
            2 => " && !filter:has-tag",
            3 => " && filter:lacks-tag",
            4 => " && !filter:lacks-tag",
            5 => " && filter:tagged-unflagged",
            6 => " && !filter:tagged-unflagged",
            _ => unreachable!(),
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
    if case.grouped {
        assert_eq!(case.factor[0], case.factor[1], "a group shares its factors");
        // The same factors as the distributed form below, so the oracle's
        // per-term set expression is unchanged: grouping is pure syntax.
        let factored = term(case.first, case.factor[0]);
        let factors = factored
            .strip_prefix(&format!("profile:{}", NAMES[case.first]))
            .expect("term starts with its profile");
        return format!(
            "(profile:{} || profile:{}){factors}",
            NAMES[case.first], NAMES[case.second]
        );
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
        flagged: 0,
        unknown_tag: 0,
        unknown_flag: 0,
        digits: [0; 4],
        patterns: [6, 1, 1],
        extra_patterns: [14; 3],
        excludes: [2, 0, 0],
        types: [1, 0, 0],
        roots: [3, 3, 2],
        source_where: [0; 3],
        first: 0,
        second: 1,
        factor: [0; 2],
        exclude_c: true,
        protect_c: false,
        union: false,
        grouped: false,
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

/// Saved boundary for the factored surface form: a group's factors apply to
/// every source in it, not only the first. Runs in `just check`, where the
/// generated property's 32 cases do not reliably reach this shape.
#[test]
fn grouped_union_applies_its_factor_to_every_member() {
    let mut case = saved_case();
    case.present = bit(0, 0) | bit(0, 1);
    case.files = 0;
    case.tagged = bit(0, 1);
    case.patterns = [1, 2, 1]; // a*, b*, a*
    case.extra_patterns = [14; 3];
    case.excludes = [0; 3];
    case.types = [0; 3]; // directories
    case.roots = [1; 3];
    case.first = 0;
    case.second = 1;
    case.factor = [2, 2]; // !filter:has-tag
    case.exclude_c = false;
    case.union = true;
    case.grouped = true;
    let selection = expression(&case);
    assert_eq!(selection, "(profile:a || profile:b) && !filter:has-tag");
    let specs = parse_profiles(&profiles(&case)).expect("saved profiles");
    let library = Library::with_user_profiles(&specs).expect("saved profile library");
    let tier = Tier::select(&selection, &library, None, 1000).expect("saved selection");
    let request = Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").unwrap(),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier,
    };
    let host = ModelHost::new(&model(&case)).expect("saved model");
    let plan = executor::dry_run(&host, &request, &[1000], &mut |_| {});
    assert!(plan.errors.is_empty(), "{plan:#?}");
    let eligible = plan
        .plan
        .iter()
        .filter(|entry| entry.reason.is_none())
        .map(|entry| entry.candidate.clone())
        .collect::<BTreeSet<_>>();
    // b0 is tagged, so the shared !filter:has-tag must keep it out.
    let only_a = BTreeSet::from([PathBuf::from("/tmp/sel/a0")]);
    assert_eq!(expected(&case), only_a, "oracle fixture changed");
    assert_eq!(eligible, only_a, "{plan:#?}");
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
    case.source_where[0] = 1;
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

#[test]
fn config_protect_uses_raw_outer_match_even_when_source_where_rejects_it() {
    let mut case = saved_case();
    case.present = bit(0, 0) | bit(0, 2);
    case.files = bit(0, 2);
    case.patterns[0] = 6; // */* selects the inner file
    case.extra_patterns[0] = 14;
    case.excludes[0] = 0;
    case.types[0] = 1;
    case.roots[0] = 1;
    case.patterns[2] = 0; // * finds its containing directory
    case.extra_patterns[2] = 14;
    case.excludes[2] = 0;
    case.types[2] = 0;
    case.roots[2] = 1;
    case.source_where[2] = 1; // absent TAG rejects positive c, but not protect c
    case.first = 0;
    case.union = false;
    case.exclude_c = false;
    case.protect_c = true;

    let yaml = profiles(&case);
    let indented = yaml
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let config_yaml = format!("version: 1\nprofiles:\n{indented}\nprotect: [c]\n");
    let config = config::parse(
        Path::new("/tmp/general-reference.yaml"),
        config_yaml.as_bytes(),
    )
    .expect("saved config");
    let mut tier =
        Tier::select(&expression(&case), &config.library, None, 1000).expect("saved selection");
    tier.protections =
        executor::bind_protections(&config, 1000, false).expect("saved protection binding");
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
    assert!(self::expected(&case).is_empty(), "oracle fixture changed");
    assert!(
        plan.plan
            .iter()
            .any(|entry| entry.candidate == Path::new("/tmp/sel/a0/x0")
                && entry.reason == Some(Reason::Protected)),
        "raw outer protection did not cover the inner candidate: {plan:#?}"
    );
}

#[test]
fn mixed_marker_filter_complements_the_whole_known_conjunction() {
    let mut case = saved_case();
    case.present = bit(0, 0) | bit(0, 1);
    case.files = 0;
    case.tagged = bit(0, 0);
    case.flagged = bit(0, 1);
    case.patterns[0] = 0;
    case.extra_patterns[0] = 14;
    case.excludes[0] = 0;
    case.types[0] = 0;
    case.roots[0] = 1;
    case.first = 0;
    case.union = false;
    case.exclude_c = false;
    case.protect_c = false;

    for (factor, selected) in [(5, "/tmp/sel/a0"), (6, "/tmp/sel/b0")] {
        case.factor[0] = factor;
        let specs = parse_profiles(&profiles(&case)).expect("saved profiles");
        let library = Library::with_user_profiles(&specs).expect("saved library");
        let request = Request {
            path: PathBuf::from("/tmp"),
            min_free: parse_size("100%").unwrap(),
            target_free: None,
            timeout: Some(Duration::from_secs(600)),
            tally_budget: None,
            tier: Tier::select(&expression(&case), &library, None, 1000).expect("saved selection"),
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
        let only_selected = BTreeSet::from([PathBuf::from(selected)]);
        assert_eq!(
            self::expected(&case),
            only_selected,
            "oracle fixture changed"
        );
        assert_eq!(actual, only_selected, "{plan:#?}");
    }
}

#[test]
fn unknown_marker_type_passes_neither_filter_polarity_or_source_where() {
    let mut case = saved_case();
    case.present = bit(0, 0) | bit(0, 1);
    case.files = 0;
    case.tagged = bit(0, 0);
    case.unknown_tag = bit(0, 0);
    case.patterns[0] = 0; // both outer directories
    case.extra_patterns[0] = 14;
    case.excludes[0] = 0;
    case.types[0] = 0;
    case.roots[0] = 1;
    case.first = 0;
    case.union = false;
    case.exclude_c = false;

    for (source_where, factor, admitted) in [
        (0, 1, false), // has TAG
        (0, 2, true),  // !has TAG
        (0, 3, true),  // has_not TAG
        (0, 4, false), // !has_not TAG
        (1, 0, false), // source has TAG
        (2, 0, true),  // source has_not TAG
    ] {
        case.source_where[0] = source_where;
        case.factor[0] = factor;
        let specs = parse_profiles(&profiles(&case)).expect("saved profiles");
        let library = Library::with_user_profiles(&specs).expect("saved library");
        let request = Request {
            path: PathBuf::from("/tmp"),
            min_free: parse_size("100%").unwrap(),
            target_free: None,
            timeout: Some(Duration::from_secs(600)),
            tally_budget: None,
            tier: Tier::select(&expression(&case), &library, None, 1000).expect("selection"),
        };
        let model = model(&case);
        let host = ModelHost::new(&model).expect("saved model");
        inject_unknown_tags(&host, &case);
        let plan = executor::dry_run(&host, &request, &[1000], &mut |_| {});
        assert!(plan.errors.is_empty(), "{case:?}\n{plan:#?}");
        let actual = plan
            .plan
            .iter()
            .filter(|entry| entry.reason.is_none())
            .map(|entry| entry.candidate.clone())
            .collect::<BTreeSet<_>>();
        let expected = if admitted {
            BTreeSet::from([PathBuf::from("/tmp/sel/b0")])
        } else {
            BTreeSet::new()
        };
        assert_eq!(
            expected,
            self::expected(&case),
            "oracle fixture changed: {case:?}"
        );
        assert_eq!(actual, expected, "{case:?}\n{plan:#?}");
    }
}

#[test]
fn uncertain_negated_source_discovery_stops_instead_of_selecting() {
    let mut case = saved_case();
    case.present = bit(0, 0);
    case.files = 0;
    case.tagged = bit(0, 0);
    case.unknown_tag = bit(0, 0);
    case.patterns[0] = 0; // the outer directory is a positive candidate
    case.extra_patterns[0] = 14;
    case.excludes[0] = 0;
    case.types[0] = 0;
    case.roots[0] = 1;
    case.patterns[2] = 6; // */* reaches the marker with unknown type
    case.extra_patterns[2] = 14;
    case.excludes[2] = 0;
    case.types[2] = 0;
    case.roots[2] = 1;
    case.first = 0;
    case.union = false;
    case.exclude_c = true;

    let specs = parse_profiles(&profiles(&case)).expect("saved profiles");
    let library = Library::with_user_profiles(&specs).expect("saved library");
    let request = Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").unwrap(),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select(&expression(&case), &library, None, 1000).expect("selection"),
    };
    let model = model(&case);
    let host = ModelHost::new(&model).expect("saved model");
    inject_unknown_tags(&host, &case);
    let plan = executor::dry_run(&host, &request, &[1000], &mut |_| {});
    assert!(
        plan.errors
            .iter()
            .any(|error| error.message.contains("!profile:c discovery uncertain")),
        "{plan:#?}"
    );
    assert!(
        plan.plan.iter().all(|entry| entry.reason.is_some()),
        "uncertain negation admitted a candidate: {plan:#?}"
    );
}

#[test]
fn unknown_type_below_selected_directory_refuses_whole_tree() {
    let mut case = saved_case();
    case.present = bit(0, 1) | bit(0, 4);
    case.files = 0;
    case.tagged = bit(0, 4);
    case.unknown_tag = bit(0, 4);
    case.digits = [0, 1, 0, 0];
    case.patterns[0] = 2; // b* selects outer b1, not its child
    case.extra_patterns[0] = 14;
    case.excludes[0] = 0;
    case.types[0] = 0;
    case.roots[0] = 1;
    case.first = 0;
    case.union = false;
    case.exclude_c = false;

    let specs = parse_profiles(&profiles(&case)).expect("saved profiles");
    let library = Library::with_user_profiles(&specs).expect("saved library");
    let request = Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").unwrap(),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select(&expression(&case), &library, None, 1000).expect("selection"),
    };
    let model = model(&case);
    let host = ModelHost::new(&model).expect("saved model");
    inject_unknown_tags(&host, &case);
    let plan = executor::dry_run(&host, &request, &[1000], &mut |_| {});
    assert!(plan.errors.is_empty(), "{plan:#?}");
    assert!(self::expected(&case).is_empty(), "oracle fixture changed");
    assert!(
        plan.plan
            .iter()
            .any(|entry| entry.candidate == Path::new("/tmp/sel/b1")
                && entry.reason == Some(Reason::UnsupportedFs)),
        "unknown nested entry did not refuse outer candidate: {plan:#?}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("DISKGC_PROPERTY_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(32),
        rng_seed: RngSeed::Fixed(0x16_2027), failure_persistence: None, ..ProptestConfig::default()
    })]

    #[test]
    fn generated_names_roots_types_and_exclusions_match_reference(
        present in 0u16..4096, files in 0u16..4096,
        tagged in 0u16..4096, flagged in 0u16..4096,
        unknown_tag in 0u16..4096, unknown_flag in 0u16..4096,
        digits in prop::array::uniform4(0u8..3),
        patterns in prop::array::uniform3(0u8..14),
        extra_patterns in prop::array::uniform3(0u8..15),
        excludes in prop::array::uniform3(0u8..4),
        types in prop::array::uniform3(0u8..3),
        roots in prop::array::uniform3(1u8..4),
        source_where in prop::array::uniform3(0u8..6),
        first in 0usize..2, second in 0usize..2,
        factor in prop::array::uniform2(0u8..7),
        exclude_c in any::<bool>(), protect_c in any::<bool>(), union in any::<bool>(),
        grouped in any::<bool>(),
    ) {
        // The grammar requires a factor after a group (no bare parentheses).
        let grouped = union && grouped && (factor[0] != 0 || exclude_c);
        let factor = if grouped { [factor[0]; 2] } else { factor };
        // A negated or protected profile treats incomplete discovery as a
        // run error. Saved boundaries check fail-closed behavior separately;
        // this property compares eligible sets only for complete discovery.
        let unknown_tag = if exclude_c || protect_c { 0 } else { unknown_tag };
        let unknown_flag = if exclude_c || protect_c { 0 } else { unknown_flag };
        let case = Case { present, files, tagged, flagged, unknown_tag, unknown_flag,
            digits, patterns, extra_patterns, excludes, types,
            roots, source_where, first, second, factor, exclude_c, protect_c, union, grouped };
        prop_assume!((0..3).all(|source| sample_and_exclusion(&case, source).is_some()));
        let yaml = profiles(&case);
        let expr = expression(&case);
        let model = model(&case);
        let replay = format!("case: {case:?}\nprofiles:\n{yaml}\nselect: {expr}\n{}", model.to_yaml());
        let indented = yaml.lines().map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n");
        let protect = if protect_c { "protect: [c]\n" } else { "" };
        let config_yaml = format!("version: 1\nprofiles:\n{indented}\n{protect}");
        let config = config::parse(Path::new("/tmp/general-reference.yaml"), config_yaml.as_bytes())
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let mut tier = Tier::select(&expr, &config.library, None, 1000)
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        if protect_c {
            tier.protections = executor::bind_protections(&config, 1000, false)
                .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        }
        let request = Request { path: PathBuf::from("/tmp"), min_free: parse_size("100%").unwrap(),
            target_free: None, timeout: Some(Duration::from_secs(600)), tally_budget: None, tier };
        let host = ModelHost::new(&model).map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        inject_unknown_tags(&host, &case);
        let plan = executor::dry_run(&host, &request, &[1000], &mut |_| {});
        prop_assert!(plan.errors.is_empty(), "{plan:#?}\n{replay}");
        let actual = plan.plan.iter().filter(|entry| entry.reason.is_none())
            .map(|entry| entry.candidate.clone()).collect::<BTreeSet<_>>();
        prop_assert_eq!(actual, expected(&case), "{:#?}\n{}", plan, replay);
    }
}
