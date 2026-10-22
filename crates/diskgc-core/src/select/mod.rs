//! The selection grammar (docs/reference.md, Selection grammar): the
//! expressions in a tier's `select` and in `--select`.
//!
//! Two steps, kept apart so each can be used and tested on its own:
//!
//! 1. [`parse`] turns text into the normalised AST ([`Expr`]). It needs
//!    nothing but the text: qualifiers, parentheses, `||` / `&&` mixing and
//!    `!` are syntax. Normalisation is distribution of profile groups
//!    ([`distribute`]), so a parsed expression is always a union of terms
//!    with one source each.
//! 2. [`check`] runs the static rules that need to know what each name is
//!    (a [`Catalog`]): unknown names, wrong qualifiers, one kind and one
//!    action per expression, predicates the kind supports, the shapes the
//!    non-`fs` kinds allow, and what `!profile:` may name.
//!
//! [`compile`] is both, for the two callers (the config's tiers and
//! `--select`); it also turns a syntax error about a name in the wrong
//! position into the wrong-qualifier error that names what it is, and keeps
//! only the suggested rewrites that would be accepted. Its errors quote the
//! expression as written.
//!
//! The AST is plain data. [`Expr`]'s `Display` is the normalised text
//! (`diskgc check` prints it), and parsing that text gives the same AST.

mod check;
mod evaluate;
mod parse;

#[cfg(test)]
mod tests;

use std::fmt;

pub use crate::kind::{Kind, PredicateName};
pub use check::{Catalog, CheckError, CheckErrors, Checked, Entry, Is, check};
pub use evaluate::{Facts, FilterVerdict, eligible, overlaps};
pub use parse::{Syntax, SyntaxError, is_name, parse};

/// A parsed, normalised expression: a union of terms (`||`), in the order
/// written, after distribution. Never empty when it comes from [`parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    pub terms: Vec<Term>,
}

/// One source profile narrowed by factors (`&&`), in the order written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// The source profile's name (written `profile:NAME`).
    pub source: String,
    pub factors: Vec<Factor>,
}

/// What may follow `&&`. There is no positive profile factor: a term has
/// one source (docs/reference.md, "One source per term").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Factor {
    /// `filter:NAME`
    Filter(String),
    /// `!filter:NAME`: the path-local complement.
    NotFilter(String),
    /// `!profile:NAME`: removes candidates overlapping that profile's matches.
    NotProfile(String),
}

/// `(s1 || s2 || …) && f1 && …` as the terms `(s1 && f1 && …) || (s2 && f1
/// && …) || …`, sources in the order given. A single source gives one term.
pub fn distribute(sources: Vec<String>, factors: &[Factor]) -> Vec<Term> {
    sources
        .into_iter()
        .map(|source| Term {
            source,
            factors: factors.to_vec(),
        })
        .collect()
}

/// Parse, then [`check`] against `catalog`. Errors quote `text` as
/// written.
pub fn compile<C: Catalog>(text: &str, catalog: &C) -> Result<Checked, SelectError> {
    match parse(text) {
        Ok(expr) => check(expr, catalog).map_err(|errors| {
            SelectError::Check(CheckErrors {
                expr: text.to_owned(),
                ..errors
            })
        }),
        Err(mut syntax) => Err(match check::requalify(&syntax, catalog) {
            Some(error) => SelectError::Check(CheckErrors {
                expr: text.to_owned(),
                column: Some(syntax.column),
                errors: vec![error],
            }),
            None => {
                // The parser offers rewrites that parse; offer only those
                // the static rules accept too.
                syntax
                    .problem
                    .retain_suggestions(|s| check::accepted(s, catalog));
                SelectError::Syntax(syntax)
            }
        }),
    }
}

/// Why [`compile`] refused an expression. Either way a load-time error:
/// exit 2, nothing deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    Syntax(SyntaxError),
    Check(CheckErrors),
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(e) => e.fmt(f),
            Self::Check(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for SelectError {}

impl fmt::Display for Factor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Filter(name) => write!(f, "filter:{name}"),
            Self::NotFilter(name) => write!(f, "!filter:{name}"),
            Self::NotProfile(name) => write!(f, "!profile:{name}"),
        }
    }
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "profile:{}", self.source)?;
        for factor in &self.factors {
            write!(f, " && {factor}")?;
        }
        Ok(())
    }
}

/// The normalised text: one term as written (`profile:a && filter:f`); a
/// union with every term that has factors in parentheses
/// (`(profile:a && filter:f) || profile:b`). It parses back to `self`.
impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let [term] = self.terms.as_slice() {
            return term.fmt(f);
        }
        for (i, term) in self.terms.iter().enumerate() {
            if i > 0 {
                f.write_str(" || ")?;
            }
            if term.factors.is_empty() {
                term.fmt(f)?;
            } else {
                write!(f, "({term})")?;
            }
        }
        Ok(())
    }
}
