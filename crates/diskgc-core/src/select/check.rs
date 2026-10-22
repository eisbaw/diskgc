//! The static rules of the selection grammar that need to know what each
//! name is (docs/reference.md, Selection grammar and Predicates): unknown
//! names, wrong qualifiers, one kind and one action per expression,
//! predicates the kind supports, what the non-enumerating kinds allow (one
//! term, at most idle filters), and which kinds `!profile:` may name.
//!
//! What a name is comes from a [`Catalog`], implemented by the profile
//! model (built-in and user profiles); tests use a fake one. The checks are
//! pure and machine-independent, so `diskgc check` can run them anywhere.
//!
//! A wrong-qualifier error suggests the other qualifier only if the
//! expression with that one name rewritten breaks no rule that the original
//! did not already break; otherwise it only says what the name is.

use std::fmt;

use super::parse::{MAX_SUGGESTION_NAME_CHARS, Syntax, SyntaxError, edit_distance, parse};
use super::{Expr, Factor, Term};
use crate::kind::{Kind, PredicateName};

/// What the checker needs to know about names: implemented by the profile
/// model over the built-in and user profiles (overrides already applied).
pub trait Catalog {
    /// How a source profile removes its candidates. Two sources have the
    /// same action exactly when their actions are equal (the action type
    /// and all its options); `Display` must show what differs, since it
    /// names both actions in the "one action" error.
    type Action: PartialEq + fmt::Display;

    /// The profile called `name`, if any.
    fn lookup(&self, name: &str) -> Option<Entry<'_, Self::Action>>;

    /// Names available for spelling suggestions. Small test catalogs may
    /// leave this empty; the production library returns all active names.
    fn names(&self) -> Vec<&str> {
        Vec::new()
    }
}

/// A profile as the checker sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry<'a, A> {
    /// A profile with `find`.
    Source { kind: Kind, action: &'a A },
    /// A profile with only `where`: the predicates it uses.
    Filter { predicates: Vec<PredicateName> },
}

/// An expression that passed [`check`], with its kind. Later stages take
/// this, not a bare [`Expr`], so nothing evaluates an unchecked expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    expr: Expr,
    kind: Kind,
    warnings: Vec<String>,
}

impl Checked {
    pub fn expr(&self) -> &Expr {
        &self.expr
    }

    /// The one kind of every source in the expression.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Advisory diagnostics that do not make a selection invalid.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// What a wrongly qualified name actually is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Is {
    Source(Kind),
    Filter,
}

/// One broken static rule. `Display` is the message; docs/reference.md
/// quotes some of them and a test keeps the two equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckError {
    /// An [`Expr`] built by hand with no terms (the parser never makes one).
    NoTerms,
    Unknown {
        written: String,
        name: String,
        suggestions: Vec<String>,
    },
    /// `profile:NAME` where `NAME` is a filter: a term must start with a
    /// source.
    FilterAsSource { name: String },
    /// The name is a profile of the other sort. `fix` is the name written
    /// with the other qualifier, when the expression would then be accepted
    /// as far as this name is concerned.
    WrongQualifier {
        written: String,
        name: String,
        is: Is,
        fix: Option<String>,
    },
    KindsDiffer {
        first: String,
        first_kind: Kind,
        other: String,
        other_kind: Kind,
    },
    ActionsDiffer {
        first: String,
        first_action: String,
        other: String,
        other_action: String,
    },
    /// A non-enumerating kind in a union.
    OneTerm { kind: Kind, terms: usize },
    UnsupportedPredicate {
        filter: String,
        predicate: PredicateName,
        kind: Kind,
    },
    /// A retention guard cannot be complemented by `!filter:`.
    NegatedRetentionGuard {
        filter: String,
        predicate: PredicateName,
    },
    /// A factor that a non-enumerating kind does not take.
    KindShape { kind: Kind, factor: String },
    /// `!profile:NAME` with a profile of a non-enumerating kind.
    NegatedKind { name: String, kind: Kind },
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTerms => f.write_str("the expression has no terms"),
            Self::Unknown {
                written,
                name,
                suggestions,
            } => {
                write!(f, "`{written}`: no profile or filter is named `{name}`")?;
                if !suggestions.is_empty() {
                    let names = suggestions
                        .iter()
                        .map(|s| format!("`{s}`"))
                        .collect::<Vec<_>>();
                    write!(f, "; did you mean {}?", names.join(", "))?;
                }
                Ok(())
            }
            Self::FilterAsSource { name } => write!(
                f,
                "`profile:{name}`: `{name}` is a filter, not a source profile; a term starts \
                 with a source profile and narrows with `&& filter:{name}`"
            ),
            Self::WrongQualifier {
                written,
                name,
                is,
                fix,
            } => {
                match is {
                    Is::Source(kind) => write!(
                        f,
                        "`{written}`: `{name}` is a source profile of kind {kind}, not a filter"
                    )?,
                    Is::Filter => {
                        write!(f, "`{written}`: `{name}` is a filter, not a source profile")?;
                    }
                }
                match fix {
                    // `&& filter:p` with p a source: the negation is the
                    // only rewrite that keeps one source per term.
                    Some(fix) if fix.starts_with('!') && !written.starts_with('!') => {
                        write!(f, "; write `{fix}` to exclude its matches")
                    }
                    Some(fix) => write!(f, "; write `{fix}`"),
                    None => Ok(()),
                }
            }
            Self::KindsDiffer {
                first,
                first_kind,
                other,
                other_kind,
            } => write!(
                f,
                "`profile:{first}` is of kind {first_kind} but `profile:{other}` is of kind \
                 {other_kind}: the sources of one expression share one kind; select them in \
                 separate tiers"
            ),
            Self::ActionsDiffer {
                first,
                first_action,
                other,
                other_action,
            } => write!(
                f,
                "`profile:{first}` removes with `{first_action}` but `profile:{other}` with \
                 `{other_action}`: the sources of one expression share one action; select them \
                 in separate tiers"
            ),
            Self::OneTerm { kind, terms } => write!(
                f,
                "kind {kind} acts on everything it finds at once (no per-candidate \
                 enumeration), so its expression has one term, not a union of {terms}; select \
                 each in its own tier"
            ),
            Self::UnsupportedPredicate {
                filter,
                predicate,
                kind,
            } => {
                let kinds: Vec<&str> = predicate.kinds().iter().map(|k| k.name()).collect();
                write!(
                    f,
                    "`{filter}` uses `{predicate}`, which kind {kind} does not support \
                     (`{predicate}` works on: {})",
                    kinds.join(", ")
                )
            }
            Self::NegatedRetentionGuard { filter, predicate } => write!(
                f,
                "`{filter}` uses `{predicate}`, a retention guard that cannot be negated"
            ),
            Self::KindShape { kind, factor } => write!(
                f,
                "kind {kind} takes only {}, so `{factor}` cannot narrow it: it has no \
                 per-candidate enumeration",
                shapes(*kind)
            ),
            Self::NegatedKind { name, kind } => write!(
                f,
                "`!profile:{name}`: a negated profile must be of kind fs or cargo-target, and \
                 `{name}` is of kind {kind}"
            ),
        }
    }
}

/// Every broken rule of one expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckErrors {
    /// The expression: as written when it came through
    /// [`compile`](super::compile), normalised from a bare [`check`].
    pub expr: String,
    /// Source column when this static error restates a parser error.
    pub column: Option<usize>,
    /// In the order found, without duplicates (distribution repeats
    /// factors).
    pub errors: Vec<CheckError>,
}

impl fmt::Display for CheckErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid selection `{}`", self.expr.escape_debug())?;
        if let Some(column) = self.column {
            write!(f, " at column {column}")?;
        }
        f.write_str(": ")?;
        for (i, error) in self.errors.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            error.fmt(f)?;
        }
        Ok(())
    }
}

impl std::error::Error for CheckErrors {}

/// Check `expr` against `catalog`; every broken rule is reported, not just
/// the first.
pub fn check<C: Catalog>(expr: Expr, catalog: &C) -> Result<Checked, CheckErrors> {
    let (kind, mut errors) = rules(&expr, catalog);
    match kind {
        Some(kind) if errors.is_empty() => {
            let warnings = selection_warnings(&expr);
            Ok(Checked {
                expr,
                kind,
                warnings,
            })
        }
        _ => {
            if expr.terms.is_empty() {
                errors.push(CheckError::NoTerms);
            }
            keep_working_fixes(&expr, catalog, &mut errors);
            Err(CheckErrors {
                expr: expr.to_string(),
                column: None,
                errors,
            })
        }
    }
}

/// The expression's kind (its first known source's) and every broken rule,
/// with unverified fixes.
fn rules<C: Catalog>(expr: &Expr, catalog: &C) -> (Option<Kind>, Vec<CheckError>) {
    let mut errors = Errors::default();
    let mut first: Option<(&str, Kind, &C::Action)> = None;
    let mut kinds_differ = false;
    for term in &expr.terms {
        let kind = match catalog.lookup(&term.source) {
            None => {
                errors.push(unknown(
                    format!("profile:{}", term.source),
                    &term.source,
                    catalog,
                ));
                None
            }
            Some(Entry::Filter { .. }) => {
                errors.push(CheckError::FilterAsSource {
                    name: term.source.clone(),
                });
                None
            }
            Some(Entry::Source { kind, action }) => {
                match first {
                    None => first = Some((&term.source, kind, action)),
                    Some((name, first_kind, _)) if first_kind != kind => {
                        kinds_differ = true;
                        errors.push(CheckError::KindsDiffer {
                            first: name.to_owned(),
                            first_kind,
                            other: term.source.clone(),
                            other_kind: kind,
                        });
                    }
                    Some((name, _, first_action)) if first_action != action => {
                        errors.push(CheckError::ActionsDiffer {
                            first: name.to_owned(),
                            first_action: first_action.to_string(),
                            other: term.source.clone(),
                            other_action: action.to_string(),
                        });
                    }
                    Some(_) => {}
                }
                Some(kind)
            }
        };
        for factor in &term.factors {
            check_factor(factor, kind, catalog, &mut errors);
        }
    }
    let kind = first.map(|(_, kind, _)| kind);
    if let Some(kind) = kind
        && !kind.enumerates()
        && !kinds_differ
        && expr.terms.len() > 1
    {
        errors.push(CheckError::OneTerm {
            kind,
            terms: expr.terms.len(),
        });
    }
    (kind, errors.0)
}

/// Whether a term of kind `kind` may have `factor` (docs/reference.md,
/// "Non-`fs` kinds"): any for `fs` and `cargo-target`, plain filters for
/// `docker-build-cache`, none for `docker-images` and `nix-store`. Which
/// predicates a filter may use is [`PredicateName::kinds`].
fn takes(kind: Kind, factor: &Factor) -> bool {
    match kind {
        Kind::Fs | Kind::CargoTarget => true,
        Kind::DockerBuildCache => matches!(factor, Factor::Filter(_)),
        Kind::DockerImages | Kind::NixStore => false,
    }
}

/// The expression shapes a non-enumerating kind allows, for messages.
fn shapes(kind: Kind) -> &'static str {
    match kind {
        Kind::DockerBuildCache => "`profile:NAME` or `profile:NAME && filter:idle-*`",
        _ => "`profile:NAME`",
    }
}

/// One factor of a term whose source is of kind `kind` (`None` when the
/// source itself is in error).
fn check_factor<C: Catalog>(factor: &Factor, kind: Option<Kind>, catalog: &C, errors: &mut Errors) {
    let written = factor.to_string();
    let shape_ok = |errors: &mut Errors| match kind {
        Some(kind) if !takes(kind, factor) => {
            errors.push(CheckError::KindShape {
                kind,
                factor: written.clone(),
            });
            false
        }
        _ => true,
    };
    match factor {
        Factor::Filter(name) | Factor::NotFilter(name) => match catalog.lookup(name) {
            None => errors.push(unknown(written.clone(), name, catalog)),
            Some(Entry::Source { kind: is, .. }) => errors.push(CheckError::WrongQualifier {
                written: written.clone(),
                name: name.clone(),
                is: Is::Source(is),
                // `&& filter:p` too: the only form a source takes after `&&`.
                fix: Some(format!("!profile:{name}")),
            }),
            Some(Entry::Filter { predicates }) => {
                if matches!(factor, Factor::NotFilter(_)) {
                    for predicate in &predicates {
                        if predicate.is_retention_guard() {
                            errors.push(CheckError::NegatedRetentionGuard {
                                filter: written.clone(),
                                predicate: *predicate,
                            });
                        }
                    }
                }
                if let Some(kind) = kind
                    && shape_ok(errors)
                {
                    for predicate in predicates {
                        if !predicate.supported_by(kind) {
                            errors.push(CheckError::UnsupportedPredicate {
                                filter: written.clone(),
                                predicate,
                                kind,
                            });
                        }
                    }
                }
            }
        },
        Factor::NotProfile(name) => match catalog.lookup(name) {
            None => errors.push(unknown(written.clone(), name, catalog)),
            Some(Entry::Filter { .. }) => errors.push(CheckError::WrongQualifier {
                written: written.clone(),
                name: name.clone(),
                is: Is::Filter,
                fix: Some(format!("!filter:{name}")),
            }),
            Some(Entry::Source { kind: negated, .. }) => {
                shape_ok(errors);
                if !negated.enumerates() {
                    errors.push(CheckError::NegatedKind {
                        name: name.clone(),
                        kind: negated,
                    });
                }
            }
        },
    }
}

fn unknown<C: Catalog>(written: String, name: &str, catalog: &C) -> CheckError {
    let name_len = name.chars().count();
    if name_len > MAX_SUGGESTION_NAME_CHARS {
        return CheckError::Unknown {
            written,
            name: name.to_owned(),
            suggestions: Vec::new(),
        };
    }
    let mut near: Vec<(&str, usize)> = catalog
        .names()
        .into_iter()
        .filter_map(|candidate| {
            // Preserve substring suggestions while avoiding an edit-distance
            // scan of a huge unknown name for every catalog entry.
            let contained = name.len() >= 3
                && candidate.len() >= 3
                && (candidate.contains(name) || name.contains(candidate));
            let distance = (name_len.abs_diff(candidate.chars().count()) <= 2)
                .then(|| edit_distance(name, candidate));
            (contained || distance.is_some_and(|d| d <= 2))
                .then_some((candidate, distance.unwrap_or(usize::MAX)))
        })
        .collect();
    near.sort_unstable_by_key(|(candidate, distance)| (*distance, *candidate));
    near.dedup_by_key(|(candidate, _)| *candidate);
    let suggestions = near
        .into_iter()
        .take(5)
        .map(|(name, _)| name.to_owned())
        .collect();
    CheckError::Unknown {
        written,
        name: name.to_owned(),
        suggestions,
    }
}

fn selection_warnings(expr: &Expr) -> Vec<String> {
    let mut warnings = Vec::new();
    for term in &expr.terms {
        if term
            .factors
            .iter()
            .any(|factor| matches!(factor, Factor::NotProfile(name) if name == &term.source))
        {
            warnings.push(format!(
                "term `{term}` selects nothing: its source is excluded by `!profile:{}`",
                term.source
            ));
        }
        for factor in &term.factors {
            let count = term.factors.iter().filter(|other| *other == factor).count();
            if count > 1 {
                let warning = format!("term `{term}` repeats `{factor}`");
                if !warnings.contains(&warning) {
                    warnings.push(warning);
                }
            }
            if let Factor::Filter(name) = factor
                && term.factors.contains(&Factor::NotFilter(name.clone()))
            {
                let warning = format!(
                    "term `{term}` selects nothing: `filter:{name}` and `!filter:{name}` contradict"
                );
                if !warnings.contains(&warning) {
                    warnings.push(warning);
                }
            }
        }
    }
    for (i, term) in expr.terms.iter().enumerate() {
        if expr.terms[..i].contains(term) {
            let warning = format!("duplicate term `{term}` in selection");
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }
    }
    warnings
}

/// Drop each wrong-qualifier fix whose rewrite of `expr` breaks a rule that
/// `expr` did not already break.
fn keep_working_fixes<C: Catalog>(expr: &Expr, catalog: &C, errors: &mut [CheckError]) {
    let bare: Vec<CheckError> = errors.iter().map(without_fix).collect();
    for (i, error) in errors.iter_mut().enumerate() {
        let CheckError::WrongQualifier {
            written,
            fix: fix @ Some(_),
            ..
        } = error
        else {
            continue;
        };
        let works = fix
            .as_deref()
            .and_then(factor_from)
            .is_some_and(|replacement| {
                let rewritten = replace_factor(expr, written, &replacement);
                let (_, after) = rules(&rewritten, catalog);
                after.iter().map(without_fix).all(|a| {
                    bare.iter()
                        .enumerate()
                        .any(|(j, before)| j != i && *before == a)
                })
            });
        if !works {
            *fix = None;
        }
    }
}

/// `expr` with every factor written `written` replaced (distribution
/// repeats it in every term of a group).
fn replace_factor(expr: &Expr, written: &str, replacement: &Factor) -> Expr {
    let terms = expr.terms.iter().map(|term| {
        let factors = term.factors.iter().map(|factor| {
            if factor.to_string() == written {
                replacement.clone()
            } else {
                factor.clone()
            }
        });
        Term {
            source: term.source.clone(),
            factors: factors.collect(),
        }
    });
    Expr {
        terms: terms.collect(),
    }
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

/// `!profile:x`, `!filter:x`, `filter:x` as a factor.
fn factor_from(text: &str) -> Option<Factor> {
    if let Some(name) = text.strip_prefix("!profile:") {
        Some(Factor::NotProfile(name.to_owned()))
    } else if let Some(name) = text.strip_prefix("!filter:") {
        Some(Factor::NotFilter(name.to_owned()))
    } else {
        text.strip_prefix("filter:")
            .map(|name| Factor::Filter(name.to_owned()))
    }
}

/// A syntax error about a name in the wrong position, restated as the
/// wrong-qualifier error when the catalog says the name is of the other
/// sort (`filter:pytest-tmp` alone: "`pytest-tmp` is a source profile").
/// The fix is kept only if the expression with that name rewritten is
/// accepted.
pub(super) fn requalify<C: Catalog>(error: &SyntaxError, catalog: &C) -> Option<CheckError> {
    let (name, written, is, fix) = match &error.problem {
        Syntax::FilterStartsTerm(name) | Syntax::FilterInGroup(name) => {
            match catalog.lookup(name)? {
                Entry::Source { kind, .. } => (
                    name,
                    format!("filter:{name}"),
                    Is::Source(kind),
                    format!("profile:{name}"),
                ),
                Entry::Filter { .. } => return None,
            }
        }
        Syntax::TwoSources(name) => match catalog.lookup(name)? {
            Entry::Filter { .. } => (
                name,
                format!("profile:{name}"),
                Is::Filter,
                format!("filter:{name}"),
            ),
            Entry::Source { .. } => return None,
        },
        _ => return None,
    };
    let chars: Vec<char> = error.expr.chars().collect();
    let at = error.column - 1;
    let fix = chars
        .get(at..at + written.chars().count())
        .filter(|token| token.iter().copied().eq(written.chars()))
        .map(|_| {
            let mut text: String = chars[..at].iter().collect();
            text.push_str(&fix);
            text.extend(&chars[at + written.chars().count()..]);
            text
        })
        .filter(|rewritten| accepted(rewritten, catalog))
        .map(|_| fix);
    Some(CheckError::WrongQualifier {
        written,
        name: name.clone(),
        is,
        fix,
    })
}

/// Whether `text` parses and breaks no static rule.
pub(super) fn accepted<C: Catalog>(text: &str, catalog: &C) -> bool {
    parse(text).is_ok_and(|expr| {
        let (kind, errors) = rules(&expr, catalog);
        kind.is_some() && errors.is_empty()
    })
}

/// Errors in the order found, each once.
#[derive(Default)]
struct Errors(Vec<CheckError>);

impl Errors {
    fn push(&mut self, error: CheckError) {
        if !self.0.contains(&error) {
            self.0.push(error);
        }
    }
}
