//! The selection grammar against docs/reference.md and the real profile
//! library (the built-ins plus test profiles).
//!
//! The doc is the table: [`doc_examples_normalise_as_documented`] and
//! [`doc_messages_are_the_messages`] read its two tables, and
//! [`every_doc_expression_is_classified`] makes sure no other expression in
//! the doc (prose, the example config, the CLI example) goes untested.

use std::collections::BTreeMap;
use std::fmt;

use proptest::prelude::*;

use super::*;
use crate::doc::{code, reference_md, section, table};
use crate::profile::{self, Library};

// ---- The catalog: the real built-in library, plus test profiles loaded
// through the same loader as user profiles, plus stand-ins for the library
// profiles whose kinds have no profile schema yet.

/// User profiles for the tests, stand-ins among them (the file's header
/// says which and why).
const TEST_PROFILES: &str = include_str!("test-profiles.yaml");

/// STAND-INS for kinds that have no profile schema yet.
const NOT_BUILT_IN_KINDS: &[(&str, Kind, &str)] = &[("nix-garbage", Kind::NixStore, "nix GC")];

/// A source's action: a real one, or a stand-in's description.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TestAction {
    Real(profile::Action),
    StandIn(&'static str),
}

impl fmt::Display for TestAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Real(action) => action.fmt(f),
            Self::StandIn(action) => f.write_str(action),
        }
    }
}

struct TestCatalog {
    library: Library,
    /// Every source, the library's included, with its action as a
    /// [`TestAction`] (the trait lends actions out by reference).
    sources: BTreeMap<String, (Kind, TestAction)>,
}

impl Catalog for TestCatalog {
    type Action = TestAction;

    fn names(&self) -> Vec<&str> {
        self.library
            .profiles()
            .map(|p| p.name.as_str())
            .chain(self.sources.keys().map(String::as_str))
            .collect()
    }

    fn lookup(&self, name: &str) -> Option<Entry<'_, TestAction>> {
        if let Some((kind, action)) = self.sources.get(name) {
            return Some(Entry::Source {
                kind: *kind,
                action,
            });
        }
        match self.library.lookup(name)? {
            Entry::Filter { predicates } => Some(Entry::Filter { predicates }),
            Entry::Source { .. } => unreachable!("every library source is in `sources`"),
        }
    }
}

fn catalog() -> TestCatalog {
    let user = profile::parse_profiles(TEST_PROFILES).unwrap_or_else(|e| panic!("{e}"));
    let library = Library::with_user_profiles(&user).unwrap_or_else(|e| panic!("{e}"));
    let mut sources = BTreeMap::new();
    for name in library.profiles().map(|p| p.name.clone()) {
        if let Some(Entry::Source { kind, action }) = library.lookup(&name) {
            sources.insert(name, (kind, TestAction::Real(action.clone())));
        }
    }
    for (name, kind, action) in NOT_BUILT_IN_KINDS {
        assert!(
            library.get(name).is_none(),
            "`{name}` is in the library now: remove its stand-in"
        );
        sources.insert((*name).to_owned(), (*kind, TestAction::StandIn(action)));
    }
    TestCatalog { library, sources }
}

/// The normalised text of an accepted expression.
fn normalised(text: &str) -> String {
    match compile(text, &catalog()) {
        Ok(checked) => checked.expr().to_string(),
        Err(e) => panic!("`{text}` was rejected: {e}"),
    }
}

/// The message(s) of a rejected expression, without the
/// `invalid selection …` prefix.
fn message(text: &str) -> String {
    match compile(text, &catalog()) {
        Ok(checked) => panic!("`{text}` was accepted as `{}`", checked.expr()),
        Err(SelectError::Syntax(e)) => e.problem.to_string(),
        Err(SelectError::Check(e)) => {
            let messages: Vec<String> = e.errors.iter().map(ToString::to_string).collect();
            messages.join("; ")
        }
    }
}

fn syntax(text: &str) -> Syntax {
    match parse(text) {
        Ok(expr) => panic!("`{text}` parsed as `{expr}`"),
        Err(e) => e.problem,
    }
}

fn check_errors(text: &str) -> Vec<CheckError> {
    let expr = parse(text).unwrap_or_else(|e| panic!("{e}"));
    match check(expr, &catalog()) {
        Ok(checked) => panic!("`{text}` passed as `{}`", checked.expr()),
        Err(e) => e.errors,
    }
}

fn term(source: &str, factors: Vec<Factor>) -> Term {
    Term {
        source: source.to_owned(),
        factors,
    }
}

fn filter(name: &str) -> Factor {
    Factor::Filter(name.to_owned())
}

// ---- docs/reference.md

/// The Selection grammar section of docs/reference.md.
fn grammar_section() -> String {
    section(&reference_md(), "### Selection grammar")
}

/// Keep actual selection examples in the PRD aligned with the parser too.
/// The Composition table contains syntax fragments, so it is excluded.
#[test]
fn prd_selection_expressions_are_classified() {
    let prd = include_str!("../../../../PRD.md");
    let found: Vec<&str> = prd
        .lines()
        .filter(|line| !line.starts_with('|'))
        .flat_map(|line| line.split('`').skip(1).step_by(2))
        .filter(|span| span.contains("profile:") && (span.contains("&&") || span.contains("||")))
        .collect();
    assert_eq!(
        found,
        ["(profile:node-compile-cache || profile:playwright-tmp) && filter:idle-3d"]
    );
    for expression in found {
        assert!(
            compile(expression, &catalog()).is_ok(),
            "invalid PRD selection `{expression}`"
        );
    }
}

/// Every inline code span in the section's prose (outside fences and
/// tables) that names a qualified profile.
fn prose_expressions(section: &str) -> Vec<String> {
    let mut prose = String::new();
    let mut fenced = false;
    for line in section.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
        } else if !fenced && !line.starts_with('|') {
            prose.push_str(line.trim());
            prose.push(' ');
        }
    }
    prose
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| span.contains("profile:") || span.contains("filter:"))
        .map(str::to_owned)
        .collect()
}

/// Every expression in the doc's code blocks: `select:` values in YAML and
/// `--select '…'` arguments.
fn code_block_expressions(doc: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut fenced = false;
    for line in doc.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            continue;
        }
        if let Some((_, value)) = line.split_once("select: ") {
            let value = value.split(" #").next().unwrap_or(value);
            found.push(value.trim().to_owned());
        }
        if let Some((_, rest)) = line.split_once("--select '") {
            found.push(rest.split('\'').next().unwrap_or(rest).to_owned());
        }
    }
    found
}

/// Every expression in the doc's code blocks, in order: the CI example,
/// the explicit /tmp Cargo tier, and the example config's tiers.
const CODE_BLOCK_EXPRESSIONS: &[&str] = &[
    "profile:cargo-target && filter:idle-1d",
    "profile:cargo-target && filter:idle-7d",
    "(profile:node-compile-cache || profile:zephyr-run) && filter:idle-3d",
    "profile:cargo-target && filter:idle-7d",
    "profile:docker-build-cache && filter:idle-7d",
    "profile:docker-dangling",
    "profile:chromium-tmp && filter:idle-2d",
    "profile:cargo-target && filter:idle-1h",
];

/// What the grammar section's prose expressions are. Every one must be
/// listed here, so a new example in the doc cannot go untested.
enum Prose {
    /// A complete expression, accepted, with its normalised form.
    Accepted(&'static str),
    /// A complete expression, rejected with this message.
    Rejected(&'static str),
    /// A piece of syntax, not a complete expression (`profile:NAME`).
    Fragment,
}

const PROSE: &[(&str, Prose)] = &[
    ("profile:", Prose::Fragment),
    ("filter:", Prose::Fragment),
    ("!profile:", Prose::Fragment),
    ("profile:NAME", Prose::Fragment),
    ("!filter:…", Prose::Fragment),
    ("!profile:…", Prose::Fragment),
    ("!profile:a && !profile:b", Prose::Fragment),
    ("profile:a", Prose::Fragment),
    ("profile: a", Prose::Fragment),
    ("filter:nix-garbage", Prose::Fragment),
    ("!profile:nix-garbage", Prose::Fragment),
    (
        "filter:pytest-tmp",
        Prose::Rejected(
            "`filter:pytest-tmp`: `pytest-tmp` is a source profile of kind fs, not a filter; \
             write `profile:pytest-tmp`",
        ),
    ),
    (
        "profile:a || profile:b && filter:f",
        Prose::Rejected(
            "`||` and `&&` cannot be mixed without parentheses; write \
             `(profile:a || profile:b) && filter:f` or `profile:a || (profile:b && filter:f)`",
        ),
    ),
    (
        "(profile:a || profile:b) && filter:f",
        Prose::Accepted("(profile:a && filter:f) || (profile:b && filter:f)"),
    ),
    (
        "(profile:a && filter:f) || (profile:b && filter:f)",
        Prose::Accepted("(profile:a && filter:f) || (profile:b && filter:f)"),
    ),
    (
        "!(profile:a || profile:b)",
        Prose::Rejected("`!` applies to one name, not a group: write `!profile:a && !profile:b`"),
    ),
];

/// AC #2: every row of the examples table parses, passes the static rules,
/// and normalises to its "Normalised" cell, which parses back to itself.
#[test]
fn doc_examples_normalise_as_documented() {
    let rows = table(&grammar_section(), "| Expression | Normalised |");
    for row in &rows {
        let (expr, want) = (code(&row[0]), code(&row[1]));
        assert_eq!(normalised(expr), want, "examples table row `{expr}`");
        assert_eq!(normalised(want), want, "normal form `{want}` is not stable");
    }
}

/// Every row of the messages table is rejected with exactly that message.
#[test]
fn doc_messages_are_the_messages() {
    let rows = table(&grammar_section(), "| Expression | Message |");
    for row in &rows {
        assert_eq!(
            message(code(&row[0])),
            row[1],
            "messages table row `{}`",
            row[0]
        );
    }
}

/// AC #2, the other half: no expression in the doc is untested. Prose
/// spans must be classified in [`PROSE`] (and behave as classified), and
/// every expression in a code block (the example config, the CLI example)
/// is accepted.
#[test]
fn every_doc_expression_is_classified() {
    let found = prose_expressions(&grammar_section());
    let missing: Vec<&String> = found
        .iter()
        .filter(|s| !PROSE.iter().any(|(p, _)| p == s))
        .collect();
    assert!(
        missing.is_empty(),
        "expressions in the grammar's prose missing from PROSE: {missing:?}"
    );
    let stale: Vec<&str> = PROSE
        .iter()
        .map(|(p, _)| *p)
        .filter(|p| !found.iter().any(|s| s == p))
        .collect();
    assert!(
        stale.is_empty(),
        "PROSE entries no longer in the doc: {stale:?}"
    );
    for (text, what) in PROSE {
        match what {
            Prose::Accepted(want) => assert_eq!(normalised(text), *want, "`{text}`"),
            Prose::Rejected(want) => assert_eq!(message(text), *want, "`{text}`"),
            Prose::Fragment => {}
        }
    }

    let blocks = code_block_expressions(&reference_md());
    assert_eq!(
        blocks, CODE_BLOCK_EXPRESSIONS,
        "the doc's code-block expressions"
    );
    for text in CODE_BLOCK_EXPRESSIONS {
        compile(text, &catalog()).unwrap_or_else(|e| panic!("code-block expression: {e}"));
    }
}

// ---- AC #1

#[test]
fn ac1_unparenthesised_mix_shows_both_readings() {
    let err = parse("profile:a || profile:b && filter:f").unwrap_err();
    assert_eq!(
        err.problem,
        Syntax::Mixed {
            readings: vec![
                "(profile:a || profile:b) && filter:f".to_owned(),
                "profile:a || (profile:b && filter:f)".to_owned(),
            ]
        }
    );
    assert_eq!(err.column, 24, "points at the `&&`");
    assert_eq!(
        err.to_string(),
        "invalid selection `profile:a || profile:b && filter:f` at column 24: `||` and `&&` \
         cannot be mixed without parentheses; write `(profile:a || profile:b) && filter:f` or \
         `profile:a || (profile:b && filter:f)`"
    );
}

#[test]
fn mixing_is_refused_in_every_position() {
    for text in [
        "profile:a && filter:f || profile:b",
        "profile:a || profile:b || profile:c && filter:f",
        "(profile:a || profile:b && filter:f)",
        "(profile:a && filter:f || profile:b)",
        "(profile:a && filter:f) || profile:b && filter:f",
        "((profile:a || profile:b && filter:f))",
    ] {
        assert!(
            matches!(syntax(text), Syntax::Mixed { .. }),
            "`{text}`: {:?}",
            syntax(text)
        );
    }
    // Only readings that parse are offered.
    assert_eq!(
        syntax("(profile:a && filter:f) || profile:b && filter:g"),
        Syntax::Mixed {
            readings: vec!["(profile:a && filter:f) || (profile:b && filter:g)".to_owned()]
        }
    );
    assert_eq!(
        syntax("profile:a && filter:f || profile:b && filter:g"),
        Syntax::Mixed {
            readings: vec!["(profile:a && filter:f) || (profile:b && filter:g)".to_owned()]
        }
    );
    assert_eq!(
        syntax("profile:a && filter:f || filter:g"),
        Syntax::FilterUnion
    );
    assert_eq!(
        syntax("profile:a && filter:f || !filter:g"),
        Syntax::FilterUnion
    );
}

// ---- Grammar: accepted forms and normalisation

#[test]
fn distribution_keeps_the_written_order() {
    let expr = parse("(profile:b || profile:a) && !filter:x && filter:y").unwrap();
    let factors = vec![Factor::NotFilter("x".into()), filter("y")];
    assert_eq!(
        expr.terms,
        vec![term("b", factors.clone()), term("a", factors)]
    );
}

#[test]
fn every_grammar_form_is_accepted() {
    for (text, want) in [
        ("profile:a", "profile:a"),
        ("profile:a && filter:f", "profile:a && filter:f"),
        ("profile:a || profile:b", "profile:a || profile:b"),
        (
            "(profile:a || profile:b) && filter:f",
            "(profile:a && filter:f) || (profile:b && filter:f)",
        ),
        (
            "(profile:a && filter:f) || profile:b",
            "(profile:a && filter:f) || profile:b",
        ),
        (
            "((profile:a || profile:b) && filter:f) || profile:c",
            "(profile:a && filter:f) || (profile:b && filter:f) || profile:c",
        ),
        (
            "profile:a&&!profile:b&&!filter:f",
            "profile:a && !profile:b && !filter:f",
        ),
        (
            "  ( profile:a\t|| profile:b )\n&& ! filter:f ",
            "(profile:a && !filter:f) || (profile:b && !filter:f)",
        ),
        ("profile:a_1 && filter:x-y9", "profile:a_1 && filter:x-y9"),
    ] {
        assert_eq!(parse(text).unwrap().to_string(), want, "`{text}`");
    }
}

/// User decision (task-7 review): every parenthesis has a purpose. Each
/// rejection suggests the spelling without the pointless parentheses.
#[test]
fn every_parenthesis_has_a_purpose() {
    let unnarrowed = |s: &str| Syntax::UnnarrowedGroup {
        suggestion: Some(s.into()),
    };
    let paren_one = |s: &str| Syntax::ParenOne {
        suggestion: Some(s.into()),
    };
    for (text, want) in [
        (
            "(profile:a || profile:b)",
            unnarrowed("profile:a || profile:b"),
        ),
        (
            "(profile:a || profile:b) || profile:c",
            unnarrowed("profile:a || profile:b || profile:c"),
        ),
        (
            "profile:c || (profile:a || profile:b) || profile:d",
            unnarrowed("profile:c || profile:a || profile:b || profile:d"),
        ),
        (
            "((profile:a || profile:b)) || profile:c",
            unnarrowed("profile:a || profile:b || profile:c"),
        ),
        (
            "((profile:a || profile:b))",
            unnarrowed("profile:a || profile:b"),
        ),
        ("(profile:a)", paren_one("profile:a")),
        (
            "(profile:a) && filter:f",
            paren_one("profile:a && filter:f"),
        ),
        (
            "profile:a || (profile:b)",
            paren_one("profile:a || profile:b"),
        ),
        (
            "((profile:a)) || profile:c",
            paren_one("profile:a || profile:c"),
        ),
        (
            "((profile:a) && filter:f) || profile:c",
            paren_one("(profile:a && filter:f) || profile:c"),
        ),
    ] {
        let got = syntax(text);
        assert_eq!(got, want, "`{text}`");
        for suggestion in got.suggestions() {
            parse(suggestion).unwrap_or_else(|e| panic!("`{text}` suggested: {e}"));
        }
    }
}

// ---- Grammar: one negative case per rule

#[test]
fn negation_applies_to_one_name() {
    assert_eq!(
        syntax("profile:a && !(profile:b || filter:c)"),
        Syntax::NegatedGroup {
            suggestion: Some("!profile:b && !filter:c".into())
        }
    );
    assert_eq!(
        syntax("profile:a && !(profile:b && filter:c)"),
        Syntax::NegatedGroup { suggestion: None }
    );
    assert_eq!(
        syntax("!(profile:a || profile:b)"),
        Syntax::NegatedGroup {
            suggestion: Some("!profile:a && !profile:b".into())
        }
    );
    assert_eq!(syntax("profile:a && !!filter:f"), Syntax::DoubleNegation);
    assert_eq!(syntax("!profile:a"), Syntax::NotStartsTerm);
    assert_eq!(
        syntax("profile:a || !profile:b"),
        Syntax::NotStartsTerm,
        "a union member is a term start"
    );
    assert_eq!(syntax("(profile:a || !profile:b)"), Syntax::NotInGroup);
}

#[test]
fn qualifiers_are_required_and_known() {
    assert_eq!(
        syntax("pytest-tmp && filter:f"),
        Syntax::Unqualified("pytest-tmp".into())
    );
    assert_eq!(
        syntax("profile:a && idle-3d"),
        Syntax::Unqualified("idle-3d".into())
    );
    assert_eq!(syntax("prof:a"), Syntax::UnknownQualifier("prof".into()));
    assert_eq!(syntax("profile: a"), Syntax::MissingName("profile".into()));
    assert_eq!(
        syntax("profile:a && filter:"),
        Syntax::MissingName("filter".into())
    );
    assert_eq!(syntax("profile:a.b"), Syntax::UnexpectedChar('.'));
    assert_eq!(syntax("profile:a # note"), Syntax::UnexpectedChar('#'));
    assert_eq!(syntax("profile:a & filter:f"), Syntax::SingleOperator('&'));
    assert_eq!(syntax("profile:a | profile:b"), Syntax::SingleOperator('|'));
}

#[test]
fn a_term_has_one_source_and_starts_with_it() {
    assert_eq!(
        syntax("profile:a && profile:b"),
        Syntax::TwoSources("b".into())
    );
    assert_eq!(
        syntax("(profile:a || profile:b) && profile:c"),
        Syntax::TwoSources("c".into())
    );
    assert_eq!(
        syntax("filter:f && profile:a"),
        Syntax::FilterStartsTerm("f".into())
    );
    assert_eq!(
        syntax("profile:a || filter:f"),
        Syntax::FilterStartsTerm("f".into())
    );
}

#[test]
fn parentheses_group_only_profiles_and_nest_once() {
    assert_eq!(
        syntax("(profile:a || filter:f) && filter:g"),
        Syntax::FilterInGroup("f".into())
    );
    assert_eq!(
        syntax("profile:a && (filter:f || filter:g)"),
        Syntax::GroupedFactor
    );
    assert_eq!(
        syntax("(profile:a || profile:b) || profile:c"),
        Syntax::UnnarrowedGroup {
            suggestion: Some("profile:a || profile:b || profile:c".into())
        }
    );
    assert_eq!(
        syntax("profile:c || (profile:a || profile:b) && filter:f"),
        Syntax::Mixed {
            readings: vec!["profile:c || ((profile:a || profile:b) && filter:f)".into()]
        },
        "a group followed by `&&` in a union is a mix; only the reading that parses is offered"
    );
    assert_eq!(
        syntax("(profile:a || profile:b) && filter:f || profile:c"),
        Syntax::Mixed {
            readings: vec!["((profile:a || profile:b) && filter:f) || profile:c".into()]
        }
    );
    assert_eq!(
        syntax("profile:c || (profile:a || profile:b) || profile:d"),
        Syntax::UnnarrowedGroup {
            suggestion: Some("profile:c || profile:a || profile:b || profile:d".into())
        }
    );
    assert_eq!(
        syntax("(profile:a && filter:f)"),
        Syntax::LoneParenTerm {
            suggestion: Some("profile:a && filter:f".into())
        }
    );
    assert_eq!(
        syntax("(profile:a && filter:f) && filter:g"),
        Syntax::NarrowedParenTerm
    );
    for text in [
        "(((profile:a)))  || profile:b",
        "((profile:a || (profile:b))) || profile:c",
        "((profile:a && filter:f) || profile:b)",
        "((profile:a || profile:b) || profile:c) || profile:d",
    ] {
        assert_eq!(syntax(text), Syntax::TooDeep, "`{text}`");
    }
}

#[test]
fn unbalanced_and_truncated_expressions() {
    assert_eq!(syntax(""), Syntax::Empty);
    assert_eq!(syntax("  \t"), Syntax::Empty);
    assert_eq!(syntax("profile:a)"), Syntax::UnmatchedClose);
    assert_eq!(syntax("profile:a && filter:f)"), Syntax::UnmatchedClose);
    assert_eq!(syntax("profile:a || profile:b)"), Syntax::UnmatchedClose);
    assert_eq!(
        syntax("(profile:a || )"),
        Syntax::Expected {
            expected: "a `profile:` name",
            found: "`)`".into()
        }
    );
    assert_eq!(
        syntax("(profile:a || profile:b)) && filter:f"),
        Syntax::UnmatchedClose
    );
    let end = |expected: &'static str| Syntax::Expected {
        expected,
        found: "the end of the expression".into(),
    };
    assert_eq!(syntax("(profile:a"), end("`||`, `&&` or `)`"));
    assert_eq!(
        syntax("profile:a &&"),
        end("a `filter:` name or `!` after `&&`")
    );
    assert_eq!(syntax("profile:a ||"), end("a `profile:` name"));
    assert_eq!(syntax("profile:a && filter:f ||"), end("a `profile:` name"));
    assert_eq!(
        syntax("profile:a || profile:b &&"),
        end("a `filter:` name or `!` after `&&`")
    );
    assert_eq!(
        syntax("profile:a filter:f"),
        Syntax::Expected {
            expected: "`&&`, `||` or the end of the expression",
            found: "`filter:f`".into()
        }
    );
    assert_eq!(
        syntax("()"),
        Syntax::Expected {
            expected: "a `profile:` name",
            found: "`)`".into()
        }
    );
}

#[test]
fn columns_count_characters() {
    let err = parse("(profile:å").unwrap_err();
    assert_eq!(
        (err.column, err.problem),
        (10, Syntax::InvalidName("å".into()))
    );
    let err = parse("profile:a ||").unwrap_err();
    assert_eq!(err.column, 13, "one past the end");
    // A no-break space is two bytes and one column.
    let err = parse("\u{a0}profile:a ||").unwrap_err();
    assert_eq!(err.column, 14);
    let err = parse("profile:a\u{a0}\u{a0}&& å").unwrap_err();
    assert_eq!(
        (err.column, err.problem),
        (15, Syntax::InvalidName("å".into()))
    );
}

/// User decision (task-7 review): NAME is `[a-z0-9][a-z0-9_-]*`.
#[test]
fn names_are_lowercase_letters_digits_dash_underscore() {
    for good in ["a", "idle-1h", "a_b-9", "1", "9-", "x_"] {
        assert!(is_name(good), "{good}");
    }
    for bad in [
        "", "-", "-a", "_a", "A", "aB", "a.b", "a b", "a:b", "å", "{uid}", "a/b",
    ] {
        assert!(!is_name(bad), "{bad}");
    }
    for (text, name) in [
        ("profile:Tmp-entry", "Tmp-entry"),
        ("profile:a && filter:-x", "-x"),
        ("profile:a && filter:_x", "_x"),
        ("Pytest-tmp && filter:f", "Pytest-tmp"),
        ("profile:så", "så"),
    ] {
        assert_eq!(syntax(text), Syntax::InvalidName(name.into()), "`{text}`");
    }
    // A valid bare name is still reported as missing its qualifier.
    assert_eq!(syntax("tmp-entry"), Syntax::Unqualified("tmp-entry".into()));
}

/// Every built-in profile name in docs/reference.md (Built-in profile
/// library) is a NAME, so every built-in can be selected.
#[test]
fn every_built_in_name_is_a_name() {
    let library = section(&reference_md(), "### Built-in profile library");
    let rows = table(&library, "| Category | Profile |");
    let names: Vec<&str> = rows
        .iter()
        .flat_map(|row| row[1].split(", "))
        .map(code)
        .collect();
    assert!(
        names.contains(&"cargo-target") && names.contains(&"idle-2d"),
        "{names:?}"
    );
    let bad: Vec<&&str> = names.iter().filter(|name| !is_name(name)).collect();
    assert!(bad.is_empty(), "built-in names that are not NAMEs: {bad:?}");
}

// ---- Static rules, each with a failing case

#[test]
fn unknown_names_are_errors() {
    assert_eq!(
        check_errors("profile:nope && !filter:nada && !profile:zip"),
        vec![
            CheckError::Unknown {
                written: "profile:nope".into(),
                name: "nope".into(),
                suggestions: vec![],
            },
            CheckError::Unknown {
                written: "!filter:nada".into(),
                name: "nada".into(),
                suggestions: vec![],
            },
            CheckError::Unknown {
                written: "!profile:zip".into(),
                name: "zip".into(),
                suggestions: vec![],
            },
        ]
    );
}

#[test]
fn spelling_suggestions_use_the_real_profile_catalog() {
    let library = Library::with_user_profiles(&[]).unwrap();
    let error = compile("profile:cargo-targt", &library)
        .unwrap_err()
        .to_string();
    assert!(error.contains("did you mean `cargo-target`?"), "{error}");
    let error = compile("profile:cargo-target && filter:idle-3e", &library)
        .unwrap_err()
        .to_string();
    assert!(error.contains("`idle-3d`"), "{error}");
    let error = compile("profile:totally-unrelated-name", &library)
        .unwrap_err()
        .to_string();
    assert!(!error.contains("did you mean"), "{error}");
}

#[test]
fn long_unknown_names_do_not_trigger_unbounded_suggestion_work() {
    let library = Library::with_user_profiles(&[]).unwrap();
    let name = "cargo-target".repeat(10_000);
    let error = compile(&format!("profile:{name}"), &library)
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("invalid selection `profile:"));
    assert!(!error.contains("did you mean"));

    let qualifier = "profile".repeat(10_000);
    let error = parse(&format!("{qualifier}:cargo-target"))
        .unwrap_err()
        .to_string();
    assert!(!error.contains("did you mean"));
}

#[test]
fn qualifier_suggestions_and_columns_survive_restatement() {
    let typo = parse("profle:a").unwrap_err();
    assert_eq!(typo.column, 1);
    assert!(typo.to_string().contains("did you mean `profile:`?"));
    let error = compile("  filter:pytest-tmp", &catalog())
        .unwrap_err()
        .to_string();
    assert!(error.contains("at column 3:"), "{error}");
    let error = compile("profile:a && profile:idle-3d", &catalog())
        .unwrap_err()
        .to_string();
    assert!(error.contains("at column 14:"), "{error}");
}

#[test]
fn diagnostics_escape_control_characters_and_do_not_promise_invalid_rewrites() {
    let error = parse("profile:a\n#").unwrap_err().to_string();
    assert!(error.contains("`profile:a\\n#`"), "{error}");
    assert!(!error.contains('\n'), "{error}");
    let error = compile("profile:nope\t", &catalog())
        .unwrap_err()
        .to_string();
    assert!(error.contains("`profile:nope\\t`"), "{error}");
    assert!(!error.contains('\t'), "{error}");
    let error = compile("profile:a || profile:cargo-target && filter:f", &catalog())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("no suggested parenthesisation is valid"),
        "{error}"
    );
    assert!(!error.contains("add parentheses"), "{error}");
}

#[test]
fn successful_selections_report_duplicate_and_contradictory_terms() {
    let catalog = catalog();
    for (text, expected) in [
        ("profile:a || profile:a", "duplicate term"),
        ("profile:a && !profile:a", "selects nothing"),
        ("profile:a && filter:f && !filter:f", "contradict"),
        ("profile:a && filter:f && filter:f", "repeats"),
    ] {
        let checked = compile(text, &catalog).unwrap();
        assert!(
            checked
                .warnings()
                .iter()
                .any(|warning| warning.contains(expected)),
            "{text}: {:?}",
            checked.warnings()
        );
    }
    assert!(
        compile("profile:a && filter:f", &catalog)
            .unwrap()
            .warnings()
            .is_empty()
    );
}

#[test]
fn wrong_qualifiers_name_what_the_name_is() {
    assert_eq!(
        message("profile:idle-3d"),
        "`profile:idle-3d`: `idle-3d` is a filter, not a source profile; a term starts with a \
         source profile and narrows with `&& filter:idle-3d`"
    );
    assert_eq!(
        message("profile:a && filter:tmp-entry"),
        "`filter:tmp-entry`: `tmp-entry` is a source profile of kind fs, not a filter; write \
         `!profile:tmp-entry` to exclude its matches"
    );
    assert_eq!(
        message("profile:a && !profile:idle-30d"),
        "`!profile:idle-30d`: `idle-30d` is a filter, not a source profile; write \
         `!filter:idle-30d`"
    );
    assert_eq!(
        message("profile:a && !filter:cargo-target"),
        "`!filter:cargo-target`: `cargo-target` is a source profile of kind cargo-target, not \
         a filter; write `!profile:cargo-target`"
    );
    // Syntax errors about a name in the wrong position, restated by compile.
    assert_eq!(
        message("profile:a && profile:idle-3d"),
        "`profile:idle-3d`: `idle-3d` is a filter, not a source profile; write `filter:idle-3d`"
    );
    assert_eq!(
        message("(profile:a || filter:tmp-entry) && filter:f"),
        "`filter:tmp-entry`: `tmp-entry` is a source profile of kind fs, not a filter; write \
         `profile:tmp-entry`"
    );
    // ... but only when the name really is of the other sort.
    assert_eq!(
        message("profile:a && profile:b"),
        Syntax::TwoSources("b".into()).to_string()
    );
    assert_eq!(
        message("filter:f"),
        Syntax::FilterStartsTerm("f".into()).to_string()
    );
    let Err(SelectError::Check(errors)) = compile("filter:pytest-tmp", &catalog()) else {
        panic!("not a wrong-qualifier error");
    };
    assert_eq!(
        errors.to_string(),
        "invalid selection `filter:pytest-tmp` at column 1: `filter:pytest-tmp`: `pytest-tmp` is a source \
         profile of kind fs, not a filter; write `profile:pytest-tmp`"
    );
}

#[test]
fn one_kind_per_expression() {
    assert_eq!(
        check_errors(
            "(profile:a || profile:cargo-target || profile:nix-garbage) && filter:idle-1d"
        ),
        vec![
            CheckError::KindsDiffer {
                first: "a".into(),
                first_kind: Kind::Fs,
                other: "cargo-target".into(),
                other_kind: Kind::CargoTarget,
            },
            CheckError::KindsDiffer {
                first: "a".into(),
                first_kind: Kind::Fs,
                other: "nix-garbage".into(),
                other_kind: Kind::NixStore,
            },
            CheckError::KindShape {
                kind: Kind::NixStore,
                factor: "filter:idle-1d".into(),
            },
        ]
    );
}

#[test]
fn one_action_per_expression() {
    assert_eq!(
        check_errors("profile:a || profile:pip-cache || profile:tmp-entry"),
        vec![CheckError::ActionsDiffer {
            first: "a".into(),
            first_action: "delete".into(),
            other: "pip-cache".into(),
            other_action:
                "command [\"pip\", \"--cache-dir\", \"{path}\", \"cache\", \"purge\"], cwd: candidate"
                    .into(),
        }]
    );
    // Same kind and action: fine, in either order, and it is the kind.
    for text in [
        "profile:a || profile:tmp-entry",
        "profile:tmp-entry || profile:a",
    ] {
        let checked = compile(text, &catalog()).unwrap();
        assert_eq!(checked.kind(), Kind::Fs);
    }
}

/// mped S2: a non-enumerating kind acts as a whole, so its expression is
/// one term, whatever the actions are.
#[test]
fn non_enumerating_kinds_take_one_term() {
    for (text, kind) in [
        ("profile:nix-garbage || profile:nix-garbage", Kind::NixStore),
        (
            "(profile:docker-build-cache || profile:docker-build-cache) && filter:idle-7d",
            Kind::DockerBuildCache,
        ),
        (
            "profile:docker-dangling || profile:docker-dangling",
            Kind::DockerImages,
        ),
    ] {
        assert_eq!(
            check_errors(text),
            vec![CheckError::OneTerm { kind, terms: 2 }],
            "`{text}`"
        );
    }
    // Different actions too: both rules are reported.
    assert_eq!(
        check_errors("profile:docker-dangling || profile:docker-unreferenced"),
        vec![
            CheckError::ActionsDiffer {
                first: "docker-dangling".into(),
                first_action: "docker image prune".into(),
                other: "docker-unreferenced".into(),
                other_action: "docker image prune --all".into(),
            },
            CheckError::OneTerm {
                kind: Kind::DockerImages,
                terms: 2
            },
        ]
    );
    // Enumerating kinds take unions.
    compile("profile:cargo-target || profile:cargo-target", &catalog()).unwrap();
}

/// mped S3: a suggested qualifier is offered only if the rewrite breaks no
/// new rule; the error still says what the name is.
#[test]
fn rejected_fixes_are_not_suggested() {
    for (text, want) in [
        (
            "profile:a && filter:nix-garbage",
            "`filter:nix-garbage`: `nix-garbage` is a source profile of kind nix-store, not a \
             filter",
        ),
        (
            "profile:a && !filter:docker-dangling",
            "`!filter:docker-dangling`: `docker-dangling` is a source profile of kind \
             docker-images, not a filter",
        ),
        (
            "profile:docker-build-cache && !filter:tmp-entry",
            "`!filter:tmp-entry`: `tmp-entry` is a source profile of kind fs, not a filter",
        ),
        (
            "profile:nix-garbage && profile:idle-3d",
            "`profile:idle-3d`: `idle-3d` is a filter, not a source profile",
        ),
        (
            "profile:cargo-target && profile:has-cachedir-tag",
            "`profile:has-cachedir-tag`: `has-cachedir-tag` is a filter, not a source profile",
        ),
        (
            "profile:a || filter:cargo-target",
            "`filter:cargo-target`: `cargo-target` is a source profile of kind cargo-target, not \
             a filter",
        ),
    ] {
        assert_eq!(message(text), want, "`{text}`");
    }
    // An unrelated error elsewhere does not suppress a working fix.
    assert_eq!(
        message("profile:a && !filter:tmp-entry && filter:nope"),
        "`!filter:tmp-entry`: `tmp-entry` is a source profile of kind fs, not a filter; write \
         `!profile:tmp-entry`; `filter:nope`: no profile or filter is named `nope`"
    );
}

/// mped S3 for syntax errors: compile offers only the parser's rewrites
/// that the static rules accept as well.
#[test]
fn compile_drops_syntax_suggestions_the_rules_reject() {
    // The parser alone suggests both rewrites ...
    assert_eq!(
        syntax("(profile:a || profile:nix-garbage) || profile:b"),
        Syntax::UnnarrowedGroup {
            suggestion: Some("profile:a || profile:nix-garbage || profile:b".into())
        }
    );
    assert_eq!(
        syntax("(profile:nix-garbage) && filter:idle-1d"),
        Syntax::ParenOne {
            suggestion: Some("profile:nix-garbage && filter:idle-1d".into())
        }
    );
    // ... compile drops them: mixed kinds, a filter on nix-store.
    assert_eq!(
        message("(profile:a || profile:nix-garbage) || profile:b"),
        Syntax::UnnarrowedGroup { suggestion: None }.to_string()
    );
    assert_eq!(
        message("(profile:nix-garbage) && filter:idle-1d"),
        Syntax::ParenOne { suggestion: None }.to_string()
    );
}

/// mped S4: errors from compile quote the expression as written; a bare
/// check quotes its normalised form.
#[test]
fn compile_errors_quote_the_text_as_written() {
    let text = "(profile:a ||  profile:b) && filter:nope";
    let Err(SelectError::Check(errors)) = compile(text, &catalog()) else {
        panic!("accepted");
    };
    assert_eq!(errors.expr, text);
    let Err(SelectError::Check(errors)) = compile("filter:pytest-tmp", &catalog()) else {
        panic!("not requalified");
    };
    assert_eq!(errors.expr, "filter:pytest-tmp");
    let errors = check(parse(text).unwrap(), &catalog()).unwrap_err();
    assert_eq!(
        errors.expr,
        "(profile:a && filter:nope) || (profile:b && filter:nope)"
    );
}

#[test]
fn negated_profiles_do_not_count_towards_kind_or_action() {
    let checked = compile(
        "profile:tmp-entry && !profile:cargo-target && !profile:pip-cache",
        &catalog(),
    )
    .unwrap();
    assert_eq!(checked.kind(), Kind::Fs);
    let checked = compile(
        "profile:cargo-target && !profile:claude-code-tmp",
        &catalog(),
    )
    .unwrap();
    assert_eq!(checked.kind(), Kind::CargoTarget);
}

#[test]
fn predicates_must_be_supported_by_the_kind() {
    assert_eq!(
        check_errors("profile:cargo-target && !filter:has-cachedir-tag"),
        vec![CheckError::UnsupportedPredicate {
            filter: "!filter:has-cachedir-tag".into(),
            predicate: PredicateName::Has,
            kind: Kind::CargoTarget,
        }]
    );
    assert_eq!(
        check_errors("profile:docker-build-cache && filter:has-cachedir-tag"),
        vec![CheckError::UnsupportedPredicate {
            filter: "filter:has-cachedir-tag".into(),
            predicate: PredicateName::Has,
            kind: Kind::DockerBuildCache,
        }]
    );
    // The supported ones pass.
    compile("profile:a && filter:has-cachedir-tag", &catalog()).unwrap();
    compile("profile:docker-build-cache && filter:idle-7d", &catalog()).unwrap();
    compile("profile:cargo-target && filter:idle-7d", &catalog()).unwrap();
}

#[test]
fn negated_retention_guards_are_rejected_even_in_mixed_filters() {
    for (name, predicate) in [
        ("retention-newest", PredicateName::KeepNewest),
        ("retention-links", PredicateName::ProtectLinks),
        ("retention-link-dirs", PredicateName::ProtectLinks),
        ("retention-current", PredicateName::ProtectLinks),
        (
            "retention-installed",
            PredicateName::ProtectCursorExtensions,
        ),
        ("retention-mixed", PredicateName::KeepNewest),
    ] {
        let written = format!("!filter:{name}");
        let text = format!("profile:a && {written}");
        assert_eq!(
            check_errors(&text),
            vec![CheckError::NegatedRetentionGuard {
                filter: written,
                predicate,
            }],
            "{text}"
        );
        compile(&format!("profile:a && filter:{name}"), &catalog())
            .unwrap_or_else(|error| panic!("positive guard {name}: {error}"));
    }
    for name in ["f", "has-cachedir-tag"] {
        compile(&format!("profile:a && !filter:{name}"), &catalog())
            .unwrap_or_else(|error| panic!("safe complement {name}: {error}"));
    }
}

proptest! {
    /// The independent oracle classifies filter names by their declared
    /// predicates, including a mixed has+guard filter; the compiler must
    /// reject a guard on either term and accept known-evidence complements.
    #[test]
    fn guard_negation_is_static_for_random_union_terms(
        first in 0usize..8,
        second in 0usize..8,
        source in any::<bool>(),
    ) {
        const FILTERS: [&str; 8] = [
            "f", "has-cachedir-tag", "retention-newest", "retention-links",
            "retention-link-dirs", "retention-current", "retention-installed",
            "retention-mixed",
        ];
        let first_source = if source { "a" } else { "b" };
        let second_source = if source { "b" } else { "a" };
        let text = format!(
            "(profile:{first_source} && !filter:{}) || (profile:{second_source} && !filter:{})",
            FILTERS[first], FILTERS[second]
        );
        let expect_rejected = first >= 2 || second >= 2;
        prop_assert_eq!(compile(&text, &catalog()).is_err(), expect_rejected, "{}", text);
    }
}

#[test]
fn predicate_table_matches_the_reference() {
    // docs/reference.md, Predicates (`where`).
    let table: Vec<(PredicateName, Vec<Kind>)> = PredicateName::ALL
        .iter()
        .map(|p| {
            (
                *p,
                Kind::ALL
                    .into_iter()
                    .filter(|k| p.supported_by(*k))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        table,
        vec![
            (
                PredicateName::IdleFor,
                vec![Kind::Fs, Kind::CargoTarget, Kind::DockerBuildCache]
            ),
            (PredicateName::Has, vec![Kind::Fs]),
            (PredicateName::HasDir, vec![Kind::Fs]),
            (PredicateName::HasNot, vec![Kind::Fs]),
            (PredicateName::AppNotRunning, vec![Kind::Fs]),
            (PredicateName::LacksEntry, vec![Kind::Fs]),
            (PredicateName::SourceAnchor, vec![Kind::Fs]),
            (PredicateName::LivenessContract, vec![Kind::Fs]),
            (PredicateName::KeepNewest, vec![Kind::Fs]),
            (PredicateName::ProtectLinks, vec![Kind::Fs]),
            (PredicateName::ProtectCursorExtensions, vec![Kind::Fs]),
        ]
    );
}

#[test]
fn non_fs_kinds_take_only_their_shapes() {
    for (text, kind, factor) in [
        (
            "profile:nix-garbage && filter:idle-1d",
            Kind::NixStore,
            "filter:idle-1d",
        ),
        (
            "profile:docker-dangling && !filter:idle-1d",
            Kind::DockerImages,
            "!filter:idle-1d",
        ),
        (
            "profile:docker-build-cache && !filter:idle-30d",
            Kind::DockerBuildCache,
            "!filter:idle-30d",
        ),
        (
            "profile:docker-build-cache && !profile:tmp-entry",
            Kind::DockerBuildCache,
            "!profile:tmp-entry",
        ),
    ] {
        assert_eq!(
            check_errors(text),
            vec![CheckError::KindShape {
                kind,
                factor: factor.into()
            }],
            "`{text}`"
        );
    }
    for text in [
        "profile:nix-garbage",
        "profile:docker-dangling",
        "profile:docker-build-cache && filter:idle-7d && filter:idle-1d",
    ] {
        compile(text, &catalog()).unwrap_or_else(|e| panic!("{e}"));
    }
}

#[test]
fn negated_profiles_must_enumerate() {
    assert_eq!(
        check_errors("profile:a && !profile:docker-build-cache && !profile:nix-garbage"),
        vec![
            CheckError::NegatedKind {
                name: "docker-build-cache".into(),
                kind: Kind::DockerBuildCache,
            },
            CheckError::NegatedKind {
                name: "nix-garbage".into(),
                kind: Kind::NixStore,
            },
        ]
    );
}

#[test]
fn errors_are_reported_once_and_all_together() {
    let Err(SelectError::Check(errors)) = compile(
        "(profile:a || profile:b) && filter:nope && !profile:docker-dangling",
        &catalog(),
    ) else {
        panic!("accepted");
    };
    assert_eq!(
        errors.to_string(),
        "invalid selection `(profile:a || profile:b) && filter:nope && \
         !profile:docker-dangling`: `filter:nope`: no profile or \
         filter is named `nope`; `!profile:docker-dangling`: a negated profile must be of kind \
         fs or cargo-target, and `docker-dangling` is of kind docker-images"
    );
}

#[test]
fn a_hand_built_empty_expression_is_refused() {
    assert_eq!(
        check(Expr { terms: vec![] }, &catalog())
            .unwrap_err()
            .errors,
        vec![CheckError::NoTerms]
    );
}

// ---- Properties

const PROFILES: &[&str] = &["a", "b", "c"];
const FILTERS: &[&str] = &["f", "g", "h"];

/// Names for the compile properties: sources and filters of every kind,
/// on both sides, so wrong qualifiers and kind rules come up often.
const MIXED: &[&str] = &[
    "a",
    "tmp-entry",
    "nix-garbage",
    "cargo-target",
    "docker-build-cache",
    "docker-dangling",
    "idle-3d",
    "has-cachedir-tag",
    "f",
];

fn factor_strategy(
    profiles: &'static [&'static str],
    filters: &'static [&'static str],
) -> impl Strategy<Value = Factor> {
    (
        0..3u8,
        prop::sample::select(profiles),
        prop::sample::select(filters),
    )
        .prop_map(|(sort, profile, filter)| match sort {
            0 => Factor::Filter(filter.into()),
            1 => Factor::NotFilter(filter.into()),
            _ => Factor::NotProfile(profile.into()),
        })
}

fn term_strategy() -> impl Strategy<Value = Term> {
    (
        prop::sample::select(PROFILES),
        prop::collection::vec(factor_strategy(PROFILES, FILTERS), 0..4),
    )
        .prop_map(|(source, factors)| Term {
            source: source.into(),
            factors,
        })
}

/// A term as written: one source or a group of two or more, and factors
/// (at least one after a group).
#[derive(Debug, Clone)]
struct Written {
    sources: Vec<String>,
    factors: Vec<Factor>,
}

impl Written {
    fn render(&self, sp: &str) -> String {
        let names: Vec<String> = self
            .sources
            .iter()
            .map(|s| format!("profile:{s}"))
            .collect();
        let mut text = match names.as_slice() {
            [one] => one.clone(),
            _ => format!("({sp}{}{sp})", names.join(&format!("{sp}||{sp}"))),
        };
        for factor in &self.factors {
            text.push_str(&format!("{sp}&&{sp}{factor}"));
        }
        text
    }

    fn terms(&self) -> Vec<Term> {
        distribute(self.sources.clone(), &self.factors)
    }
}

/// `narrowed`: always with factors (a parenthesised union member).
fn written_strategy(
    profiles: &'static [&'static str],
    filters: &'static [&'static str],
    narrowed: bool,
) -> impl Strategy<Value = Written> {
    (
        prop::collection::vec(prop::sample::select(profiles), 1..4),
        prop::collection::vec(factor_strategy(profiles, filters), 1..3),
        any::<bool>(),
    )
        .prop_map(move |(sources, mut factors, bare)| {
            if sources.len() == 1 && bare && !narrowed {
                factors.clear();
            }
            Written {
                sources: sources.into_iter().map(str::to_owned).collect(),
                factors,
            }
        })
}

/// A surface expression and the AST it must parse to: one written term,
/// or a union of members that are bare profiles or parenthesised terms.
fn surface_strategy(
    profiles: &'static [&'static str],
    filters: &'static [&'static str],
) -> impl Strategy<Value = (String, Expr)> {
    let spacing = prop::sample::select(vec!["", " ", " \t "]);
    let single = (written_strategy(profiles, filters, false), spacing.clone())
        .prop_map(|(w, sp)| (w.render(sp), Expr { terms: w.terms() }));
    let member = prop_oneof![
        prop::sample::select(profiles).prop_map(|p| (format!("profile:{p}"), None)),
        written_strategy(profiles, filters, true).prop_map(|w| (String::new(), Some(w))),
    ];
    let union = (prop::collection::vec(member, 2..5), spacing).prop_map(|(members, sp)| {
        let mut texts = Vec::new();
        let mut terms = Vec::new();
        for (text, written) in members {
            match written {
                None => {
                    terms.push(term(text.trim_start_matches("profile:"), vec![]));
                    texts.push(text);
                }
                Some(w) => {
                    terms.extend(w.terms());
                    texts.push(format!("({sp}{}{sp})", w.render(sp)));
                }
            }
        }
        (texts.join(&format!("{sp}||{sp}")), Expr { terms })
    });
    prop_oneof![single, union]
}

fn without_fix(error: &CheckError) -> CheckError {
    match error {
        CheckError::WrongQualifier {
            written, name, is, ..
        } => CheckError::WrongQualifier {
            written: written.clone(),
            name: name.clone(),
            is: is.clone(),
            fix: None,
        },
        other => other.clone(),
    }
}

/// `text` (whitespace-separated, parentheses possibly attached) with the
/// token `written` replaced by `fix`: every occurrence, or the first.
fn rewrite_tokens(text: &str, written: &str, fix: &str, all: bool) -> String {
    let mut done = false;
    let pieces: Vec<String> = text
        .split(' ')
        .map(|piece| {
            let core = piece.trim_start_matches('(').trim_end_matches(')');
            if core == written && (all || !done) {
                done = true;
                piece.replacen(core, fix, 1)
            } else {
                piece.to_owned()
            }
        })
        .collect();
    pieces.join(" ")
}

/// mped S3: every rewrite `compile` suggests is accepted, or (a
/// wrong-qualifier fix in an expression with other errors) breaks no rule
/// the original did not already break.
fn suggestions_compile(text: &str) -> Result<(), TestCaseError> {
    let catalog = catalog();
    match compile(text, &catalog) {
        Ok(checked) => {
            let again = compile(&checked.expr().to_string(), &catalog).map(|c| c.expr().clone());
            prop_assert_eq!(again.ok(), Some(checked.expr().clone()));
        }
        Err(SelectError::Syntax(e)) => {
            for s in e.problem.suggestions() {
                prop_assert!(compile(s, &catalog).is_ok(), "`{}` suggested `{}`", text, s);
            }
        }
        Err(SelectError::Check(errors)) => {
            // Check errors of a parsed expression: rewrite every occurrence
            // in its canonical text. A restated syntax error: the one at
            // fault, in the text as written.
            let (base, all) = match parse(text) {
                Ok(expr) => (expr.to_string(), true),
                Err(_) => (text.to_owned(), false),
            };
            let before: Vec<CheckError> = errors.errors.iter().map(without_fix).collect();
            for (i, error) in errors.errors.iter().enumerate() {
                let CheckError::WrongQualifier {
                    written,
                    fix: Some(fix),
                    ..
                } = error
                else {
                    continue;
                };
                let rewritten = rewrite_tokens(&base, written, fix, all);
                match compile(&rewritten, &catalog) {
                    Ok(_) => {}
                    Err(SelectError::Check(after)) => {
                        for a in after.errors.iter().map(without_fix) {
                            prop_assert!(
                                before.iter().enumerate().any(|(j, b)| j != i && *b == a),
                                "`{}`: fix `{}` gives `{}`, which adds: {}",
                                text,
                                fix,
                                rewritten,
                                a
                            );
                        }
                    }
                    Err(SelectError::Syntax(e)) => {
                        prop_assert!(false, "`{}`: fix `{}` does not parse: {}", text, fix, e);
                    }
                }
            }
        }
    }
    Ok(())
}

proptest! {
    /// Printing a normalised AST and parsing it back is the identity.
    #[test]
    fn print_then_parse_is_identity(terms in prop::collection::vec(term_strategy(), 1..5)) {
        let expr = Expr { terms };
        let text = expr.to_string();
        prop_assert_eq!(parse(&text).map_err(|e| e.to_string()), Ok(expr));
    }

    /// Every surface form parses to its distributed AST, and the normal
    /// form is stable.
    #[test]
    fn surface_forms_parse_to_their_distribution(
        (text, want) in surface_strategy(PROFILES, FILTERS)
    ) {
        let got = parse(&text).map_err(|e| e.to_string());
        prop_assert_eq!(got, Ok(want.clone()), "{}", text);
        prop_assert_eq!(parse(&want.to_string()).map_err(|e| e.to_string()), Ok(want));
    }

    /// Any token soup: no panic; accepted text round-trips; every
    /// suggested rewrite in an error parses.
    #[test]
    fn token_soup_never_panics_and_suggestions_parse(
        toks in prop::collection::vec(
            prop::sample::select(vec![
                "profile:a", "profile:b", "filter:f", "(", ")", "&&", "||", "!", "&", "|",
                "x:", "a", " ", "profile:", "#", "A",
            ]),
            0..14,
        )
    ) {
        let text = toks.concat();
        match parse(&text) {
            Ok(expr) => prop_assert_eq!(parse(&expr.to_string()).ok(), Some(expr)),
            Err(e) => {
                for s in e.problem.suggestions() {
                    prop_assert!(parse(s).is_ok(), "`{}` suggested `{}`", text, s);
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// mped S3 over well-formed expressions with names of every sort.
    #[test]
    fn compile_suggestions_compile_on_surface_forms((text, _) in surface_strategy(MIXED, MIXED)) {
        suggestions_compile(&text)?;
    }

    /// mped S3 over token soup (space-separated, so tokens can be rewritten).
    #[test]
    fn compile_suggestions_compile_on_token_soup(
        toks in prop::collection::vec(
            prop::sample::select(vec![
                "profile:a", "profile:tmp-entry", "profile:nix-garbage", "profile:cargo-target",
                "profile:docker-build-cache", "profile:idle-3d", "profile:has-cachedir-tag",
                "filter:f", "filter:idle-3d", "filter:nix-garbage", "filter:cargo-target",
                "filter:tmp-entry", "!profile:tmp-entry", "!profile:idle-3d",
                "!profile:docker-dangling", "!filter:idle-3d", "!filter:tmp-entry",
                "!filter:nix-garbage", "(", ")", "&&", "||",
            ]),
            1..10,
        )
    ) {
        suggestions_compile(&toks.join(" "))?;
    }
}
