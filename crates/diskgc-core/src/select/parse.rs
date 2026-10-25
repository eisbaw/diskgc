//! Text to the normalised AST: the grammar in docs/reference.md (Selection
//! grammar), production by production, with an error for every way out of
//! it that says what to write instead.
//!
//! Where an error suggests a rewritten expression (the two readings of an
//! unparenthesised mix, a group without its parentheses), the suggestion is
//! re-parsed first and left out if it would be rejected too, so diskgc never
//! tells anyone to write something it refuses.

use std::fmt;

use super::{Expr, Factor, Term, distribute};
use crate::suggest::{MAX_NAME_CHARS, edit_distance};

/// Why an expression does not parse, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    /// The expression as given.
    pub expr: String,
    /// 1-based character column of the offending token (one past the end
    /// for "end of the expression").
    pub column: usize,
    pub problem: Syntax,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid selection `{}` at column {}: {}",
            self.expr.escape_debug(),
            self.column,
            self.problem
        )
    }
}

impl std::error::Error for SyntaxError {}

/// The syntax rules an expression can break. `Display` is the message;
/// docs/reference.md (Selection grammar, rejected expressions) quotes some
/// of them and a test keeps the two equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Syntax {
    Empty,
    UnexpectedChar(char),
    /// A single `&` or `|`.
    SingleOperator(char),
    UnknownQualifier(String),
    /// `profile:` or `filter:` with no name right after it.
    MissingName(String),
    /// A name without `profile:` or `filter:`.
    Unqualified(String),
    /// A word that is not a NAME (`[a-z0-9][a-z0-9_-]*`).
    InvalidName(String),
    Expected {
        expected: &'static str,
        found: String,
    },
    UnmatchedClose,
    /// `||` and `&&` at one level. The readings are the parenthesised
    /// rewrites that parse (none, one or two).
    Mixed {
        readings: Vec<String>,
    },
    /// `… && filter:f || filter:g`: `||` used on filters.
    FilterUnion,
    /// A term (or a group) that starts with `filter:NAME`.
    FilterStartsTerm(String),
    /// A term that starts with `!`.
    NotStartsTerm,
    /// `&& profile:NAME`: a second source in a term.
    TwoSources(String),
    /// `!( … )`, with `!a && !b` when the group is a plain list of names.
    NegatedGroup {
        suggestion: Option<String>,
    },
    DoubleNegation,
    /// `(profile:a)`: parentheses around one profile.
    ParenOne {
        suggestion: Option<String>,
    },
    /// A profile group not followed by `&&`: `(profile:a || profile:b)`
    /// alone, `(…) || …`, `((…))`.
    UnnarrowedGroup {
        suggestion: Option<String>,
    },
    /// `(profile:a && filter:f)` on its own.
    LoneParenTerm {
        suggestion: Option<String>,
    },
    /// `(profile:a && filter:f) && filter:g`
    NarrowedParenTerm,
    NotInGroup,
    FilterInGroup(String),
    /// Parentheses deeper than a group inside a union member.
    TooDeep,
    /// `&& (…)`
    GroupedFactor,
}

impl Syntax {
    /// The suggested rewrites (whole expressions) this error offers.
    pub fn suggestions(&self) -> Vec<&str> {
        match self {
            Self::Mixed { readings } => readings.iter().map(String::as_str).collect(),
            Self::ParenOne { suggestion }
            | Self::UnnarrowedGroup { suggestion }
            | Self::LoneParenTerm { suggestion } => suggestion.as_deref().into_iter().collect(),
            _ => Vec::new(),
        }
    }

    /// Drop the suggested rewrites that `keep` refuses (compile: those the
    /// static rules reject).
    pub(super) fn retain_suggestions(&mut self, keep: impl Fn(&str) -> bool) {
        match self {
            Self::Mixed { readings } => readings.retain(|r| keep(r)),
            Self::ParenOne { suggestion }
            | Self::UnnarrowedGroup { suggestion }
            | Self::LoneParenTerm { suggestion } => {
                *suggestion = suggestion.take().filter(|s| keep(s));
            }
            _ => {}
        }
    }
}

impl fmt::Display for Syntax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the expression is empty: it needs a `profile:NAME`"),
            Self::UnexpectedChar(c) => write!(
                f,
                "unexpected character {c:?}: an expression has only `profile:NAME`, \
                 `filter:NAME`, `&&`, `||`, `!` and parentheses"
            ),
            Self::InvalidName(name) => write!(
                f,
                "`{name}` is not a valid name: a name is lowercase letters, digits, `-` and \
                 `_`, starting with a letter or a digit"
            ),
            Self::SingleOperator(c) => write!(f, "a single `{c}`: the operator is `{c}{c}`"),
            Self::UnknownQualifier(q) => {
                write!(
                    f,
                    "unknown qualifier `{q}:`: names are written `profile:NAME` or `filter:NAME`"
                )?;
                if let Some(qualifier) = closest_qualifier(q) {
                    write!(f, "; did you mean `{qualifier}:`?")?;
                }
                Ok(())
            }
            Self::MissingName(q) => write!(f, "`{q}:` must be followed directly by a name"),
            Self::Unqualified(name) => write!(
                f,
                "`{name}` has no qualifier: write `profile:{name}` for a source profile or \
                 `filter:{name}` for a filter"
            ),
            Self::Expected { expected, found } => write!(f, "expected {expected}, found {found}"),
            Self::UnmatchedClose => f.write_str("`)` without a matching `(`"),
            Self::Mixed { readings } => {
                f.write_str("`||` and `&&` cannot be mixed without parentheses; ")?;
                match readings.as_slice() {
                    [] => f.write_str("no suggested parenthesisation is valid; rewrite as valid terms or separate tiers"),
                    [one] => write!(f, "write `{one}`"),
                    [first, rest @ ..] => {
                        write!(f, "write `{first}`")?;
                        for reading in rest {
                            write!(f, " or `{reading}`")?;
                        }
                        Ok(())
                    }
                }
            }
            Self::FilterUnion => f.write_str(
                "`||` joins profiles and parenthesised terms, not filters: filters only narrow \
                 a term with `&&`; for candidates that pass either filter, write a union of \
                 two terms",
            ),
            Self::FilterStartsTerm(name) => write!(
                f,
                "`filter:{name}` cannot start a term: a term is a `profile:` (or a \
                 parenthesised group of them) narrowed with `&& filter:{name}`"
            ),
            Self::NotStartsTerm => f.write_str(
                "`!` only narrows: a term starts with a `profile:`, and `!` follows `&&` \
                 (`profile:… && !profile:…`)",
            ),
            Self::TwoSources(name) => write!(
                f,
                "one source per term: `profile:{name}` cannot follow `&&`; to exclude its \
                 matches write `!profile:{name}`, to add them write a union with `||`"
            ),
            Self::NegatedGroup { suggestion } => {
                f.write_str("`!` applies to one name, not a group: ")?;
                match suggestion {
                    Some(s) => write!(f, "write `{s}`"),
                    None => f.write_str("negate each name and join them with `&&`"),
                }
            }
            Self::DoubleNegation => f.write_str("`!` cannot be repeated"),
            Self::ParenOne { suggestion } => {
                f.write_str("parentheses around one profile have no purpose: ")?;
                match suggestion {
                    Some(s) => write!(f, "write `{s}`"),
                    None => f.write_str("drop them"),
                }
            }
            Self::UnnarrowedGroup { suggestion } => {
                f.write_str(
                    "a profile group must be followed by `&&`: parentheses group profiles only \
                     to share factors; ",
                )?;
                match suggestion {
                    Some(s) => write!(f, "write `{s}`"),
                    None => f.write_str("list its profiles in the union"),
                }
            }
            Self::LoneParenTerm { suggestion } => {
                f.write_str("a parenthesised term stands only as a member of a union")?;
                match suggestion {
                    Some(s) => write!(f, ": write `{s}`"),
                    None => Ok(()),
                }
            }
            Self::NarrowedParenTerm => f.write_str(
                "a parenthesised term cannot be narrowed after its `)`: move the `&&` factors \
                 inside the parentheses",
            ),
            Self::NotInGroup => f.write_str(
                "a profile group holds only `profile:` names joined by `||`; `!` goes after \
                 the group: `(profile:a || profile:b) && !profile:c`",
            ),
            Self::FilterInGroup(name) => write!(
                f,
                "a profile group holds only `profile:` names joined by `||`; filters follow \
                 the group: `(profile:a || profile:b) && filter:{name}`"
            ),
            Self::TooDeep => f.write_str(
                "parentheses nest only as a profile group inside a union member, as in \
                 `((profile:a || profile:b) && filter:f) || profile:c`",
            ),
            Self::GroupedFactor => f.write_str(
                "parentheses after `&&`: filters and negations are single names \
                 (`&& filter:f && !profile:p`); only profiles are grouped",
            ),
        }
    }
}

fn closest_qualifier(written: &str) -> Option<&'static str> {
    let written_len = written.chars().count();
    if written_len > MAX_NAME_CHARS {
        return None;
    }
    ["profile", "filter"]
        .into_iter()
        .filter(|known| written_len.abs_diff(known.chars().count()) <= 2)
        .filter(|known| edit_distance(written, known) <= 2)
        .min_by_key(|known| (edit_distance(written, known), *known))
}

/// Whether `s` is a valid NAME in the grammar, `[a-z0-9][a-z0-9_-]*`:
/// lowercase ASCII letters, digits, `-` and `_`, starting with a letter or
/// a digit. Profile names must satisfy it, or the profile could not be
/// selected.
pub fn is_name(s: &str) -> bool {
    let lower_or_digit = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut chars = s.chars();
    chars.next().is_some_and(lower_or_digit)
        && chars.all(|c| lower_or_digit(c) || c == '-' || c == '_')
}

/// Parse and normalise an expression.
pub fn parse(text: &str) -> Result<Expr, SyntaxError> {
    parse_with(text, true)
}

/// `suggest: false` leaves the suggestions out of errors; it is what checks
/// a suggestion, so checking never recurses.
fn parse_with(text: &str, suggest: bool) -> Result<Expr, SyntaxError> {
    let error = |(column, problem)| SyntaxError {
        expr: text.to_owned(),
        column,
        problem,
    };
    let toks = lex(text).map_err(error)?;
    let mut parser = Parser {
        text,
        toks,
        pos: 0,
        suggest,
    };
    parser.expr()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Qual {
    Profile,
    Filter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Open,
    Close,
    And,
    Or,
    Not,
    Name(Qual, String),
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => f.write_str("("),
            Self::Close => f.write_str(")"),
            Self::And => f.write_str("&&"),
            Self::Or => f.write_str("||"),
            Self::Not => f.write_str("!"),
            Self::Name(Qual::Profile, name) => write!(f, "profile:{name}"),
            Self::Name(Qual::Filter, name) => write!(f, "filter:{name}"),
        }
    }
}

#[derive(Debug)]
struct Lexed {
    tok: Tok,
    /// 1-based character column.
    column: usize,
}

/// What the lexer reads as one word (a qualifier or a name). Wider than a
/// NAME, so that `profile:Foo` is reported as an invalid name rather than
/// as an unexpected character.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

fn lex(text: &str) -> Result<Vec<Lexed>, (usize, Syntax)> {
    let chars: Vec<char> = text.chars().collect();
    let take_name = |i: &mut usize| -> String {
        let start = *i;
        while chars.get(*i).copied().is_some_and(is_word_char) {
            *i += 1;
        }
        chars[start..*i].iter().collect()
    };
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let column = i + 1;
        let tok = match c {
            c if c.is_whitespace() => {
                i += 1;
                continue;
            }
            '(' | ')' | '!' => {
                i += 1;
                match c {
                    '(' => Tok::Open,
                    ')' => Tok::Close,
                    _ => Tok::Not,
                }
            }
            '&' | '|' => {
                if chars.get(i + 1) != Some(&c) {
                    return Err((column, Syntax::SingleOperator(c)));
                }
                i += 2;
                if c == '&' { Tok::And } else { Tok::Or }
            }
            c if is_word_char(c) => {
                let word = take_name(&mut i);
                if chars.get(i) != Some(&':') {
                    let problem = if is_name(&word) {
                        Syntax::Unqualified(word)
                    } else {
                        Syntax::InvalidName(word)
                    };
                    return Err((column, problem));
                }
                i += 1;
                let qual = match word.as_str() {
                    "profile" => Qual::Profile,
                    "filter" => Qual::Filter,
                    _ => return Err((column, Syntax::UnknownQualifier(word))),
                };
                let name_column = i + 1;
                let name = take_name(&mut i);
                if name.is_empty() {
                    return Err((column, Syntax::MissingName(word)));
                }
                if !is_name(&name) {
                    return Err((name_column, Syntax::InvalidName(name)));
                }
                Tok::Name(qual, name)
            }
            other => return Err((column, Syntax::UnexpectedChar(other))),
        };
        out.push(Lexed { tok, column });
    }
    Ok(out)
}

/// What starts an expression or a union member, before the next operator
/// says which production it belongs to.
enum Lead {
    /// `profile:a`: a term's source or a union member.
    Profile(String),
    /// `(profile:a || profile:b)`, two or more: only a term's sources, so
    /// `&&` must follow.
    Group(Vec<String>),
    /// `(profile:a && …)` or `((…) && …)`: only a union member; already
    /// distributed.
    ParenTerm(Vec<Term>),
}

struct Parser<'a> {
    text: &'a str,
    toks: Vec<Lexed>,
    pos: usize,
    suggest: bool,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|l| &l.tok)
    }

    fn bump(&mut self) -> Option<Tok> {
        let tok = self.toks.get(self.pos).map(|l| l.tok.clone());
        self.pos += 1;
        tok
    }

    fn error(&self, at: usize, problem: Syntax) -> SyntaxError {
        let column = match self.toks.get(at) {
            Some(lexed) => lexed.column,
            None => self.text.chars().count() + 1,
        };
        SyntaxError {
            expr: self.text.to_owned(),
            column,
            problem,
        }
    }

    fn fail<T>(&self, at: usize, problem: Syntax) -> Result<T, SyntaxError> {
        Err(self.error(at, problem))
    }

    /// The token at `at` is not one of `expected`.
    fn unexpected(&self, at: usize, expected: &'static str) -> SyntaxError {
        let problem = match self.toks.get(at) {
            Some(lexed) => Syntax::Expected {
                expected,
                found: format!("`{}`", lexed.tok),
            },
            None => Syntax::Expected {
                expected,
                found: "the end of the expression".to_owned(),
            },
        };
        self.error(at, problem)
    }

    /// Like [`Self::unexpected`], at the top level, where no `(` is open.
    fn trailing(&self, expected: &'static str) -> SyntaxError {
        match self.peek() {
            Some(Tok::Close) => self.error(self.pos, Syntax::UnmatchedClose),
            _ => self.unexpected(self.pos, expected),
        }
    }

    /// `expr := term | union`
    fn expr(&mut self) -> Result<Expr, SyntaxError> {
        if self.toks.is_empty() {
            return self.fail(0, Syntax::Empty);
        }
        let start = self.pos;
        let lead = self.lead()?;
        match self.peek() {
            // term := profile
            None => match lead {
                Lead::Profile(name) => Ok(Expr {
                    terms: distribute(vec![name], &[]),
                }),
                Lead::Group(_) => Err(self.unnarrowed_group(start)),
                Lead::ParenTerm(_) => {
                    let suggestion = self.suggest(self.without(&[start, self.pos - 1]));
                    self.fail(start, Syntax::LoneParenTerm { suggestion })
                }
            },
            // term := nterm := sources ( '&&' factor )+
            Some(Tok::And) => {
                let sources = match lead {
                    Lead::Profile(name) => vec![name],
                    Lead::Group(names) => names,
                    Lead::ParenTerm(_) => return self.fail(self.pos, Syntax::NarrowedParenTerm),
                };
                let factors = self.factors()?;
                match self.peek() {
                    None => Ok(Expr {
                        terms: distribute(sources, &factors),
                    }),
                    Some(Tok::Or) => Err(self.mixed_and_or(start)),
                    _ => Err(self.trailing("`&&`, `||` or the end of the expression")),
                }
            }
            // union := uterm ( '||' uterm )+
            Some(Tok::Or) => {
                let mut terms = self.member(lead, start)?;
                while self.peek() == Some(&Tok::Or) {
                    self.pos += 1;
                    let member = self.pos;
                    let lead = self.lead()?;
                    if matches!(lead, Lead::Group(_)) && self.peek() == Some(&Tok::And) {
                        // `… || (p || q) && f`: a mix, not a bare group.
                        return Err(self.mixed_or_and(start, member));
                    }
                    terms.extend(self.member(lead, member)?);
                    match self.peek() {
                        None | Some(Tok::Or) => {}
                        Some(Tok::And) => return Err(self.mixed_or_and(start, member)),
                        _ => {
                            return Err(self.trailing("`||` or the end of the expression"));
                        }
                    }
                }
                Ok(Expr { terms })
            }
            _ => Err(self.trailing("`&&`, `||` or the end of the expression")),
        }
    }

    /// `uterm := profile | '(' nterm ')'`, from a lead just parsed at
    /// `start`.
    fn member(&self, lead: Lead, start: usize) -> Result<Vec<Term>, SyntaxError> {
        match lead {
            Lead::Profile(name) => Ok(distribute(vec![name], &[])),
            Lead::ParenTerm(terms) => Ok(terms),
            Lead::Group(_) => Err(self.unnarrowed_group(start)),
        }
    }

    /// A group just parsed from `open` to the previous token, with no `&&`
    /// after it.
    fn unnarrowed_group(&self, open: usize) -> SyntaxError {
        let suggestion = self.suggest(self.without(&[open, self.pos - 1]));
        self.error(open, Syntax::UnnarrowedGroup { suggestion })
    }

    /// A profile, or a parenthesised group or term.
    fn lead(&mut self) -> Result<Lead, SyntaxError> {
        let at = self.pos;
        match self.bump() {
            Some(Tok::Name(Qual::Profile, name)) => Ok(Lead::Profile(name)),
            Some(Tok::Open) => self.paren(at),
            Some(Tok::Name(Qual::Filter, name)) => self.fail(at, Syntax::FilterStartsTerm(name)),
            Some(Tok::Not) => Err(self.misplaced_not(at)),
            _ => Err(self.unexpected(at, "a `profile:` name")),
        }
    }

    /// After the `(` at `open`: a group, `(profile:a && …)`, or
    /// `((group) && …)`. Every parenthesis must have a purpose: a group has
    /// two or more profiles, a parenthesised term has `&&`.
    fn paren(&mut self, open: usize) -> Result<Lead, SyntaxError> {
        let at = self.pos;
        match self.bump() {
            Some(Tok::Name(Qual::Profile, first)) => match self.peek() {
                Some(Tok::Close) => Err(self.paren_one(open, None)),
                Some(Tok::Or) => Ok(Lead::Group(self.group_rest(first, open)?)),
                Some(Tok::And) => {
                    let factors = self.factors()?;
                    self.close_term(open)?;
                    Ok(Lead::ParenTerm(distribute(vec![first], &factors)))
                }
                _ => Err(self.unexpected(self.pos, "`||`, `&&` or `)`")),
            },
            Some(Tok::Open) => {
                let inner = at;
                let names = match self.bump() {
                    Some(Tok::Name(Qual::Profile, first)) => match self.peek() {
                        Some(Tok::Close) => return Err(self.paren_one(inner, Some(open))),
                        Some(Tok::Or) => self.group_rest(first, inner)?,
                        Some(Tok::And) => return self.fail(self.pos, Syntax::TooDeep),
                        _ => return Err(self.unexpected(self.pos, "`||` or `)`")),
                    },
                    Some(Tok::Open) => return self.fail(inner + 1, Syntax::TooDeep),
                    Some(Tok::Name(Qual::Filter, name)) => {
                        return self.fail(inner + 1, Syntax::FilterStartsTerm(name));
                    }
                    Some(Tok::Not) => return self.fail(inner + 1, Syntax::NotInGroup),
                    _ => return Err(self.unexpected(inner + 1, "a `profile:` name")),
                };
                match self.peek() {
                    Some(Tok::And) => {
                        let factors = self.factors()?;
                        self.close_term(open)?;
                        Ok(Lead::ParenTerm(distribute(names, &factors)))
                    }
                    // `((p || q))`: both pairs are pointless.
                    Some(Tok::Close) => {
                        let drop = [open, inner, self.pos - 1, self.pos];
                        let suggestion = self.suggest(self.without(&drop));
                        self.fail(inner, Syntax::UnnarrowedGroup { suggestion })
                    }
                    Some(Tok::Or) => self.fail(self.pos, Syntax::TooDeep),
                    _ => Err(self.unexpected(self.pos, "`&&` or `)`")),
                }
            }
            Some(Tok::Name(Qual::Filter, name)) => self.fail(at, Syntax::FilterStartsTerm(name)),
            Some(Tok::Not) => Err(self.misplaced_not(at)),
            _ => Err(self.unexpected(at, "a `profile:` name")),
        }
    }

    /// `(profile:a)`, opened at `open`, with the `)` just ahead; `outer`
    /// is the `(` right before it, if any. Suggests dropping the pair, or
    /// both pairs when that is what it takes (`((profile:a))`).
    fn paren_one(&self, open: usize, outer: Option<usize>) -> SyntaxError {
        let close = self.pos;
        let mut suggestion = self.suggest(self.without(&[open, close]));
        if suggestion.is_none()
            && let Some(outer) = outer
            && self.toks.get(close + 1).map(|l| &l.tok) == Some(&Tok::Close)
        {
            suggestion = self.suggest(self.without(&[outer, open, close, close + 1]));
        }
        self.error(open, Syntax::ParenOne { suggestion })
    }

    /// The rest of `'(' profile ( '||' profile )+ ')'`, at the first `||`.
    fn group_rest(&mut self, first: String, open: usize) -> Result<Vec<String>, SyntaxError> {
        let mut names = vec![first];
        let mut last = self.pos - 1;
        loop {
            match self.peek() {
                Some(Tok::Or) => {
                    self.pos += 1;
                    let at = self.pos;
                    match self.bump() {
                        Some(Tok::Name(Qual::Profile, name)) => {
                            names.push(name);
                            last = at;
                        }
                        Some(Tok::Name(Qual::Filter, name)) => {
                            return self.fail(at, Syntax::FilterInGroup(name));
                        }
                        Some(Tok::Not) => return self.fail(at, Syntax::NotInGroup),
                        Some(Tok::Open) => return self.fail(at, Syntax::TooDeep),
                        _ => return Err(self.unexpected(at, "a `profile:` name")),
                    }
                }
                Some(Tok::Close) => {
                    self.pos += 1;
                    return Ok(names);
                }
                Some(Tok::And) => return Err(self.mixed_or_and(open + 1, last)),
                _ => return Err(self.unexpected(self.pos, "`||` or `)`")),
            }
        }
    }

    /// The `)` closing a parenthesised term opened at `open`.
    fn close_term(&mut self, open: usize) -> Result<(), SyntaxError> {
        match self.peek() {
            Some(Tok::Close) => {
                self.pos += 1;
                Ok(())
            }
            Some(Tok::Or) => Err(self.mixed_and_or(open + 1)),
            _ => Err(self.unexpected(self.pos, "`&&` or `)`")),
        }
    }

    /// `( '&&' factor )*`
    fn factors(&mut self) -> Result<Vec<Factor>, SyntaxError> {
        let mut factors = Vec::new();
        while self.peek() == Some(&Tok::And) {
            self.pos += 1;
            factors.push(self.factor()?);
        }
        Ok(factors)
    }

    /// `factor := filter | '!' filter | '!' profile`
    fn factor(&mut self) -> Result<Factor, SyntaxError> {
        let at = self.pos;
        match self.bump() {
            Some(Tok::Name(Qual::Filter, name)) => Ok(Factor::Filter(name)),
            Some(Tok::Name(Qual::Profile, name)) => self.fail(at, Syntax::TwoSources(name)),
            Some(Tok::Not) => {
                let name_at = self.pos;
                match self.bump() {
                    Some(Tok::Name(Qual::Filter, name)) => Ok(Factor::NotFilter(name)),
                    Some(Tok::Name(Qual::Profile, name)) => Ok(Factor::NotProfile(name)),
                    Some(Tok::Not) => self.fail(name_at, Syntax::DoubleNegation),
                    Some(Tok::Open) => Err(self.negated_group(at)),
                    _ => Err(self.unexpected(name_at, "a `filter:` or `profile:` name after `!`")),
                }
            }
            Some(Tok::Open) => self.fail(at, Syntax::GroupedFactor),
            _ => Err(self.unexpected(at, "a `filter:` name or `!` after `&&`")),
        }
    }

    /// A `!` at `at` where a term starts.
    fn misplaced_not(&self, at: usize) -> SyntaxError {
        match self.toks.get(at + 1) {
            Some(Lexed { tok: Tok::Open, .. }) => self.negated_group(at),
            _ => self.error(at, Syntax::NotStartsTerm),
        }
    }

    /// `!(` at `at`: suggest `!a && !b` when the group is a plain list.
    fn negated_group(&self, at: usize) -> SyntaxError {
        let mut names = Vec::new();
        let mut i = at + 2;
        let suggestion = loop {
            match self.toks.get(i).map(|l| &l.tok) {
                Some(name @ Tok::Name(..)) => names.push(format!("!{name}")),
                _ => break None,
            }
            match self.toks.get(i + 1).map(|l| &l.tok) {
                Some(Tok::Or) => i += 2,
                Some(Tok::Close) => break Some(names.join(" && ")),
                _ => break None,
            }
        };
        self.error(at, Syntax::NegatedGroup { suggestion })
    }

    /// `… || member && …`, the `&&` at the current position: the union
    /// from `start` filtered as a whole, or only its last member (at
    /// `member`).
    fn mixed_or_and(&self, start: usize, member: usize) -> SyntaxError {
        let and = self.pos;
        if and + 1 == self.toks.len() {
            return self.unexpected(and + 1, "a `filter:` name or `!` after `&&`");
        }
        let end = self.run_end(and);
        let readings = [
            self.wrapped(&[(start, and)]),
            self.wrapped(&[(member, end)]),
        ];
        let readings = readings.into_iter().filter_map(|r| self.suggest(r));
        self.error(
            and,
            Syntax::Mixed {
                readings: readings.collect(),
            },
        )
    }

    /// `… && … || …`, the `||` at the current position: the term from
    /// `start` parenthesised (and the next member too if it has `&&`).
    fn mixed_and_or(&self, start: usize) -> SyntaxError {
        let or = self.pos;
        if or + 1 == self.toks.len() {
            return self.unexpected(or + 1, "a `profile:` name");
        }
        if let Some(Tok::Name(Qual::Filter, _) | Tok::Not) = self.toks.get(or + 1).map(|l| &l.tok) {
            return self.error(or, Syntax::FilterUnion);
        }
        let end = self.run_end(or + 1);
        let mut spans = vec![(start, or)];
        if self.toks[or + 1..end].iter().any(|l| l.tok == Tok::And) {
            spans.push((or + 1, end));
        }
        let readings = self.suggest(self.wrapped(&spans)).into_iter().collect();
        self.error(or, Syntax::Mixed { readings })
    }

    /// The end of the operand run starting at `from`: the first `||` or
    /// unmatched `)` at its own depth, or the end.
    fn run_end(&self, from: usize) -> usize {
        let mut depth = 0usize;
        for (i, lexed) in self.toks.iter().enumerate().skip(from) {
            match lexed.tok {
                Tok::Open => depth += 1,
                Tok::Close if depth == 0 => return i,
                Tok::Close => depth -= 1,
                Tok::Or if depth == 0 => return i,
                _ => {}
            }
        }
        self.toks.len()
    }

    /// The tokens with `(` inserted before each span's start and `)`
    /// before its end.
    fn wrapped(&self, spans: &[(usize, usize)]) -> Vec<Tok> {
        let mut out = Vec::new();
        for i in 0..=self.toks.len() {
            out.extend(spans.iter().filter(|s| s.1 == i).map(|_| Tok::Close));
            out.extend(spans.iter().filter(|s| s.0 == i).map(|_| Tok::Open));
            if let Some(lexed) = self.toks.get(i) {
                out.push(lexed.tok.clone());
            }
        }
        out
    }

    /// The tokens without those at the given indices.
    fn without(&self, drop: &[usize]) -> Vec<Tok> {
        let kept = self.toks.iter().enumerate();
        kept.filter(|(i, _)| !drop.contains(i))
            .map(|(_, lexed)| lexed.tok.clone())
            .collect()
    }

    /// The rendered tokens, if suggestions are on and the text parses.
    fn suggest(&self, toks: Vec<Tok>) -> Option<String> {
        if !self.suggest {
            return None;
        }
        let text = render(&toks);
        parse_with(&text, false).is_ok().then_some(text)
    }
}

/// Canonical spacing: one space around binary operators, none elsewhere.
fn render(toks: &[Tok]) -> String {
    let mut text = String::new();
    for tok in toks {
        match tok {
            Tok::And => text.push_str(" && "),
            Tok::Or => text.push_str(" || "),
            other => text.push_str(&other.to_string()),
        }
    }
    text
}
