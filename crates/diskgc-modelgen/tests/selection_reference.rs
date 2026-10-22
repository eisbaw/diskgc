//! Integrated selection oracle: generated YAML profiles and host trees go
//! through Tier binding, filesystem discovery, and the ModelHost dry run.
//! The oracle uses a fixed table of path shapes, not the product matcher.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::executor::{self, ProtectedKind, ProtectedSource, Request, Tier};
use diskgc_core::fs_kind::Find;
use diskgc_core::profile::{EntryType, Library, Where, parse_profiles};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::{Bytes, DirSpec, FileSpec, HostModel, NodeSpec};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

const PATHS: [&str; 7] = ["a0", "b0", "c0", "a0/x0", "a0/y0", "b0/x0", "b0/y0"];
const NAMES: [&str; 3] = ["a", "b", "c"];
const EXAMPLES: [&str; 7] = ["a0", "b0", "a0/x0", "a0/x0", "a0", "a0/x0", "a0"];

fn patterns(shape: u8) -> &'static [&'static str] {
    match shape {
        0 => &["a*"],
        1 => &["b*"],
        2 => &["*/x*"],
        3 => &["*/*"],
        4 => &["*"],
        5 => &["a*/*"],
        6 => &["*", "*/*"],
        _ => unreachable!(),
    }
}

/// Independent finite-set meaning of the seven generated path shapes.
fn matches(shape: u8, path: &str) -> bool {
    let (parent, child) = path
        .split_once('/')
        .map_or((path, None), |(p, c)| (p, Some(c)));
    match shape {
        0 => child.is_none() && parent.starts_with('a'),
        1 => child.is_none() && parent.starts_with('b'),
        2 => child.is_some_and(|name| name.starts_with('x')),
        3 => child.is_some(),
        4 => child.is_none(),
        5 => parent.starts_with('a') && child.is_some(),
        6 => true,
        _ => unreachable!(),
    }
}

fn profile_yaml(shapes: [u8; 3], source_has_tag: [bool; 3]) -> String {
    let mut yaml = String::new();
    for i in 0..3 {
        let paths = patterns(shapes[i])
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ");
        yaml.push_str(&format!(
            "- name: {}\n  find: {{roots: [/tmp/sel], paths: [{}], type: dir}}\n  action: delete\n  examples: {{match: [/tmp/sel/{}]}}\n",
            NAMES[i], paths, EXAMPLES[shapes[i] as usize]
        ));
        if source_has_tag[i] {
            yaml.push_str("  where: {has: TAG}\n");
        }
    }
    yaml.push_str(
        "- name: tagged\n  where: {has: TAG}\n- name: untagged\n  where: {has_not: TAG}\n",
    );
    yaml
}

fn model(present: u8, tagged: u8) -> HostModel {
    let mut model = diskgc_model::load::parse_yaml(
        "host_model: 1\nclock: 2026-10-15T03:00:00Z\nmounts:\n  - {id: 1, at: /, fs: ext4, dev: '254:1', size: 100G}\n  - {id: 30, at: /tmp, fs: tmpfs, dev: '0:42', size: 1G, parent: 1}\n",
    ).expect("base model");
    model.tree.insert(
        PathBuf::from("/run/diskgc/lock"),
        NodeSpec::File(FileSpec {
            uid: Some(0),
            gid: Some(0),
            ..FileSpec::default()
        }),
    );
    let mut root = DirSpec::default();
    for (parent, path) in PATHS.iter().enumerate().take(3) {
        if present & (1 << parent) == 0 {
            continue;
        }
        let mut dir = candidate_dir(tagged & (1 << parent) != 0);
        for (child, child_path) in PATHS.iter().enumerate().skip(3) {
            let Some((owner, leaf)) = child_path.split_once('/') else {
                unreachable!()
            };
            if owner == *path && present & (1 << child) != 0 {
                dir.entries.insert(
                    leaf.to_owned(),
                    NodeSpec::Dir(candidate_dir(tagged & (1 << child) != 0)),
                );
            }
        }
        root.entries.insert((*path).to_owned(), NodeSpec::Dir(dir));
    }
    model
        .tree
        .insert(PathBuf::from("/tmp/sel"), NodeSpec::Dir(root));
    model
}

fn candidate_dir(tagged: bool) -> DirSpec {
    let mut dir = DirSpec::default();
    dir.entries.insert(
        "payload".to_owned(),
        NodeSpec::File(FileSpec {
            size: Some(Bytes(4096)),
            ..FileSpec::default()
        }),
    );
    if tagged {
        dir.entries
            .insert("TAG".to_owned(), NodeSpec::File(FileSpec::default()));
    }
    dir
}

fn exists(present: u8, i: usize) -> bool {
    present & (1 << i) != 0
        && (i < 3
            || PATHS[i].split_once('/').is_some_and(|(p, _)| {
                PATHS
                    .iter()
                    .take(3)
                    .position(|name| *name == p)
                    .is_some_and(|parent| present & (1 << parent) != 0)
            }))
}

/// `!profile:` and protect ignore source `where` and stop at raw outer
/// matches. Positive discovery instead descends past rejected matches.
fn raw(shapes: [u8; 3], present: u8, source: usize) -> BTreeSet<usize> {
    (0..PATHS.len())
        .filter(|&i| {
            exists(present, i)
                && matches(shapes[source], PATHS[i])
                && !PATHS[i]
                    .split_once('/')
                    .is_some_and(|(parent, _)| matches(shapes[source], parent))
        })
        .collect()
}

fn discovered(
    shapes: [u8; 3],
    source_has_tag: [bool; 3],
    present: u8,
    tagged: u8,
    source: usize,
) -> BTreeSet<usize> {
    let admits = |i: usize| !source_has_tag[source] || tagged & (1 << i) != 0;
    (0..PATHS.len())
        .filter(|&i| {
            exists(present, i)
                && matches(shapes[source], PATHS[i])
                && admits(i)
                && !PATHS[i].split_once('/').is_some_and(|(parent, _)| {
                    PATHS
                        .iter()
                        .take(3)
                        .position(|name| *name == parent)
                        .is_some_and(|j| matches(shapes[source], parent) && admits(j))
                })
        })
        .collect()
}

fn overlaps(a: usize, b: usize) -> bool {
    a == b
        || PATHS[a]
            .strip_prefix(PATHS[b])
            .is_some_and(|tail| tail.starts_with('/'))
        || PATHS[b]
            .strip_prefix(PATHS[a])
            .is_some_and(|tail| tail.starts_with('/'))
}

fn factor(choice: u8) -> &'static str {
    match choice {
        0 => "",
        1 => " && filter:tagged",
        2 => " && !filter:tagged",
        3 => " && filter:untagged",
        4 => " && !filter:untagged",
        _ => unreachable!(),
    }
}

fn admits(choice: u8, tagged: bool) -> bool {
    match choice {
        0 => true,
        1 | 4 => tagged,
        2 | 3 => !tagged,
        _ => unreachable!(),
    }
}

fn expression(
    first: usize,
    second: usize,
    factor_a: u8,
    factor_b: u8,
    exclude: bool,
    union: bool,
) -> String {
    let a = format!(
        "profile:{}{}{}",
        NAMES[first],
        factor(factor_a),
        if exclude { " && !profile:c" } else { "" }
    );
    if !union {
        return a;
    }
    let b = format!("profile:{}{}", NAMES[second], factor(factor_b));
    let paren = |term: String| {
        if term.contains(" && ") {
            format!("({term})")
        } else {
            term
        }
    };
    format!("{} || {}", paren(a), paren(b))
}

/// One generated selection scenario. Keeping the input together also makes
/// the oracle's set expression explicit at each property call site.
struct Scenario {
    shapes: [u8; 3],
    source_has_tag: [bool; 3],
    present: u8,
    tagged: u8,
    first: usize,
    second: usize,
    factor_a: u8,
    factor_b: u8,
    exclude: bool,
    union: bool,
    protect: bool,
}

fn expected(scenario: &Scenario) -> BTreeSet<PathBuf> {
    let raws = (0..3)
        .map(|source| raw(scenario.shapes, scenario.present, source))
        .collect::<Vec<_>>();
    let found = (0..3)
        .map(|source| {
            discovered(
                scenario.shapes,
                scenario.source_has_tag,
                scenario.present,
                scenario.tagged,
                source,
            )
        })
        .collect::<Vec<_>>();
    let mut eligible = BTreeSet::new();
    for (source, choice, active, negated) in [
        (scenario.first, scenario.factor_a, true, scenario.exclude),
        (scenario.second, scenario.factor_b, scenario.union, false),
    ] {
        if !active {
            continue;
        }
        for &i in &found[source] {
            let has_tag = scenario.tagged & (1 << i) != 0;
            if !admits(choice, has_tag) {
                continue;
            }
            if negated && raws[2].iter().any(|&j| overlaps(i, j)) {
                continue;
            }
            eligible.insert(i);
        }
    }
    if scenario.protect {
        eligible.retain(|&i| !raws[2].iter().any(|&j| overlaps(i, j)));
    }
    eligible
        .into_iter()
        .map(|i| Path::new("/tmp/sel").join(PATHS[i]))
        .collect()
}

fn request(yaml: &str, expr: &str, shapes: [u8; 3], protect: bool) -> Result<Request, String> {
    let specs = parse_profiles(yaml).map_err(|e| e.to_string())?;
    let library = Library::with_user_profiles(&specs).map_err(|e| e.to_string())?;
    let mut tier = Tier::select(expr, &library, None, 1000)?;
    if protect {
        tier.protections.push(ProtectedSource {
            profile: "c".to_owned(),
            roots: vec![PathBuf::from("/tmp/sel")],
            kind: ProtectedKind::Fs(Box::new(Find::new(
                &patterns(shapes[2])
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect::<Vec<_>>(),
                EntryType::Dir,
                &Where::default(),
                1000,
            ))),
        });
    }
    Ok(Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").unwrap(),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier,
    })
}

fn runtime(
    model: &HostModel,
    yaml: &str,
    expr: &str,
    shapes: [u8; 3],
    protect: bool,
) -> Result<BTreeSet<PathBuf>, String> {
    let request = request(yaml, expr, shapes, protect)?;
    let host = ModelHost::new(model).map_err(|e| e.to_string())?;
    let report = executor::dry_run(&host, &request, &[1000], &mut |_| {});
    if !report.errors.is_empty() {
        return Err(format!("plan errors: {:?}", report.errors));
    }
    Ok(report
        .plan
        .into_iter()
        .filter(|p| p.reason.is_none())
        .map(|p| p.candidate)
        .collect())
}

fn acted(
    model: &HostModel,
    yaml: &str,
    expr: &str,
    shapes: [u8; 3],
    protect: bool,
) -> Result<BTreeSet<PathBuf>, String> {
    let request = request(yaml, expr, shapes, protect)?;
    let host = ModelHost::new(model).map_err(|e| e.to_string())?;
    let report = executor::ensure(&host, &request, &[1000], &mut |_| {});
    if !report.errors.is_empty() {
        return Err(format!("reclaim errors: {:?}", report.errors));
    }
    Ok(report.actions.into_iter().map(|a| a.candidate).collect())
}

/// Shrunk vector from the generated property: `where` rejects the outer
/// pattern match, so positive discovery reaches its tagged child. The same
/// raw outer match still excludes that child for `!profile:` and protect.
#[test]
fn rejected_outer_where_allows_nested_positive_but_still_excludes_raw_overlap() {
    let fixture =
        include_str!("../../diskgc-model/tests/regressions/task16-rejected-outer-where.yaml");
    let model = diskgc_model::load::parse_yaml(fixture).expect("saved model");
    let shapes = [6, 1, 4];
    let profiles = profile_yaml(shapes, [true, false, true]);
    let nested = BTreeSet::from([PathBuf::from("/tmp/sel/b0/y0")]);
    assert_eq!(
        runtime(&model, &profiles, "profile:a", shapes, false).unwrap(),
        nested
    );
    assert!(
        runtime(&model, &profiles, "profile:a && !profile:c", shapes, false)
            .unwrap()
            .is_empty()
    );
    assert!(
        runtime(&model, &profiles, "profile:a", shapes, true)
            .unwrap()
            .is_empty()
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("DISKGC_PROPERTY_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(32),
        rng_seed: RngSeed::Fixed(0x16_2026), failure_persistence: None, ..ProptestConfig::default()
    })]

    #[test]
    fn generated_profiles_expressions_and_trees_match_integrated_reference(
        present in 0u8..128, tagged in 0u8..128,
        shapes in prop::array::uniform3(0u8..7),
        source_has_tag in prop::array::uniform3(any::<bool>()),
        first in 0usize..3, second in 0usize..3,
        factor_a in 0u8..5, factor_b in 0u8..5,
        exclude in any::<bool>(), union in any::<bool>(), protect in any::<bool>(),
    ) {
        let model = model(present, tagged);
        let yaml = profile_yaml(shapes, source_has_tag);
        let expr = expression(first, second, factor_a, factor_b, exclude, union);
        let actual = runtime(&model, &yaml, &expr, shapes, protect)
            .map_err(|e| TestCaseError::fail(format!("{e}\nprofiles:\n{yaml}\nselection: {expr}\n{}", model.to_yaml())))?;
        let expected = expected(&Scenario {
            shapes, source_has_tag, present, tagged, first, second,
            factor_a, factor_b, exclude, union, protect,
        });
        prop_assert_eq!(actual, expected, "profiles:\n{}\nselection: {}\n{}", yaml, expr, model.to_yaml());
    }

    #[test]
    fn runtime_negation_and_protect_only_narrow_and_union_commutes(
        present in 0u8..128, tagged in 0u8..128,
        shapes in prop::array::uniform3(0u8..7),
        source_has_tag in prop::array::uniform3(any::<bool>()),
        a in 0usize..3, b in 0usize..3, choice in 0u8..5,
    ) {
        let model = model(present, tagged);
        let yaml = profile_yaml(shapes, source_has_tag);
        let base = expression(a, b, choice, 0, false, false);
        let excluded = expression(a, b, choice, 0, true, false);
        let ab = expression(a, b, choice, 0, false, true);
        let ba = expression(b, a, 0, choice, false, true);
        let selected = |expr: &str, protect| runtime(&model, &yaml, expr, shapes, protect)
            .map_err(|e| TestCaseError::fail(format!("{e}\nprofiles:\n{yaml}\nselection: {expr}\n{}", model.to_yaml())));
        let ordinary = selected(&base, false)?;
        prop_assert!(selected(&excluded, false)?.is_subset(&ordinary));
        prop_assert!(selected(&base, true)?.is_subset(&ordinary));
        prop_assert_eq!(selected(&ab, false)?, selected(&ba, false)?);
    }

    #[test]
    fn stable_reclaim_actions_stay_within_reference_eligible_set(
        present in 0u8..128, tagged in 0u8..128,
        shapes in prop::array::uniform3(0u8..7),
        source_has_tag in prop::array::uniform3(any::<bool>()),
        a in 0usize..3, b in 0usize..3,
        factor_a in 0u8..5, factor_b in 0u8..5,
        exclude in any::<bool>(), union in any::<bool>(), protect in any::<bool>(),
    ) {
        let model = model(present, tagged);
        let yaml = profile_yaml(shapes, source_has_tag);
        let expr = expression(a, b, factor_a, factor_b, exclude, union);
        let expected = expected(&Scenario {
            shapes, source_has_tag, present, tagged, first: a, second: b,
            factor_a, factor_b, exclude, union, protect,
        });
        let actions = acted(&model, &yaml, &expr, shapes, protect)
            .map_err(|e| TestCaseError::fail(format!("{e}\nprofiles:\n{yaml}\nselection: {expr}\n{}", model.to_yaml())))?;
        prop_assert!(actions.is_subset(&expected),
            "action outside reference eligible set: actions {:?}, eligible {:?}\nprofiles:\n{}\nselection: {}\n{}",
            actions, expected, yaml, expr, model.to_yaml());
        let non_overlapping = expected.iter().all(|a| expected.iter().all(|b|
            a == b || !(a.starts_with(b) || b.starts_with(a))));
        if non_overlapping {
            prop_assert_eq!(actions, expected,
                "stable non-overlapping run omitted eligible action\nprofiles:\n{}\nselection: {}\n{}",
                yaml, expr, model.to_yaml());
        }
    }
}
