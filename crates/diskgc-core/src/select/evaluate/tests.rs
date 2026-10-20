use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use proptest::prelude::*;

use super::*;
use crate::fs_kind::{Find, Pattern, path_matches};
use crate::kind::{Kind, PredicateName};
use crate::profile::{self, Body, Library, Roots};
use crate::select::{Catalog, Entry, Factor, compile};

const SOURCES: [&str; 3] = ["a", "b", "c"];
const FILTERS: [&str; 2] = ["f", "g"];

struct CatalogOfThree;

impl Catalog for CatalogOfThree {
    type Action = &'static str;

    fn lookup(&self, name: &str) -> Option<Entry<'_, Self::Action>> {
        static DELETE: &str = "delete";
        if SOURCES.contains(&name) {
            Some(Entry::Source {
                kind: Kind::Fs,
                action: &DELETE,
            })
        } else if FILTERS.contains(&name) {
            Some(Entry::Filter {
                predicates: vec![PredicateName::IdleFor],
            })
        } else {
            None
        }
    }
}

/// A small arbitrary directory tree. `parent[i] < i`, so each node has a
/// unique path and an ancestry chain. A source's `where` can remove raw
/// matches; negation and protection must still see the raw set.
#[derive(Debug, Clone)]
struct TreeFacts {
    paths: Vec<PathBuf>,
    parent: Vec<Option<usize>>,
    raw: [[bool; 8]; 3],
    source: [[bool; 8]; 3],
    filters: [[FilterVerdict; 8]; 2],
}

impl TreeFacts {
    fn index(&self, path: &Path) -> usize {
        self.paths
            .iter()
            .position(|p| p == path)
            .expect("tree path")
    }

    fn source_index(name: &str) -> usize {
        SOURCES.iter().position(|n| *n == name).expect("source")
    }

    fn filter_index(name: &str) -> usize {
        FILTERS.iter().position(|n| *n == name).expect("filter")
    }

    fn ancestor_of(&self, ancestor: usize, mut child: usize) -> bool {
        loop {
            if ancestor == child {
                return true;
            }
            let Some(parent) = self.parent[child] else {
                return false;
            };
            child = parent;
        }
    }

    fn raw_overlaps(&self, profile: &str, candidate: usize) -> bool {
        let p = Self::source_index(profile);
        (0..self.paths.len()).any(|i| {
            self.raw[p][i] && (self.ancestor_of(i, candidate) || self.ancestor_of(candidate, i))
        })
    }
}

impl Facts for TreeFacts {
    type Error = std::convert::Infallible;

    fn source_matches(&self, profile: &str, candidate: &Path) -> Result<bool, Self::Error> {
        Ok(self.source[Self::source_index(profile)][self.index(candidate)])
    }

    fn filter_verdict(&self, filter: &str, candidate: &Path) -> Result<FilterVerdict, Self::Error> {
        Ok(self.filters[Self::filter_index(filter)][self.index(candidate)])
    }

    fn profile_overlaps(&self, profile: &str, candidate: &Path) -> Result<bool, Self::Error> {
        Ok(self.raw[Self::source_index(profile)]
            .iter()
            .enumerate()
            .take(self.paths.len())
            .any(|(i, hit)| *hit && overlaps(&self.paths[i], candidate)))
    }

    fn protection_overlaps(
        &self,
        protection: &Protection,
        candidate: &Path,
    ) -> Result<bool, Self::Error> {
        let p = Self::source_index(&protection.profile);
        Ok(self.raw[p]
            .iter()
            .enumerate()
            .take(self.paths.len())
            .any(|(i, hit)| {
                *hit && (protection.roots.is_empty()
                    || protection
                        .roots
                        .iter()
                        .any(|root| self.paths[i].starts_with(root)))
                    && overlaps(&self.paths[i], candidate)
            }))
    }
}

/// Set algebra is deliberately separate from the term-by-term evaluator.
/// The tree's parent links determine overlap without using `overlaps`.
fn reference(
    selection: &crate::select::Checked,
    facts: &TreeFacts,
    protect: &[Protection],
) -> BTreeSet<usize> {
    let universe: BTreeSet<usize> = (0..facts.paths.len()).collect();
    let protected: BTreeSet<usize> = universe
        .iter()
        .copied()
        .filter(|i| {
            protect.iter().any(|p| {
                let source = TreeFacts::source_index(&p.profile);
                (0..facts.paths.len()).any(|j| {
                    facts.raw[source][j]
                        && (p.roots.is_empty()
                            || p.roots.iter().any(|root| facts.paths[j].starts_with(root)))
                        && (facts.ancestor_of(j, *i) || facts.ancestor_of(*i, j))
                })
            })
        })
        .collect();
    let mut result = BTreeSet::new();
    for term in &selection.expr().terms {
        let source = TreeFacts::source_index(&term.source);
        let mut term_set: BTreeSet<usize> = universe
            .iter()
            .copied()
            .filter(|i| facts.source[source][*i])
            .collect();
        for factor in &term.factors {
            let accepted: BTreeSet<usize> = universe
                .iter()
                .copied()
                .filter(|i| match factor {
                    Factor::Filter(name) => {
                        facts.filters[TreeFacts::filter_index(name)][*i] == FilterVerdict::Matches
                    }
                    Factor::NotFilter(name) => {
                        facts.filters[TreeFacts::filter_index(name)][*i]
                            == FilterVerdict::DoesNotMatch
                    }
                    Factor::NotProfile(name) => !facts.raw_overlaps(name, *i),
                })
                .collect();
            term_set = term_set.intersection(&accepted).copied().collect();
        }
        result.extend(term_set);
    }
    result.difference(&protected).copied().collect()
}

fn actual(
    selection: &crate::select::Checked,
    facts: &TreeFacts,
    protect: &[Protection],
) -> BTreeSet<usize> {
    (0..facts.paths.len())
        .filter(|i| eligible(selection, &facts.paths[*i], protect, facts).unwrap())
        .collect()
}

fn arb_facts() -> impl Strategy<Value = TreeFacts> {
    (
        prop::collection::vec(0usize..8, 1..9),
        prop::collection::vec(any::<bool>(), 24),
        prop::collection::vec(any::<bool>(), 24),
        prop::collection::vec(0u8..3, 16),
    )
        .prop_map(|(parents, raw_bits, where_bits, verdicts)| {
            let mut paths: Vec<PathBuf> = Vec::new();
            let mut parent = Vec::new();
            for (i, proposed) in parents.iter().enumerate() {
                let p = (i > 0).then(|| proposed % i);
                let base = p.map_or_else(|| PathBuf::from("/tmp"), |n| paths[n].clone());
                paths.push(base.join(format!("n{i}")));
                parent.push(p);
            }
            let mut raw = [[false; 8]; 3];
            let mut source = [[false; 8]; 3];
            let mut filters = [[FilterVerdict::Unknown; 8]; 2];
            for p in 0..3 {
                for i in 0..paths.len() {
                    raw[p][i] = raw_bits[p * 8 + i];
                    source[p][i] = raw[p][i] && where_bits[p * 8 + i];
                }
            }
            for f in 0..2 {
                for i in 0..paths.len() {
                    filters[f][i] = match verdicts[f * 8 + i] {
                        0 => FilterVerdict::Matches,
                        1 => FilterVerdict::DoesNotMatch,
                        _ => FilterVerdict::Unknown,
                    };
                }
            }
            TreeFacts {
                paths,
                parent,
                raw,
                source,
                filters,
            }
        })
}

fn expression(source: usize, other: usize, filter: usize, flags: u8) -> String {
    let mut text = format!("profile:{}", SOURCES[source]);
    if flags & 1 != 0 {
        text.push_str(&format!(" && filter:{}", FILTERS[filter]));
    }
    if flags & 2 != 0 {
        text.push_str(&format!(" && !filter:{}", FILTERS[1 - filter]));
    }
    if flags & 4 != 0 {
        text.push_str(&format!(" && !profile:{}", SOURCES[other]));
    }
    text
}

fn union_member(text: String) -> String {
    if text.contains(" && ") {
        format!("({text})")
    } else {
        text
    }
}

proptest! {
    #[test]
    fn eligible_set_equals_reference_for_random_trees(
        facts in arb_facts(),
        first in 0usize..3,
        second in 0usize..3,
        excluded in 0usize..3,
        filter in 0usize..2,
        flags in 0u8..8,
        other_flags in 0u8..8,
        protected in prop::option::of(0usize..3),
    ) {
        let text = format!("{} || {}",
            union_member(expression(first, excluded, filter, flags)),
            union_member(expression(second, excluded, 1-filter, other_flags)));
        let checked = compile(&text, &CatalogOfThree).unwrap();
        let protect = protected.map(|p| vec![Protection::from(SOURCES[p])]).unwrap_or_default();
        prop_assert_eq!(actual(&checked, &facts, &protect), reference(&checked, &facts, &protect));
    }

    #[test]
    fn narrowing_never_adds_and_union_order_does_not_matter(
        facts in arb_facts(),
        a in 0usize..3,
        b in 0usize..3,
        filter in 0usize..2,
        protected in 0usize..3,
    ) {
        let base = compile(&format!("profile:{}", SOURCES[a]), &CatalogOfThree).unwrap();
        let negated = compile(&format!("profile:{} && !filter:{} && !profile:{}",
            SOURCES[a], FILTERS[filter], SOURCES[b]), &CatalogOfThree).unwrap();
        let base_set = actual(&base, &facts, &[]);
        let narrowed = actual(&negated, &facts, &[]);
        let guarded = actual(&base, &facts, &[Protection::from(SOURCES[protected])]);
        prop_assert!(narrowed.is_subset(&base_set));
        prop_assert!(guarded.is_subset(&base_set));

        let ab = compile(&format!("profile:{} || profile:{}", SOURCES[a], SOURCES[b]), &CatalogOfThree).unwrap();
        let ba = compile(&format!("profile:{} || profile:{}", SOURCES[b], SOURCES[a]), &CatalogOfThree).unwrap();
        prop_assert_eq!(actual(&ab, &facts, &[]), actual(&ba, &facts, &[]));
    }
}

#[test]
fn unknown_filter_passes_neither_polarity() {
    let mut facts = arb_sample();
    facts.raw[0][0] = true;
    facts.source[0][0] = true;
    facts.filters[0][0] = FilterVerdict::Unknown;
    for text in ["profile:a && filter:f", "profile:a && !filter:f"] {
        let checked = compile(text, &CatalogOfThree).unwrap();
        assert!(!eligible(&checked, &facts.paths[0], &[], &facts).unwrap());
    }
}

#[test]
fn exclusions_and_protection_ignore_a_sources_where() {
    let mut facts = arb_sample();
    facts.raw[0][0] = true;
    facts.source[0][0] = true;
    facts.raw[1][0] = true;
    facts.source[1][0] = false; // source b's own `where` rejected it
    let candidate = &facts.paths[0];
    let plain = compile("profile:a", &CatalogOfThree).unwrap();
    let excluded = compile("profile:a && !profile:b", &CatalogOfThree).unwrap();
    assert!(eligible(&plain, candidate, &[], &facts).unwrap());
    assert!(!eligible(&excluded, candidate, &[], &facts).unwrap());
    assert!(!eligible(&plain, candidate, &["b".into()], &facts).unwrap());
}

fn arb_sample() -> TreeFacts {
    TreeFacts {
        paths: vec![PathBuf::from("/tmp/a")],
        parent: vec![None],
        raw: [[false; 8]; 3],
        source: [[false; 8]; 3],
        filters: [[FilterVerdict::Unknown; 8]; 2],
    }
}

struct PatternFacts {
    source: Vec<PathBuf>,
    excluded: Vec<PathBuf>,
}

fn found_by(library: &Library, name: &str, path: &Path) -> bool {
    let Body::Source(source) = &library.get(name).expect("test profile exists").body else {
        panic!("{name} is not a source")
    };
    let Roots::Fixed(roots) = &source.roots else {
        panic!("{name} needs roots")
    };
    let roots: Vec<&Path> = roots.iter().map(Path::new).collect();
    let find = Find::of(source, 1000).expect("fs profile");
    path_matches(Some(&roots), &find.patterns, path)
}

impl Facts for PatternFacts {
    type Error = std::convert::Infallible;

    fn source_matches(&self, _: &str, candidate: &Path) -> Result<bool, Self::Error> {
        Ok(self.source.iter().any(|p| p == candidate))
    }

    fn filter_verdict(&self, _: &str, _: &Path) -> Result<FilterVerdict, Self::Error> {
        unreachable!("this selection has no filters")
    }

    fn profile_overlaps(&self, _: &str, candidate: &Path) -> Result<bool, Self::Error> {
        Ok(self.excluded.iter().any(|p| overlaps(p, candidate)))
    }

    fn protection_overlaps(
        &self,
        protection: &Protection,
        candidate: &Path,
    ) -> Result<bool, Self::Error> {
        Ok(self.excluded.iter().any(|p| {
            (protection.roots.is_empty() || protection.roots.iter().any(|root| p.starts_with(root)))
                && overlaps(p, candidate)
        }))
    }
}

#[test]
fn tmp_entry_excludes_entire_claude_session_container() {
    let user = profile::parse_profiles(include_str!("../test-profiles.yaml")).unwrap();
    let library = Library::with_user_profiles(&user).unwrap();
    let checked = compile("profile:tmp-entry && !profile:claude-code-tmp", &library).unwrap();
    let candidates = [
        PathBuf::from("/tmp/claude-1000"),
        PathBuf::from("/tmp/other"),
    ];
    let claude_session = PathBuf::from("/tmp/claude-1000/-project/session");
    let facts = PatternFacts {
        source: candidates
            .iter()
            .filter(|p| found_by(&library, "tmp-entry", p))
            .cloned()
            .collect(),
        excluded: [claude_session]
            .into_iter()
            .filter(|p| found_by(&library, "claude-code-tmp", p))
            .collect(),
    };
    assert!(!eligible(&checked, &candidates[0], &[], &facts).unwrap());
    assert!(eligible(&checked, &candidates[1], &[], &facts).unwrap());
    assert!(
        !eligible(
            &compile("profile:tmp-entry", &library).unwrap(),
            &candidates[0],
            &["claude-code-tmp".into()],
            &facts
        )
        .unwrap()
    );
}

#[test]
fn overlap_is_component_aware_and_symmetric() {
    assert!(overlaps(Path::new("/tmp/a"), Path::new("/tmp/a/b")));
    assert!(overlaps(Path::new("/tmp/a/b"), Path::new("/tmp/a")));
    assert!(overlaps(Path::new("/tmp/a"), Path::new("/tmp/a")));
    assert!(!overlaps(Path::new("/tmp/a"), Path::new("/tmp/ab")));
}

struct DistinctRootFacts {
    raw_b: [PathBuf; 2],
    tier_root: PathBuf,
}

impl Facts for DistinctRootFacts {
    type Error = std::convert::Infallible;

    fn source_matches(&self, _: &str, _: &Path) -> Result<bool, Self::Error> {
        Ok(true)
    }

    fn filter_verdict(&self, _: &str, _: &Path) -> Result<FilterVerdict, Self::Error> {
        unreachable!("no filters in this selection")
    }

    fn profile_overlaps(&self, profile: &str, candidate: &Path) -> Result<bool, Self::Error> {
        assert_eq!(profile, "b");
        Ok(self
            .raw_b
            .iter()
            .any(|p| p.starts_with(&self.tier_root) && overlaps(p, candidate)))
    }

    fn protection_overlaps(
        &self,
        protection: &Protection,
        candidate: &Path,
    ) -> Result<bool, Self::Error> {
        assert_eq!(protection.profile, "b");
        Ok(self.raw_b.iter().any(|p| {
            protection.roots.iter().any(|root| p.starts_with(root)) && overlaps(p, candidate)
        }))
    }
}

#[test]
fn protection_roots_are_independent_of_negated_profile_tier_roots() {
    let facts = DistinctRootFacts {
        raw_b: [
            PathBuf::from("/workspace/cache"),
            PathBuf::from("/important/cache"),
        ],
        tier_root: PathBuf::from("/workspace"),
    };
    let protected = [Protection {
        profile: "b".to_owned(),
        roots: vec![PathBuf::from("/important")],
    }];
    let union = compile("profile:a || (profile:c && !profile:b)", &CatalogOfThree).unwrap();
    assert!(eligible(&union, Path::new("/workspace/cache"), &protected, &facts).unwrap());
    assert!(!eligible(&union, Path::new("/important/cache"), &protected, &facts).unwrap());
    let negated = compile("profile:c && !profile:b", &CatalogOfThree).unwrap();
    assert!(!eligible(&negated, Path::new("/workspace/cache"), &[], &facts).unwrap());
    assert!(eligible(&negated, Path::new("/important/cache"), &[], &facts).unwrap());
}

struct FailedProtection;

impl Facts for FailedProtection {
    type Error = &'static str;

    fn source_matches(&self, _: &str, _: &Path) -> Result<bool, Self::Error> {
        Ok(true)
    }

    fn filter_verdict(&self, _: &str, _: &Path) -> Result<FilterVerdict, Self::Error> {
        Ok(FilterVerdict::Matches)
    }

    fn profile_overlaps(&self, _: &str, _: &Path) -> Result<bool, Self::Error> {
        Err("protected discovery failed")
    }

    fn protection_overlaps(&self, _: &Protection, _: &Path) -> Result<bool, Self::Error> {
        Err("protected discovery failed")
    }
}

#[test]
fn failed_protection_and_exclusion_propagate() {
    let candidate = Path::new("/tmp/a");
    let plain = compile("profile:a", &CatalogOfThree).unwrap();
    assert_eq!(
        eligible(&plain, candidate, &["b".into()], &FailedProtection),
        Err("protected discovery failed")
    );
    let excluded = compile("profile:a && !profile:b", &CatalogOfThree).unwrap();
    assert_eq!(
        eligible(&excluded, candidate, &[], &FailedProtection),
        Err("protected discovery failed")
    );
}

#[test]
fn patterns_reach_only_declared_depths() {
    let pattern = Pattern::compile("project/.pytest_cache", 1000);
    let root = Path::new("/home/user");
    assert!(path_matches(
        Some(&[root]),
        std::slice::from_ref(&pattern),
        Path::new("/home/user/project/.pytest_cache")
    ));
    assert!(!path_matches(
        Some(&[root]),
        std::slice::from_ref(&pattern),
        Path::new("/home/user/project/sub/.pytest_cache")
    ));
}
