//! Integrated selection oracle: generated YAML profiles and host trees go
//! through Tier binding, filesystem discovery, and the ModelHost dry run.
//! The oracle uses a fixed table of path shapes, not the product matcher.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::config;
use diskgc_core::executor::{self, ProtectedKind, ProtectedSource, Request, Tier};
use diskgc_core::fs_kind::Find;
use diskgc_core::profile::{EntryType, Library, Where, parse_profiles};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::{Bytes, DirSpec, FileSpec, HostModel, NodeSpec, Op};
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

/// A second, independent input family: two direct-child markers and two
/// ages on each existing directory. A recent nested child makes its parent
/// recent as well because the idle measure covers the whole candidate tree.
fn evidence_model(present: u8, has_a: u8, has_b: u8, old: u8) -> HostModel {
    let mut model = model(0, 0);
    let Some(NodeSpec::Dir(root)) = model.tree.get_mut(Path::new("/tmp/sel")) else {
        unreachable!("base model has /tmp/sel")
    };
    for (parent, name) in PATHS.iter().enumerate().take(3) {
        if !exists(present, parent) {
            continue;
        }
        let mut dir = evidence_candidate(
            has_a & (1 << parent) != 0,
            has_b & (1 << parent) != 0,
            old & (1 << parent) != 0,
        );
        for (child, path) in PATHS.iter().enumerate().skip(3) {
            let (owner, leaf) = path.split_once('/').expect("nested path");
            if owner == *name && exists(present, child) {
                dir.entries.insert(
                    leaf.to_owned(),
                    NodeSpec::Dir(evidence_candidate(
                        has_a & (1 << child) != 0,
                        has_b & (1 << child) != 0,
                        old & (1 << child) != 0,
                    )),
                );
            }
        }
        root.entries.insert((*name).to_owned(), NodeSpec::Dir(dir));
    }
    model
}

fn evidence_candidate(has_a: bool, has_b: bool, old: bool) -> DirSpec {
    let timestamp: diskgc_model::spec::WallTime = if old {
        "2026-10-01T00:00:00Z"
    } else {
        "2026-10-15T00:00:00Z"
    }
    .parse()
    .expect("fixed timestamp");
    let file = || FileSpec {
        mtime: Some(timestamp),
        ..FileSpec::default()
    };
    let mut dir = DirSpec::default();
    dir.attrs.mtime = Some(timestamp);
    dir.entries.insert(
        "payload".to_owned(),
        NodeSpec::File(FileSpec {
            size: Some(Bytes(4096)),
            ..file()
        }),
    );
    if has_a {
        dir.entries.insert("A".to_owned(), NodeSpec::File(file()));
    }
    if has_b {
        dir.entries.insert("B".to_owned(), NodeSpec::File(file()));
    }
    dir
}

fn evidence_profiles(shapes: [u8; 3], source_where: [u8; 3]) -> String {
    let mut yaml = String::new();
    for i in 0..3 {
        let paths = patterns(shapes[i])
            .iter()
            .map(|path| format!("\"{path}\""))
            .collect::<Vec<_>>()
            .join(", ");
        yaml.push_str(&format!(
            "- name: {}\n  find: {{roots: [/tmp/sel], paths: [{paths}], type: dir}}\n  action: delete\n  examples: {{match: [/tmp/sel/{}]}}\n",
            NAMES[i], EXAMPLES[shapes[i] as usize]
        ));
        match source_where[i] {
            0 => {}
            1 => yaml.push_str("  where: {has: A}\n"),
            2 => yaml.push_str("  where: {has_not: B}\n"),
            _ => unreachable!(),
        }
    }
    yaml.push_str(
        "- name: has-a\n  where: {has: A}\n- name: lacks-b\n  where: {has_not: B}\n- name: old-2d\n  where: {idle_for: 2d}\n- name: a-and-old\n  where: {has: A, idle_for: 2d}\n",
    );
    yaml
}

/// The nine expression factors are defined here independently of the
/// product parser and evaluator. Choices 7 and 8 are a conjunction and its
/// complement, rather than separate complements of its two predicates.
fn evidence_factor_text(choice: u8) -> &'static str {
    match choice {
        0 => "",
        1 => " && filter:has-a",
        2 => " && !filter:has-a",
        3 => " && filter:lacks-b",
        4 => " && !filter:lacks-b",
        5 => " && filter:old-2d",
        6 => " && !filter:old-2d",
        7 => " && filter:a-and-old",
        8 => " && !filter:a-and-old",
        _ => unreachable!(),
    }
}

fn evidence_factor_passes(choice: u8, has_a: bool, has_b: bool, old: bool) -> bool {
    match choice {
        0 => true,
        1 => has_a,
        2 => !has_a,
        3 => !has_b,
        4 => has_b,
        5 => old,
        6 => !old,
        7 => has_a && old,
        8 => !(has_a && old),
        _ => unreachable!(),
    }
}

fn evidence_expression(
    first: usize,
    second: usize,
    choices: [u8; 4],
    exclude: [bool; 2],
    union: bool,
) -> String {
    let term = |source: usize, a: u8, b: u8, exclude: bool| {
        format!(
            "profile:{}{}{}{}",
            NAMES[source],
            evidence_factor_text(a),
            evidence_factor_text(b),
            if exclude { " && !profile:c" } else { "" }
        )
    };
    let left = term(first, choices[0], choices[1], exclude[0]);
    if !union {
        return left;
    }
    let right = term(second, choices[2], choices[3], exclude[1]);
    let member = |text: String| {
        if text.contains(" && ") {
            format!("({text})")
        } else {
            text
        }
    };
    format!("{} || {}", member(left), member(right))
}

#[derive(Debug, Clone, Copy)]
struct EvidenceScenario {
    present: u8,
    has_a: u8,
    has_b: u8,
    old: u8,
    shapes: [u8; 3],
    source_where: [u8; 3],
    first: usize,
    second: usize,
    choices: [u8; 4],
    exclude: [bool; 2],
    union: bool,
    protect: bool,
}

fn evidence_source_admits(scenario: &EvidenceScenario, source: usize, i: usize) -> bool {
    match scenario.source_where[source] {
        0 => true,
        1 => scenario.has_a & (1 << i) != 0,
        2 => scenario.has_b & (1 << i) == 0,
        _ => unreachable!(),
    }
}

fn evidence_discovered(scenario: &EvidenceScenario, source: usize) -> BTreeSet<usize> {
    (0..PATHS.len())
        .filter(|&i| {
            exists(scenario.present, i)
                && matches(scenario.shapes[source], PATHS[i])
                && evidence_source_admits(scenario, source, i)
                && !PATHS[i].split_once('/').is_some_and(|(parent, _)| {
                    PATHS
                        .iter()
                        .take(3)
                        .position(|name| *name == parent)
                        .is_some_and(|j| {
                            matches(scenario.shapes[source], parent)
                                && evidence_source_admits(scenario, source, j)
                        })
                })
        })
        .collect()
}

/// An old candidate has no recent file or directory anywhere in its tree.
fn evidence_old(scenario: &EvidenceScenario, i: usize) -> bool {
    PATHS.iter().enumerate().all(|(j, path)| {
        !exists(scenario.present, j)
            || !(j == i
                || path
                    .strip_prefix(PATHS[i])
                    .is_some_and(|tail| tail.starts_with('/')))
            || scenario.old & (1 << j) != 0
    })
}

fn evidence_expected(scenario: &EvidenceScenario) -> BTreeSet<PathBuf> {
    let guard = raw(scenario.shapes, scenario.present, 2);
    let mut selected = BTreeSet::new();
    for (source, choices, exclude, active) in [
        (
            scenario.first,
            [scenario.choices[0], scenario.choices[1]],
            scenario.exclude[0],
            true,
        ),
        (
            scenario.second,
            [scenario.choices[2], scenario.choices[3]],
            scenario.exclude[1],
            scenario.union,
        ),
    ] {
        if !active {
            continue;
        }
        for i in evidence_discovered(scenario, source) {
            let a = scenario.has_a & (1 << i) != 0;
            let b = scenario.has_b & (1 << i) != 0;
            let old = evidence_old(scenario, i);
            if choices
                .iter()
                .all(|&choice| evidence_factor_passes(choice, a, b, old))
                && !((exclude || scenario.protect)
                    && guard.iter().any(|&guarded| overlaps(i, guarded)))
            {
                selected.insert(Path::new("/tmp/sel").join(PATHS[i]));
            }
        }
    }
    selected
}

/// Exercise the user config's protection binder, rather than constructing a
/// ProtectedSource in the test. Source `c` may reject its own candidate via
/// `where`; protection still uses its raw, outer structural match.
fn evidence_request(profiles: &str, selection: &str, protect: bool) -> Result<Request, String> {
    let indented_profiles = profiles
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let yaml = format!("version: 1\nprofiles:\n{indented_profiles}\nprotect: [c]\n");
    let config = config::parse(Path::new("/tmp/evidence-config.yaml"), yaml.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut tier = Tier::select(selection, &config.library, None, 1000)?;
    if protect {
        tier.protections =
            executor::bind_protections(&config, 1000, false).map_err(|error| error.to_string())?;
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

fn version_model(newest: usize, tagged: u8) -> HostModel {
    let mut model = model(0, 0);
    let mut versions = DirSpec::default();
    for i in 0..3 {
        let mut dir = candidate_dir(tagged & (1 << i) != 0);
        // Distinct directory mtimes make `keep_newest: 1` unambiguous.
        let day = if i == newest { 4 } else { i + 1 };
        dir.attrs.mtime = Some(format!("2026-10-0{day}T00:00:00Z").parse().unwrap());
        versions.entries.insert(format!("v{i}"), NodeSpec::Dir(dir));
    }
    let Some(NodeSpec::Dir(root)) = model.tree.get_mut(Path::new("/tmp/sel")) else {
        unreachable!("generated model has /tmp/sel directory")
    };
    root.entries
        .insert("versions".to_owned(), NodeSpec::Dir(versions));
    model
}

const VERSION_PROFILES: &str = r#"
- name: open-version
  find: {roots: [/tmp/sel], paths: ["versions/*"], type: dir}
  action: delete
  examples: {match: [/tmp/sel/versions/v0]}
- name: guard-version
  find: {roots: [/tmp/sel], paths: ["versions/*"], type: dir}
  where: {has: TAG, keep_newest: 1}
  action: delete
  examples: {match: [/tmp/sel/versions/v0]}
- name: filter-guard-version
  find: {roots: [/tmp/sel], paths: ["versions/*"], type: dir}
  where: {has: TAG}
  action: delete
  examples: {match: [/tmp/sel/versions/v0]}
- name: keep-one
  where: {keep_newest: 1}
"#;

const OWNER_CONFIG: &str = r#"version: 1
profiles:
  - name: owner-all
    find: {roots: required, paths: ["*"], type: dir}
    action: delete
    examples: {root: /tmp/owner-1000, match: [/tmp/owner-1000/cache]}
  - name: owner-guard
    find: {roots: required, paths: [cache], type: dir}
    where: {has: absent-marker}
    action: delete
    examples: {root: /tmp/owner-1000, match: [/tmp/owner-1000/cache]}
protect: [{profile: owner-guard, roots: ["/tmp/owner-{uid}"]}]
"#;

fn owner_model() -> HostModel {
    let mut model = model(0, 0);
    for uid in [1000, 1001] {
        let mut root = DirSpec::default();
        root.attrs.uid = Some(uid);
        for name in ["cache", "other"] {
            let mut candidate = DirSpec::default();
            candidate.attrs.uid = Some(uid);
            candidate.entries.insert(
                "payload".to_owned(),
                NodeSpec::File(FileSpec {
                    uid: Some(uid),
                    size: Some(Bytes(4096)),
                    ..FileSpec::default()
                }),
            );
            root.entries
                .insert(name.to_owned(), NodeSpec::Dir(candidate));
        }
        model.tree.insert(
            PathBuf::from(format!("/tmp/owner-{uid}")),
            NodeSpec::Dir(root),
        );
    }
    model
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

/// AC3: both sources are user YAML profiles. The narrow negated match is a
/// child of the broad candidate, so overlap must retain the whole container.
#[test]
fn user_profiles_keep_the_whole_claude_container_in_plan_and_reclaim() {
    let profiles = include_str!("fixtures/task16-claude-profiles.yaml");
    let model = diskgc_model::load::parse_yaml(include_str!(
        "../../diskgc-model/tests/regressions/task16-claude-container.yaml"
    ))
    .expect("saved host model");
    let specs = parse_profiles(profiles).expect("user profiles");
    let library = Library::with_user_profiles(&specs).expect("user library");
    let selection = "profile:tmp-entry && !profile:claude-sessions";
    let request = Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").unwrap(),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select(selection, &library, None, 1000).expect("supported selection"),
    };
    let planned_host = ModelHost::new(&model).expect("saved host loads");
    let plan = executor::dry_run(&planned_host, &request, &[1000], &mut |_| {});
    assert!(plan.errors.is_empty(), "{plan:#?}");
    assert!(
        plan.plan.iter().any(|entry| {
            entry.candidate == Path::new("/tmp/claude-1000") && entry.reason == Some("excluded")
        }),
        "{plan:#?}"
    );
    assert!(
        plan.plan.iter().any(|entry| {
            entry.candidate == Path::new("/tmp/outside") && entry.reason.is_none()
        }),
        "{plan:#?}"
    );

    let reclaim_host = ModelHost::new(&model).expect("saved host loads");
    let report = executor::ensure(&reclaim_host, &request, &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{report:#?}");
    assert_eq!(report.actions.len(), 1, "{report:#?}");
    assert_eq!(report.actions[0].candidate, Path::new("/tmp/outside"));
    assert!(reclaim_host.stat_path(Path::new("/tmp/outside")).is_none());
    for retained in [
        "/tmp/claude-1000",
        "/tmp/claude-1000/-project/session-1/transcript",
        "/tmp/claude-1000/unrelated/note",
    ] {
        assert!(
            reclaim_host.stat_path(Path::new(retained)).is_some(),
            "{retained}: {report:#?}"
        );
    }
    assert!(
        reclaim_host.trace().iter().all(|call| {
            call.op != Op::Unlinkat
                || !call
                    .path
                    .as_ref()
                    .is_some_and(|p| p.starts_with("/tmp/claude-1000"))
        }),
        "Claude container was unlinked: {report:#?}"
    );
}

#[test]
fn rejected_outer_source_where_does_not_weaken_term_negation_or_config_protect() {
    let scenario = EvidenceScenario {
        present: 0b000_1011, // a0, b0, a0/x0
        has_a: 0b000_1000,   // only the nested candidate has A
        has_b: 0b000_1000,   // only the nested candidate has B
        old: 0b000_1011,
        shapes: [6, 1, 6],
        source_where: [1, 2, 1], // guard c rejects outer a0 and b0
        first: 0,
        second: 1,
        choices: [7, 4, 3, 8], // mixed positive and complemented evidence
        exclude: [true, false],
        union: true,
        protect: false,
    };
    let model = evidence_model(
        scenario.present,
        scenario.has_a,
        scenario.has_b,
        scenario.old,
    );
    let profiles = evidence_profiles(scenario.shapes, scenario.source_where);
    let expression = evidence_expression(
        scenario.first,
        scenario.second,
        scenario.choices,
        scenario.exclude,
        scenario.union,
    );
    let replay = format!(
        "scenario: {scenario:?}\nprofiles:\n{profiles}\nselection: {expression}\n{}",
        model.to_yaml()
    );
    assert_eq!(
        evidence_discovered(&scenario, 0),
        BTreeSet::from([3]),
        "rejected outer match must expose the nested positive candidate\n{replay}"
    );
    assert_eq!(
        evidence_discovered(&scenario, 2),
        BTreeSet::from([3]),
        "{replay}"
    );
    assert_eq!(
        raw(scenario.shapes, scenario.present, 2),
        BTreeSet::from([0, 1]),
        "{replay}"
    );

    for protect in [false, true] {
        let scenario = EvidenceScenario {
            protect,
            ..scenario
        };
        let expected = evidence_expected(&scenario);
        let request = evidence_request(&profiles, &expression, protect)
            .unwrap_or_else(|error| panic!("{error}\n{replay}"));
        let plan_host = ModelHost::new(&model).expect("valid model");
        let plan = executor::dry_run(&plan_host, &request, &[1000], &mut |_| {});
        assert!(plan.errors.is_empty(), "{plan:#?}\n{replay}");
        let eligible = plan
            .plan
            .iter()
            .filter(|entry| entry.reason.is_none())
            .map(|entry| entry.candidate.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(eligible, expected, "{plan:#?}\n{replay}");
        let run_host = ModelHost::new(&model).expect("valid model");
        let report = executor::ensure(&run_host, &request, &[1000], &mut |_| {});
        assert!(report.errors.is_empty(), "{report:#?}\n{replay}");
        let actions = report
            .actions
            .iter()
            .map(|action| action.candidate.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(actions, expected, "{report:#?}\n{replay}");
        assert!(
            run_host.stat_path(Path::new("/tmp/sel/a0/x0")).is_some(),
            "{report:#?}\n{replay}"
        );
    }
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
    fn interacting_source_marker_and_age_filters_match_independent_reference(
        present in 0u8..128,
        has_a in 0u8..128,
        has_b in 0u8..128,
        old in 0u8..128,
        shapes in prop::array::uniform3(0u8..7),
        source_where in prop::array::uniform3(0u8..3),
        first in 0usize..3,
        second in 0usize..3,
        choices in prop::array::uniform4(0u8..9),
        exclude in prop::array::uniform2(any::<bool>()),
        union in any::<bool>(),
        protect in any::<bool>(),
    ) {
        let scenario = EvidenceScenario {
            present, has_a, has_b, old, shapes, source_where,
            first, second, choices, exclude, union, protect,
        };
        let model = evidence_model(present, has_a, has_b, old);
        let yaml = evidence_profiles(shapes, source_where);
        let expr = evidence_expression(first, second, choices, exclude, union);
        let replay = format!(
            "scenario: {scenario:?}\nprofiles:\n{yaml}\nselection: {expr}\n{}",
            model.to_yaml()
        );
        let expected = evidence_expected(&scenario);
        let request = evidence_request(&yaml, &expr, protect)
            .map_err(|error| TestCaseError::fail(format!("{error}\n{replay}")))?;
        let plan_host = ModelHost::new(&model)
            .map_err(|error| TestCaseError::fail(format!("{error}\n{replay}")))?;
        let plan = executor::dry_run(&plan_host, &request, &[1000], &mut |_| {});
        prop_assert!(plan.errors.is_empty(), "{plan:#?}\n{replay}");
        let actual = plan.plan.iter().filter(|entry| entry.reason.is_none())
            .map(|entry| entry.candidate.clone()).collect::<BTreeSet<_>>();
        prop_assert_eq!(actual, expected.clone(), "{}", replay);

        let run_host = ModelHost::new(&model)
            .map_err(|error| TestCaseError::fail(format!("{error}\n{replay}")))?;
        let report = executor::ensure(&run_host, &request, &[1000], &mut |_| {});
        prop_assert!(report.errors.is_empty(), "{report:#?}\n{replay}");
        let actions = report.actions.iter().map(|action| action.candidate.clone())
            .collect::<BTreeSet<_>>();
        prop_assert!(actions.is_subset(&expected),
            "reclaim acted outside the independent eligible set: {:?}\n{}", actions, replay);
        let non_overlapping = expected.iter().all(|a| expected.iter().all(|b|
            a == b || !(a.starts_with(b) || b.starts_with(a))));
        if non_overlapping {
            prop_assert_eq!(actions, expected,
                "stable non-overlapping reclaim disagreed with reference\n{}", replay);
        }
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
        let replay = format!("profiles:\n{yaml}\nbase: {base}\n{}", model.to_yaml());
        prop_assert!(selected(&excluded, false)?.is_subset(&ordinary), "{}", replay);
        prop_assert!(selected(&base, true)?.is_subset(&ordinary), "{}", replay);
        prop_assert_eq!(selected(&ab, false)?, selected(&ba, false)?, "{}", replay);
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

    #[test]
    fn unknown_marker_type_admits_neither_filter_polarity(
        candidate in 0usize..3,
        choice in 1u8..5,
        source_has_tag in any::<bool>(),
    ) {
        let bit = 1 << candidate;
        let model = model(bit, bit);
        let yaml = profile_yaml([4, 1, 1], [source_has_tag, false, false]);
        let selection = format!("profile:a{}", factor(choice));
        let marker = Path::new("/tmp/sel").join(PATHS[candidate]).join("TAG");
        let replay = format!("profiles:\n{yaml}\nselection: {selection}\nomit statx type at: {}\n{}",
            marker.display(), model.to_yaml());
        let request = request(&yaml, &selection, [4, 1, 1], false)
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        // The independent oracle's evidence is Unknown. `has` and
        // `has_not`, positive or complemented, both require known evidence.
        let plan_host = ModelHost::new(&model)
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        plan_host.omit_statx_type_at(marker.clone());
        let plan = executor::dry_run(&plan_host, &request, &[1000], &mut |_| {});
        prop_assert!(plan.errors.is_empty(), "{selection}: {plan:#?}\n{}", replay);
        prop_assert!(!plan.plan.iter().any(|entry| entry.candidate == marker.parent().unwrap()
            && entry.reason.is_none()), "unknown marker selected: {selection}: {plan:#?}\n{}", replay);

        let run_host = ModelHost::new(&model)
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        run_host.omit_statx_type_at(marker);
        let report = executor::ensure(&run_host, &request, &[1000], &mut |_| {});
        prop_assert!(report.errors.is_empty(), "{selection}: {report:#?}\n{}", replay);
        prop_assert!(report.actions.is_empty(), "unknown marker reclaimed: {selection}: {report:#?}\n{}", replay);
        prop_assert_eq!(run_host.trace_count(Op::Unlinkat), 0, "{}", replay);
    }

    #[test]
    fn raw_retention_guard_vetoes_shared_union_candidate(
        newest in 0usize..3,
        tagged in 0u8..8,
        guard_as_filter in any::<bool>(),
        reverse in any::<bool>(),
    ) {
        let model = version_model(newest, tagged);
        let guarded = if guard_as_filter {
            "(profile:filter-guard-version && filter:keep-one)"
        } else {
            "profile:guard-version"
        };
        let selection = if reverse {
            format!("{guarded} || profile:open-version")
        } else {
            format!("profile:open-version || {guarded}")
        };
        let replay = format!("profiles:\n{VERSION_PROFILES}\nselection: {selection}\n{}", model.to_yaml());
        let specs = parse_profiles(VERSION_PROFILES)
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let library = Library::with_user_profiles(&specs)
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let request = Request {
            path: PathBuf::from("/tmp"), min_free: parse_size("100%").unwrap(),
            target_free: None, timeout: Some(Duration::from_secs(600)),
            tally_budget: None,
            tier: Tier::select(&selection, &library, None, 1000)
                .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?,
        };
        let newest_path = PathBuf::from(format!("/tmp/sel/versions/v{newest}"));
        let expected: BTreeSet<PathBuf> = (0..3).filter(|&i| i != newest)
            .map(|i| PathBuf::from(format!("/tmp/sel/versions/v{i}"))).collect();
        // The independent rule ranks all three structural guard matches.
        // A missing TAG on the newest may reject the guard source's own
        // action admission, but cannot disable its retention veto.
        let plan_host = ModelHost::new(&model)
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let plan = executor::dry_run(&plan_host, &request, &[1000], &mut |_| {});
        prop_assert!(plan.errors.is_empty(), "{selection}: {plan:#?}\n{}", replay);
        let actual: BTreeSet<_> = plan.plan.iter().filter(|p| p.reason.is_none())
            .map(|p| p.candidate.clone()).collect();
        prop_assert_eq!(actual, expected.clone(), "{}: {:#?}\n{}", selection, plan, replay);
        prop_assert!(plan.plan.iter().any(|p| p.candidate == newest_path
            && p.reason == Some("newest_version")), "{selection}: {plan:#?}\n{}", replay);

        let run_host = ModelHost::new(&model)
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let report = executor::ensure(&run_host, &request, &[1000], &mut |_| {});
        prop_assert!(report.errors.is_empty(), "{selection}: {report:#?}\n{}", replay);
        let actions: BTreeSet<_> = report.actions.iter().map(|a| a.candidate.clone()).collect();
        prop_assert_eq!(actions, expected, "{}: {:#?}\n{}", selection, report, replay);
        prop_assert!(run_host.stat_path(&newest_path).is_some(), "{selection}: {report:#?}\n{}", replay);
        prop_assert!(run_host.trace().iter().all(|call| call.op != Op::Unlinkat
            || !call.path.as_ref().is_some_and(|path| path.starts_with(&newest_path))),
            "newest version unlinked: {selection}: {report:#?}\n{}", replay);
    }

    #[test]
    fn uid_roots_and_config_bound_protect_follow_the_selected_owner(
        uid in prop_oneof![Just(1000u32), Just(1001u32)],
        exclude in any::<bool>(), protect in any::<bool>(),
    ) {
        let model = owner_model();
        let selection = if exclude {
            "profile:owner-all && !profile:owner-guard"
        } else {
            "profile:owner-all"
        };
        let replay = format!("config:\n{OWNER_CONFIG}\nselection: {selection}\nuid: {uid}\nprotect: {protect}\n{}",
            model.to_yaml());
        let config = config::parse(Path::new("/tmp/owner-config.yaml"), OWNER_CONFIG.as_bytes())
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let roots = ["/tmp/owner-{uid}".to_owned()];
        let mut tier = Tier::select(selection, &config.library, Some(&roots), uid)
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        if protect {
            tier.protections = executor::bind_protections(&config, uid, false)
                .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        }
        let request = Request {
            path: PathBuf::from("/tmp"), min_free: parse_size("100%").unwrap(),
            target_free: None, timeout: Some(Duration::from_secs(600)),
            tally_budget: None, tier,
        };
        let root = PathBuf::from(format!("/tmp/owner-{uid}"));
        let expected: BTreeSet<_> = ["cache", "other"].into_iter()
            .filter(|name| *name == "other" || !(exclude || protect))
            .map(|name| root.join(name)).collect();
        let plan_host = ModelHost::new(&model)
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let plan = executor::dry_run(&plan_host, &request, &[uid], &mut |_| {});
        prop_assert!(plan.errors.is_empty(), "uid {uid}: {plan:#?}\n{}", replay);
        let eligible: BTreeSet<_> = plan.plan.iter().filter(|entry| entry.reason.is_none())
            .map(|entry| entry.candidate.clone()).collect();
        prop_assert_eq!(eligible, expected.clone(), "uid {}: {:#?}\n{}", uid, plan, replay);

        let run_host = ModelHost::new(&model)
            .map_err(|e| TestCaseError::fail(format!("{e}\n{replay}")))?;
        let report = executor::ensure(&run_host, &request, &[uid], &mut |_| {});
        prop_assert!(report.errors.is_empty(), "uid {uid}: {report:#?}\n{}", replay);
        let actions: BTreeSet<_> = report.actions.iter().map(|action| action.candidate.clone()).collect();
        prop_assert_eq!(actions, expected, "uid {}: {:#?}\n{}", uid, report, replay);
        let other_uid = if uid == 1000 { 1001 } else { 1000 };
        let other_cache = PathBuf::from(format!("/tmp/owner-{other_uid}/cache"));
        prop_assert!(run_host.stat_path(&other_cache).is_some(), "{}", replay);
    }
}
