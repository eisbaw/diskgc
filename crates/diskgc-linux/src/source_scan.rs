//! Source scans of the production crates (test-only).
//!
//! The scans enforce structural rules that the type system cannot:
//! deleting operations only behind the pre-release sandbox gate
//! (docs/safety.md), and `/proc` only through `LinuxHost`'s held fd. They
//! read Rust source as text, so they are deliberately strict about what
//! they accept:
//!
//! - comments are blanked out, and so are the contents of string and char
//!   literals in the code view ([`Source::code`]); a block comment or a raw
//!   string literal is an error, not something to guess about;
//! - test code is excluded by **module**: every `#[cfg(test)]` must gate a
//!   `mod` (inline: its braces are skipped, and code after it is scanned;
//!   out of line: that module's file is excluded). Any other `#[cfg(test)]`
//!   is an error, so a test-only item can never hide production code below
//!   it;
//! - every `.rs` file below a crate's `src/` is scanned, found by walking
//!   the directory tree, so a new or nested module is covered without
//!   editing a list.
//!
//! - names the scan matches by path cannot be hidden: production code may
//!   not rename an import, glob-import, or load a module by `#[path]`
//!   ([`hiding_forms`]); production crates have no build script and no
//!   target outside `src/` ([`layout_problems`]); and their dependencies
//!   and features are a reviewed list ([`PRODUCTION_DEPENDENCIES`]);
//! - the core and the CLI may not reach the OS at all ([`os_access`]); the
//!   detailed list ([`GATED`]) is for `diskgc-linux`, and runs everywhere.
//!
//! The result is a tripwire against ordinary mistakes, not a proof:
//! deliberate obfuscation, a deleting function of an allowlisted dependency
//! under a name the list does not know, and code shapes this text scan
//! does not model can evade it (docs/safety.md, Pre-release sandbox gate).
//!
//! Every rule has a negative control below: a scripted source that breaks it
//! and must be caught.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// One source file, its path relative to the repository root.
#[derive(Debug, Clone)]
pub(crate) struct SourceFile {
    pub path: PathBuf,
    pub text: String,
}

/// The repository root (the workspace this crate belongs to).
pub(crate) fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `.rs` file below `root/rel_dir`, recursively, sorted. A symlink
/// anywhere below is an error: it could lead the compiler to source the
/// walk would not read (a linked directory) or read twice.
pub(crate) fn rust_files(root: &Path, rel_dir: &Path) -> Result<Vec<SourceFile>, String> {
    fn walk(root: &Path, rel: &Path, out: &mut Vec<SourceFile>) -> Result<(), String> {
        let dir = root.join(rel);
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("list {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("entry of {}: {e}", dir.display()))?;
            let rel = rel.join(entry.file_name());
            let kind = entry
                .file_type()
                .map_err(|e| format!("type of {}: {e}", rel.display()))?;
            if kind.is_symlink() {
                return Err(format!("{}: a symlink among the sources", rel.display()));
            } else if kind.is_dir() {
                walk(root, &rel, out)?;
            } else if rel.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(root.join(&rel))
                    .map_err(|e| format!("read {}: {e}", rel.display()))?;
                out.push(SourceFile { path: rel, text });
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, rel_dir, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// A source file with comments blanked (spaces, newlines kept), in two
/// views of the same length: `code` also blanks the contents of string and
/// char literals; `with_strings` keeps them.
#[derive(Debug)]
pub(crate) struct Source {
    pub code: Vec<char>,
    pub with_strings: Vec<char>,
}

/// Blanks comments and literal contents of `text` (see [`Source`]).
pub(crate) fn blank(name: &str, text: &str) -> Result<Source, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut code = chars.clone();
    let mut with_strings = chars.clone();
    let blank_at = |v: &mut Vec<char>, i: usize| {
        if v[i] != '\n' {
            v[i] = ' ';
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match (c, next) {
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' {
                    blank_at(&mut code, i);
                    blank_at(&mut with_strings, i);
                    i += 1;
                }
                continue;
            }
            ('/', Some('*')) => {
                return Err(format!("{name}: block comment: the scan cannot read it"));
            }
            // A raw identifier r#name is the identifier name: blank the
            // `r#` (whitespace does not end a `::` path in the tokenizer),
            // so `fs::r#rename` is read as `fs::rename`.
            ('r', Some('#'))
                if (i == 0 || !is_ident(chars[i - 1]))
                    && chars
                        .get(i + 2)
                        .is_some_and(|&n| n.is_alphabetic() || n == '_') =>
            {
                for j in [i, i + 1] {
                    blank_at(&mut code, j);
                    blank_at(&mut with_strings, j);
                }
                i += 2;
                continue;
            }
            // r"..", r#".."#, br"..", cr"..": raw strings.
            ('r', Some('"' | '#'))
                if i == 0
                    || !is_ident(chars[i - 1])
                    || (matches!(chars[i - 1], 'b' | 'c')
                        && (i == 1 || !is_ident(chars[i - 2]))) =>
            {
                return Err(format!("{name}: raw string: the scan cannot read it"));
            }
            ('"', _) => {
                // Blank the contents (in the code view) up to the closing quote.
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    blank_at(&mut code, i);
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                        blank_at(&mut code, i);
                    }
                    i += 1;
                }
            }
            // A char literal ('x' or an escape); a lifetime is left alone.
            ('\'', Some('\\')) => {
                let start = i;
                // Past the quote, the backslash and the escaped char (which
                // may itself be a quote: '\''), then to the closing quote.
                i += 3;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                for j in start..=i.min(chars.len() - 1) {
                    blank_at(&mut code, j);
                    blank_at(&mut with_strings, j);
                }
            }
            ('\'', Some(_)) if chars.get(i + 2) == Some(&'\'') => {
                for j in i..i + 3 {
                    blank_at(&mut code, j);
                    blank_at(&mut with_strings, j);
                }
                i += 2;
            }
            _ => {}
        }
        i += 1;
    }
    Ok(Source { code, with_strings })
}

/// An identifier in the code view, outside test modules.
#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub word: String,
    /// Char index in the file.
    pub pos: usize,
    /// The path segment before it (`fs` in `std::fs::write`, also for each
    /// name of a grouped import `std::fs::{copy, write}`), if any.
    pub prefix: Option<String>,
    /// The previous identifier (e.g. `fn` for a function's name).
    pub previous: Option<String>,
    /// The first non-blank char after it (`!` for a macro, `(` for a call).
    pub next_char: Option<char>,
}

/// A function body: `fn name` up to its closing brace (char indices).
#[derive(Debug, Clone)]
pub(crate) struct FnSpan {
    pub name: String,
    pub start: usize,
    /// Where the body's `{` is: the signature is `start..body`.
    pub body: usize,
    pub end: usize,
}

/// A production source file, scanned.
#[derive(Debug)]
pub(crate) struct Scanned {
    pub path: PathBuf,
    pub text: String,
    pub source: Source,
    /// Char ranges of inline test modules (excluded from `tokens`).
    pub excluded: Vec<(usize, usize)>,
    pub tokens: Vec<Token>,
    pub fns: Vec<FnSpan>,
}

impl Scanned {
    /// 1-based line of char index `pos`.
    pub fn line_of(&self, pos: usize) -> usize {
        1 + self.source.code[..pos]
            .iter()
            .filter(|&&c| c == '\n')
            .count()
    }

    /// The original text of the line holding `pos`, trimmed.
    pub fn line_text(&self, pos: usize) -> &str {
        self.text
            .lines()
            .nth(self.line_of(pos) - 1)
            .unwrap_or("")
            .trim()
    }

    /// The innermost function containing `pos`.
    pub fn enclosing_fn(&self, pos: usize) -> Option<&str> {
        self.fns
            .iter()
            .filter(|f| f.start <= pos && pos <= f.end)
            .min_by_key(|f| f.end - f.start)
            .map(|f| f.name.as_str())
    }

    pub fn is_excluded(&self, pos: usize) -> bool {
        self.excluded.iter().any(|&(a, b)| a <= pos && pos <= b)
    }

    /// `path:line (in fn f): text` for messages.
    pub fn at(&self, pos: usize) -> String {
        format!(
            "{}:{} (in fn {}): {}",
            self.path.display(),
            self.line_of(pos),
            self.enclosing_fn(pos).unwrap_or("-"),
            self.line_text(pos)
        )
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn skip_ws(code: &[char], mut i: usize) -> usize {
    while i < code.len() && code[i].is_whitespace() {
        i += 1;
    }
    i
}

/// The index of the bracket closing the one opened at `open`.
fn matching(code: &[char], open: usize, path: &Path) -> Result<usize, String> {
    let (o, c) = match code[open] {
        '{' => ('{', '}'),
        '[' => ('[', ']'),
        '(' => ('(', ')'),
        other => {
            return Err(format!(
                "{}: no bracket at {open}: {other:?}",
                path.display()
            ));
        }
    };
    let mut depth = 0usize;
    for (i, &ch) in code.iter().enumerate().skip(open) {
        if ch == o {
            depth += 1;
        } else if ch == c {
            depth -= 1;
            if depth == 0 {
                return Ok(i);
            }
        }
    }
    Err(format!("{}: unbalanced {o} at char {open}", path.display()))
}

/// The identifier starting at `i`, and the index after it.
fn ident_at(code: &[char], i: usize) -> Option<(String, usize)> {
    let mut j = i;
    while j < code.len() && is_ident(code[j]) {
        j += 1;
    }
    (j > i).then(|| (code[i..j].iter().collect(), j))
}

/// What a `#[cfg(test)]` at `at` gates: an inline module (its char range)
/// or an out-of-line module (its name). Anything else is an error.
enum TestModule {
    Inline(usize, usize),
    OutOfLine(String),
}

fn test_module(code: &[char], at: usize, path: &Path) -> Result<TestModule, String> {
    let attr_len = "#[cfg(test)]".chars().count();
    let mut i = skip_ws(code, at + attr_len);
    // Further attributes on the same item.
    while code.get(i) == Some(&'#') && code.get(i + 1) == Some(&'[') {
        i = skip_ws(code, matching(code, i + 1, path)? + 1);
    }
    let mut word = ident_at(code, i);
    if let Some((w, end)) = &word
        && w == "pub"
    {
        let mut j = skip_ws(code, *end);
        if code.get(j) == Some(&'(') {
            j = matching(code, j, path)? + 1;
        }
        word = ident_at(code, skip_ws(code, j));
    }
    let line = 1 + code[..at].iter().filter(|&&c| c == '\n').count();
    let not_a_module = || {
        format!(
            "{}:{line}: #[cfg(test)] must gate a module (`mod tests {{ .. }}` or `mod name;`); a \
             test-only item elsewhere would be skipped by nothing and is refused so the scan \
             cannot be misled",
            path.display()
        )
    };
    let Some((kw, end)) = word else {
        return Err(not_a_module());
    };
    if kw != "mod" {
        return Err(not_a_module());
    }
    let Some((name, end)) = ident_at(code, skip_ws(code, end)) else {
        return Err(not_a_module());
    };
    let j = skip_ws(code, end);
    match code.get(j) {
        Some('{') => Ok(TestModule::Inline(at, matching(code, j, path)?)),
        Some(';') => Ok(TestModule::OutOfLine(name)),
        _ => Err(not_a_module()),
    }
}

/// Where the file of out-of-line module `name`, declared in `file`, lives.
fn module_dir(file: &Path, name: &str) -> PathBuf {
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let parent = file.parent().unwrap_or(Path::new(""));
    if matches!(stem, "lib" | "main" | "mod") {
        parent.join(name)
    } else {
        parent.join(stem).join(name)
    }
}

/// Tokens and function spans of the code view, outside `excluded`.
fn tokenize(
    code: &[char],
    excluded: &[(usize, usize)],
    path: &Path,
) -> Result<(Vec<Token>, Vec<FnSpan>), String> {
    let inside = |pos: usize| excluded.iter().any(|&(a, b)| a <= pos && pos <= b);
    let mut tokens = Vec::new();
    // Grouped imports: (prefix, brace depth inside the group).
    let mut groups: Vec<(String, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut last_word: Option<String> = None;
    let mut path_sep_after: Option<String> = None;
    let mut i = 0;
    while i < code.len() {
        let c = code[i];
        if is_ident(c) && (i == 0 || !is_ident(code[i - 1])) {
            let (word, end) = ident_at(code, i).expect("identifier");
            if !inside(i) {
                let prefix = path_sep_after
                    .take()
                    .or_else(|| groups.last().map(|(p, _)| p.clone()));
                tokens.push(Token {
                    word: word.clone(),
                    pos: i,
                    prefix,
                    previous: last_word.clone(),
                    next_char: code.get(skip_ws(code, end)).copied(),
                });
            }
            path_sep_after = None;
            last_word = Some(word);
            i = end;
            continue;
        }
        match c {
            ':' if code.get(i + 1) == Some(&':') => {
                path_sep_after = last_word.clone();
                i += 2;
                continue;
            }
            '{' => {
                depth += 1;
                if let Some(prefix) = path_sep_after.take() {
                    groups.push((prefix, depth));
                }
            }
            '}' => {
                if groups.last().is_some_and(|&(_, d)| d == depth) {
                    groups.pop();
                }
                depth = depth.saturating_sub(1);
            }
            c if c.is_whitespace() => {}
            _ => path_sep_after = None,
        }
        i += 1;
    }
    let mut fns = Vec::new();
    for (n, t) in tokens.iter().enumerate() {
        if t.word != "fn" {
            continue;
        }
        let Some(name) = tokens.get(n + 1) else {
            continue;
        };
        let mut j = name.pos + name.word.chars().count();
        while j < code.len() && code[j] != '{' && code[j] != ';' {
            j += 1;
        }
        if code.get(j) == Some(&'{') {
            fns.push(FnSpan {
                name: name.word.clone(),
                start: t.pos,
                body: j,
                end: matching(code, j, path)?,
            });
        }
    }
    Ok((tokens, fns))
}

/// Scans `files` (one crate's sources): blanks them, excludes test modules
/// and returns the production files. Errors on anything it cannot read
/// with confidence.
pub(crate) fn scan_crate(files: &[SourceFile]) -> Result<Vec<Scanned>, String> {
    let mut blanked = Vec::new();
    let mut test_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for file in files {
        let source = blank(&file.path.display().to_string(), &file.text)?;
        let needle: Vec<char> = "#[cfg(test)]".chars().collect();
        let mut excluded = Vec::new();
        let mut i = 0;
        while i + needle.len() <= source.code.len() {
            if source.code[i..i + needle.len()] == needle[..] {
                match test_module(&source.code, i, &file.path)? {
                    TestModule::Inline(a, b) => excluded.push((a, b)),
                    TestModule::OutOfLine(name) => {
                        test_dirs.insert(module_dir(&file.path, &name));
                    }
                }
            }
            i += 1;
        }
        blanked.push((file, source, excluded));
    }
    for dir in &test_dirs {
        let exists = files
            .iter()
            .any(|f| f.path == dir.with_extension("rs") || f.path.starts_with(dir));
        if !exists {
            return Err(format!(
                "out-of-line test module {} not found among the scanned files",
                dir.display()
            ));
        }
    }
    let mut out = Vec::new();
    for (file, source, excluded) in blanked {
        let test_only = test_dirs
            .iter()
            .any(|d| file.path == d.with_extension("rs") || file.path.starts_with(d));
        if test_only {
            continue;
        }
        let (tokens, fns) = tokenize(&source.code, &excluded, &file.path)?;
        out.push(Scanned {
            path: file.path.clone(),
            text: file.text.clone(),
            source,
            excluded,
            tokens,
            fns,
        });
    }
    Ok(out)
}

/// What the scan looks for: an identifier, `a::b` (also `a::{.., b}`), a
/// macro (`name!`) or a call or macro (`name(` or `name!`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Pattern {
    Word(&'static str),
    Path(&'static str, &'static str),
    Macro(&'static str),
    CallOrMacro(&'static str),
}

impl Pattern {
    fn matches(self, t: &Token) -> bool {
        // A definition (`fn unlinkat(`) calls nothing.
        if t.previous.as_deref() == Some("fn") {
            return false;
        }
        match self {
            Pattern::Word(w) => t.word == w,
            Pattern::Path(a, b) => t.word == b && t.prefix.as_deref() == Some(a),
            Pattern::Macro(w) => t.word == w && t.next_char == Some('!'),
            Pattern::CallOrMacro(w) => t.word == w && matches!(t.next_char, Some('(' | '!')),
        }
    }
}

/// Whether an allowed place sits behind the sandbox gate.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Gate {
    /// The function takes the gate's `Permit` (checked: its signature names
    /// `Permit`).
    Permit,
    /// Not gated, and why that is right (e.g. diskgc's own state writes,
    /// read-only queries, a caller of a gated port method).
    Ungated(&'static str),
}

/// An allowed place: a function in a file, whether it is gated, and how
/// many matches it holds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Allowed {
    pub file: &'static str,
    pub function: &'static str,
    pub gate: Gate,
    pub matches: usize,
}

/// Problems with `allowed` itself: each entry's function exists exactly
/// once in its file, and a [`Gate::Permit`] entry's signature names
/// `Permit`, so an ungated function cannot pass as gated.
pub(crate) fn check_allowed(scanned: &[Scanned], allowed: &[Allowed]) -> Vec<String> {
    let mut problems = Vec::new();
    for a in allowed {
        let Some(file) = scanned.iter().find(|s| s.path == Path::new(a.file)) else {
            problems.push(format!("{a:?}: file not scanned"));
            continue;
        };
        let spans: Vec<&FnSpan> = file.fns.iter().filter(|f| f.name == a.function).collect();
        let [span] = spans[..] else {
            problems.push(format!("{a:?}: {} functions of that name", spans.len()));
            continue;
        };
        let takes_permit = file
            .tokens
            .iter()
            .any(|t| t.pos > span.start && t.pos < span.body && t.word == "Permit");
        match a.gate {
            Gate::Permit if !takes_permit => {
                problems.push(format!(
                    "{a:?}: marked gated but its signature takes no Permit"
                ));
            }
            Gate::Ungated(reason) if reason.trim().is_empty() => {
                problems.push(format!("{a:?}: an ungated entry needs its reason"));
            }
            _ => {}
        }
    }
    problems
}

/// Matches of `patterns` in `scanned` outside `allowed`, and the count of
/// matches inside each allowed function (same order as `allowed`).
pub(crate) fn find(
    scanned: &[Scanned],
    patterns: &[Pattern],
    allowed: &[Allowed],
) -> (Vec<String>, Vec<usize>) {
    let mut found = Vec::new();
    let mut hits = vec![0; allowed.len()];
    for file in scanned {
        for t in &file.tokens {
            let Some(p) = patterns.iter().find(|p| p.matches(t)) else {
                continue;
            };
            let place = allowed.iter().position(|a| {
                file.path == Path::new(a.file) && file.enclosing_fn(t.pos) == Some(a.function)
            });
            match place {
                Some(k) => hits[k] += 1,
                None => found.push(format!("{} [{p:?}]", file.at(t.pos))),
            }
        }
    }
    (found, hits)
}

/// The crates that ship (the binary and its libraries): every crate in
/// `crates/` except the test-only ones, named with why.
pub(crate) const TEST_ONLY_CRATES: &[(&str, &str)] = &[
    ("diskgc-model", "ModelHost: test-only, deletes nothing real"),
    ("diskgc-modelgen", "host-model generator: test-only"),
];

/// The production crates' sources, scanned. diskgc-e2e is not among them:
/// it is test infrastructure outside the workspace, never part of the
/// product build, and it deletes only inside test containers behind its own
/// interlock (docs/testing.md, Safety interlock).
pub(crate) fn production_sources() -> Vec<Scanned> {
    let repo = repo_root();
    let mut out = Vec::new();
    for name in production_crates() {
        let files = rust_files(&repo, &Path::new("crates").join(&name).join("src"))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(!files.is_empty(), "crate {name} has no sources");
        out.extend(scan_crate(&files).unwrap_or_else(|e| panic!("{e}")));
    }
    out
}

/// The names of the production crates: every directory in `crates/` except
/// [`TEST_ONLY_CRATES`] (each of which must exist).
pub(crate) fn production_crates() -> Vec<String> {
    let mut crates: Vec<String> = std::fs::read_dir(repo_root().join("crates"))
        .expect("list crates")
        .map(|e| {
            e.expect("crate entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    crates.sort();
    for (name, _) in TEST_ONLY_CRATES {
        assert!(
            crates.iter().any(|c| c == name),
            "test-only crate {name} not found: {crates:?}"
        );
    }
    crates.retain(|c| !TEST_ONLY_CRATES.iter().any(|(t, _)| t == c));
    crates
}

/// Code shapes that would hide a name from the scan, refused outright in
/// production code (none is used there): an import rename (`use std::fs as
/// f;`, `extern crate std as s;`; only `as _` is allowed), a glob import
/// (`use std::fs::*;`), and a `#[path]` attribute (a module file outside
/// `src/`, which the walk does not see).
pub(crate) fn hiding_forms(scanned: &[Scanned]) -> Vec<String> {
    let mut found = Vec::new();
    for file in scanned {
        let code = &file.source.code;
        for (n, t) in file.tokens.iter().enumerate() {
            let starts_import =
                t.word == "use" || (t.word == "crate" && t.previous.as_deref() == Some("extern"));
            if !starts_import {
                continue;
            }
            let end = (t.pos..code.len())
                .find(|&i| code[i] == ';')
                .unwrap_or(code.len());
            let in_import = || file.tokens[n + 1..].iter().take_while(|u| u.pos < end);
            let renamed = in_import()
                .zip(file.tokens.get(n + 2..).unwrap_or(&[]))
                .any(|(a, b)| a.word == "as" && b.pos < end && b.word != "_");
            if renamed {
                found.push(format!("{} [import rename]", file.at(t.pos)));
            }
            if code[t.pos..end].contains(&'*') {
                found.push(format!("{} [glob import]", file.at(t.pos)));
            }
        }
        for i in 0..code.len() {
            if code[i] != '#' || file.is_excluded(i) {
                continue;
            }
            let mut j = i + 1;
            if code.get(j) == Some(&'!') {
                j += 1;
            }
            if code.get(j) != Some(&'[') {
                continue;
            }
            if let Some((word, _)) = ident_at(code, skip_ws(code, j + 1))
                && word == "path"
            {
                found.push(format!("{} [#[path] attribute]", file.at(i)));
            }
        }
    }
    found
}

/// Problems with a production crate's layout that would compile code the
/// scan does not read: a build script (`build.rs` at the crate root, or a
/// `build` key), a `[lib]` or `[[bin]]` path outside `src/`. `root_files`
/// are the names of the files at the crate root.
pub(crate) fn layout_problems(name: &str, manifest: &str, root_files: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    if root_files.iter().any(|f| f == "build.rs") {
        problems.push(format!("crates/{name}/build.rs: a build script"));
    }
    let mut section = String::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line.to_owned();
            continue;
        }
        let key = line.split('=').next().unwrap_or("").trim();
        if section == "[package]" && key == "build" {
            problems.push(format!("crates/{name}/Cargo.toml: {line}: a build script"));
        }
        let target = matches!(section.as_str(), "[lib]" | "[[bin]]");
        let value = line.split('"').nth(1).unwrap_or("");
        let under_src = value.starts_with("src/") && !value.contains("..");
        if target && key == "path" && !under_src {
            problems.push(format!(
                "crates/{name}/Cargo.toml {section}: {line}: a target outside src/"
            ));
        }
    }
    problems
}

/// The lines of a manifest that decide what a crate links: its `[features]`
/// and every dependency section except dev-dependencies (also
/// target-specific and build ones, and `[dependencies.NAME]` tables), with
/// each `workspace = true` entry followed by the workspace's line for it.
/// Full-line comments and blank lines are left out.
pub(crate) fn pinned_lines(manifest: &str, workspace_manifest: &str) -> Vec<String> {
    let workspace_entry = |dep: &str| -> String {
        let mut section = "";
        for line in workspace_manifest.lines().map(str::trim) {
            if line.starts_with('[') {
                section = line;
            } else if section == "[workspace.dependencies]"
                && line.split(['=', '.']).next().map(str::trim) == Some(dep)
            {
                return format!("workspace: {line}");
            }
        }
        format!("workspace: {dep} NOT FOUND in [workspace.dependencies]")
    };
    let mut out = Vec::new();
    let mut section = String::new();
    let mut pinned = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line.trim_matches(['[', ']']).to_owned();
            pinned = section == "features"
                || (section.contains("dependencies") && !section.contains("dev-dependencies"));
            if pinned {
                out.push(line.to_owned());
            }
            continue;
        }
        if !pinned || line.is_empty() || line.starts_with('#') {
            continue;
        }
        out.push(line.to_owned());
        if section != "features" && line.contains("workspace") {
            // `name.workspace = true`, `name = { workspace = true, .. }`, or
            // `workspace = true` inside a `[dependencies.name]` table.
            let dep = match line.split(['=', '.']).next().map(str::trim) {
                Some("workspace") => section.rsplit('.').next().unwrap_or("").to_owned(),
                Some(name) => name.to_owned(),
                None => String::new(),
            };
            out.push(workspace_entry(&dep));
        }
    }
    out
}

/// The workspace manifest's sections that could replace a dependency's
/// source for every crate (`[patch.*]`, `[replace]`).
pub(crate) fn source_overrides(workspace_manifest: &str) -> Vec<String> {
    workspace_manifest
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("[patch") || l.starts_with("[replace"))
        .map(str::to_owned)
        .collect()
}

/// The reviewed dependency boundary of the production crates: per crate,
/// exactly what [`pinned_lines`] must return. A new dependency or feature
/// can bring deleting APIs that the scan does not know by name
/// (`fs_extra::dir::remove`, `nix::fcntl::open` with `O_TRUNC`), so
/// EDITING THIS LIST IS A SAFETY REVIEW POINT (docs/safety.md, Pre-release
/// sandbox gate): review what the new dependency or feature can delete or
/// overwrite, and route that through the gate.
pub(crate) const PRODUCTION_DEPENDENCIES: &[(&str, &[&str])] = &[
    (
        "diskgc",
        &[
            "[dependencies]",
            "diskgc-core.workspace = true",
            "workspace: diskgc-core = { path = \"crates/diskgc-core\" }",
            "diskgc-linux.workspace = true",
            "workspace: diskgc-linux = { path = \"crates/diskgc-linux\" }",
        ],
    ),
    // Reviewed for task-8 (YAML profile schema) and task-73 (Cursor's
    // installed-extension registry): serde (derive), serde-saphyr 2.0.0
    // with only `deserialize`, and serde_json. They parse strings and
    // touch no files or processes: serde-saphyr's filesystem code
    // (`include_fs`, its CLI) and its serializer are features left off, and
    // no crate below names std::fs, std::process or OpenOptions outside
    // docs. What `cargo tree -p diskgc -e normal` gained: serde, serde_core,
    // serde_derive, proc-macro2, quote, syn, unicode-ident, serde-saphyr,
    // granit-parser, arraydeque, smallvec, annotate-snippets, anstyle,
    // unicode-width, num-traits, encoding_rs_io, encoding_rs and its
    // cfg-if, core_detect, multiversion_no_op, scopeguard, simdutf8.
    (
        "diskgc-core",
        &[
            "[dependencies]",
            "serde.workspace = true",
            "workspace: serde = { version = \"1\", features = [\"derive\"] }",
            "serde-saphyr.workspace = true",
            "workspace: serde-saphyr = { version = \"=2.0.0\", default-features = false, features = [\"deserialize\"] }",
            "serde_json.workspace = true",
            "workspace: serde_json = \"1\"",
        ],
    ),
    (
        "diskgc-linux",
        &[
            "[features]",
            "release-without-sandbox-gate = []",
            "[dependencies]",
            "diskgc-core.workspace = true",
            "workspace: diskgc-core = { path = \"crates/diskgc-core\" }",
            "rustix.workspace = true",
            "workspace: rustix = { version = \"1.1.5\", features = [\"event\", \"fs\", \"net\", \"process\", \"system\", \"time\"] }",
        ],
    ),
];

/// Deleting or content-destroying operations, child processes, namespace
/// or root changes (the gate decides once per process and assumes neither
/// changes), and escape hatches past the scan. Outside the allowlisted
/// wrappers, production code names none of them.
pub(crate) const GATED: &[Pattern] = &[
    // Deleting a name.
    Pattern::Word("unlink"),
    Pattern::Word("unlinkat"),
    Pattern::Word("rmdir"),
    Pattern::Word("remove_file"),
    Pattern::Word("remove_dir"),
    Pattern::Word("remove_dir_all"),
    // Replacing a name.
    Pattern::Word("rename"),
    Pattern::Word("renameat"),
    Pattern::Word("renameat_with"),
    // Destroying or overwriting contents, or opening for writing at all.
    Pattern::Word("TRUNC"),
    Pattern::Word("WRONLY"),
    Pattern::Word("RDWR"),
    Pattern::Word("CREATE"),
    Pattern::Word("APPEND"),
    Pattern::Word("TMPFILE"),
    Pattern::Word("OpenOptions"),
    Pattern::Path("File", "create"),
    Pattern::Path("File", "options"),
    // Opens for writing too (creates, never truncates): kept out like every
    // other writing open.
    Pattern::Path("File", "create_new"),
    // Flags from numbers would carry write or truncate bits unnamed.
    Pattern::Path("OFlags", "from_bits"),
    Pattern::Path("OFlags", "from_bits_retain"),
    Pattern::Path("OFlags", "from_bits_truncate"),
    Pattern::Path("fs", "write"),
    Pattern::Path("fs", "copy"),
    Pattern::Word("set_len"),
    Pattern::Word("ftruncate"),
    Pattern::Word("fallocate"),
    // Changing permissions of user data (make_writable).
    Pattern::Word("chmod"),
    Pattern::Word("fchmod"),
    Pattern::Word("chmodat"),
    Pattern::Word("set_permissions"),
    // Child processes (prune, GC and command children).
    // std's Command can only be named through `process` (renames and
    // globs are refused), so a domain `Action::Command` is not matched.
    Pattern::Path("process", "Command"),
    Pattern::Word("fork"),
    Pattern::Word("vfork"),
    Pattern::Word("execve"),
    Pattern::Word("execveat"),
    Pattern::Word("posix_spawn"),
    // Creating a name: binding an AF_UNIX socket to a path makes a socket
    // file (rustix's `net` feature, std's `UnixListener::bind`). The `net`
    // feature is there for one read-only query, the socket diagnostics of
    // `bound_unix_sockets` (task-6), which binds nothing.
    Pattern::Word("bind"),
    // Namespace or root changes.
    Pattern::Word("setns"),
    Pattern::Word("unshare"),
    Pattern::Word("chroot"),
    Pattern::Word("pivot_root"),
    Pattern::Word("move_into_link_name_space"),
    Pattern::Word("move_into_thread_name_spaces"),
    Pattern::Path("rustix", "mount"),
    // Escape hatches: raw syscalls, code the scan does not see.
    Pattern::Word("libc"),
    Pattern::CallOrMacro("syscall"),
    Pattern::Macro("include"),
];

/// The only places allowed to name a [`GATED`] operation. Each is marked
/// gated (takes the gate's `Permit`, checked by [`check_allowed`]) or
/// ungated with its reason; adding one is a safety review point
/// (docs/safety.md, Pre-release sandbox gate).
pub(crate) const GATED_ALLOWED: &[Allowed] = &[
    Allowed {
        file: "crates/diskgc-linux/src/host.rs",
        function: "unlinkat_permitted",
        gate: Gate::Permit,
        matches: 1,
    },
    Allowed {
        file: "crates/diskgc-linux/src/host.rs",
        function: "fchmod_permitted",
        gate: Gate::Permit,
        matches: 1,
    },
    Allowed {
        file: "crates/diskgc-linux/src/host.rs",
        function: "run_command_permitted",
        gate: Gate::Permit,
        matches: 1,
    },
    // `RecvFlags::TRUNC` (MSG_TRUNC) is a receive flag of the socket
    // diagnostics query: it reports a datagram's real length so a cut-short
    // one is detected. It truncates nothing on disk.
    Allowed {
        file: "crates/diskgc-linux/src/host.rs",
        function: "recv_datagram",
        gate: Gate::Ungated("MSG_TRUNC on a netlink recv; reads only, no file is opened"),
        matches: 1,
    },
    // The deletion engine's one caller of each deleting port method: with
    // these entries the scan also checks that nothing else in the core calls
    // them (docs/safety.md, Deletion).
    Allowed {
        file: "crates/diskgc-core/src/engine/delete.rs",
        function: "checked_unlink",
        gate: Gate::Ungated("calls Delete::unlinkat, which LinuxHost gates"),
        matches: 1,
    },
    Allowed {
        file: "crates/diskgc-core/src/engine/delete.rs",
        function: "make_writable",
        gate: Gate::Ungated("calls Delete::fchmod, which LinuxHost gates"),
        matches: 1,
    },
];

/// Crates that reach the OS only through the `Host` port: they may not name
/// `std::fs`, `std::os::unix::fs`, `rustix` or `libc` at all, and of
/// `std::process` only `ExitCode` (main's exit status). Stricter and simpler
/// than [`GATED`] (docs/architecture.md, The Host port).
pub(crate) const OS_FREE_CRATES: &[&str] = &["diskgc", "diskgc-core"];

/// Where `scanned` (sources of [`OS_FREE_CRATES`]) reaches the OS directly.
pub(crate) fn os_access(scanned: &[Scanned]) -> Vec<String> {
    let mut found = Vec::new();
    for file in scanned {
        for (n, t) in file.tokens.iter().enumerate() {
            let prefix = t.prefix.as_deref();
            let next = file.tokens.get(n + 1);
            let direct = match t.word.as_str() {
                "rustix" | "libc" => true,
                "fs" => matches!(prefix, Some("std" | "unix")),
                "process" => {
                    prefix == Some("std")
                        && !next.is_some_and(|x| {
                            x.word == "ExitCode" && x.prefix.as_deref() == Some("process")
                        })
                }
                _ => false,
            };
            if direct {
                found.push(format!(
                    "{} [OS access outside the Host port]",
                    file.at(t.pos)
                ));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, text: &str) -> SourceFile {
        SourceFile {
            path: PathBuf::from(path),
            text: text.to_owned(),
        }
    }

    /// Findings of [`GATED`] (nothing allowed) in a one-file crate.
    fn gated_in(text: &str) -> Result<Vec<String>, String> {
        let scanned = scan_crate(&[file("src/lib.rs", text)])?;
        Ok(find(&scanned, GATED, &[]).0)
    }

    /// The structural half of the sandbox gate (docs/safety.md, Pre-release
    /// sandbox gate): no production crate names a [`GATED`] operation except
    /// in [`GATED_ALLOWED`], the wrappers that take the gate's `Permit`.
    #[test]
    fn deleting_syscalls_only_behind_the_gate() {
        let scanned = production_sources();
        for want in [
            "crates/diskgc/src/main.rs",
            "crates/diskgc-core/src/host.rs",
            "crates/diskgc-linux/src/host.rs",
        ] {
            assert!(
                scanned.iter().any(|s| s.path == Path::new(want)),
                "{want} not scanned"
            );
        }
        let (mut found, hits) = find(&scanned, GATED, GATED_ALLOWED);
        found.extend(hiding_forms(&scanned));
        found.extend(check_allowed(&scanned, GATED_ALLOWED));
        assert!(
            found.is_empty(),
            "gated operations outside the gated wrappers (route them through the sandbox \
             gate, docs/safety.md):\n{}",
            found.join("\n")
        );
        // Positive control: the scan sees inside each wrapper, and nothing
        // more joined it.
        for (allowed, hits) in GATED_ALLOWED.iter().zip(hits) {
            assert_eq!(hits, allowed.matches, "{allowed:?}");
        }
    }

    /// Every production crate's code is under `src/`, where the walk reads
    /// it: no build script, no target path elsewhere.
    #[test]
    fn production_crates_compile_only_what_the_scan_reads() {
        let repo = repo_root();
        let mut problems = Vec::new();
        for name in production_crates() {
            let dir = repo.join("crates").join(&name);
            let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).expect("manifest");
            let root_files: Vec<String> = std::fs::read_dir(&dir)
                .expect("list crate")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            problems.extend(layout_problems(&name, &manifest, &root_files));
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    /// The production crates link exactly the reviewed dependencies and
    /// features ([`PRODUCTION_DEPENDENCIES`]), and nothing overrides a
    /// dependency's source.
    #[test]
    fn production_dependencies_are_the_reviewed_ones() {
        let repo = repo_root();
        let workspace = std::fs::read_to_string(repo.join("Cargo.toml")).expect("workspace");
        assert_eq!(source_overrides(&workspace), Vec::<String>::new());
        let crates = production_crates();
        let listed: Vec<&str> = PRODUCTION_DEPENDENCIES.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            crates, listed,
            "every production crate needs a reviewed entry"
        );
        for (name, want) in PRODUCTION_DEPENDENCIES {
            let manifest =
                std::fs::read_to_string(repo.join("crates").join(name).join("Cargo.toml"))
                    .expect("manifest");
            let got = pinned_lines(&manifest, &workspace);
            assert_eq!(
                got, *want,
                "crates/{name}: dependencies or features changed; review what they can delete \
                 or overwrite, then update PRODUCTION_DEPENDENCIES (a safety review point, \
                 docs/safety.md)"
            );
        }
    }

    #[test]
    fn a_new_dependency_or_feature_fails_the_boundary() {
        let repo = repo_root();
        let workspace = std::fs::read_to_string(repo.join("Cargo.toml")).expect("workspace");
        let manifest =
            std::fs::read_to_string(repo.join("crates/diskgc-linux/Cargo.toml")).expect("manifest");
        let want: &[&str] = PRODUCTION_DEPENDENCIES
            .iter()
            .find(|(c, _)| *c == "diskgc-linux")
            .expect("entry")
            .1;
        assert_eq!(
            pinned_lines(&manifest, &workspace),
            want,
            "control: the real manifest"
        );
        let edits: [(&str, String, String); 7] = [
            (
                "a plain dependency",
                manifest.replace("[dependencies]\n", "[dependencies]\nfs_extra = \"1\"\n"),
                workspace.clone(),
            ),
            (
                "a target-specific dependency",
                format!("{manifest}\n[target.'cfg(unix)'.dependencies]\nnix = \"0.29\"\n"),
                workspace.clone(),
            ),
            (
                "a build dependency",
                format!("{manifest}\n[build-dependencies]\ncc = \"1\"\n"),
                workspace.clone(),
            ),
            (
                "a dependency table",
                format!("{manifest}\n[dependencies.fs_extra]\nversion = \"1\"\n"),
                workspace.clone(),
            ),
            (
                "a rustix feature in the workspace",
                manifest.clone(),
                workspace.replace("\"net\", \"process\"", "\"net\", \"mount\", \"process\""),
            ),
            (
                "a feature of a dependency",
                manifest.replace(
                    "rustix.workspace = true",
                    "rustix = { workspace = true, features = [\"mount\"] }",
                ),
                workspace.clone(),
            ),
            (
                "a default feature",
                manifest.replace("[features]\n", "[features]\ndefault = [\"x\"]\n"),
                workspace.clone(),
            ),
        ];
        for (what, manifest, workspace) in edits {
            assert_ne!(
                pinned_lines(&manifest, &workspace),
                want,
                "not caught: {what}"
            );
        }
        let patched = format!("{workspace}\n[patch.crates-io]\nrustix = {{ path = \"x\" }}\n");
        assert_eq!(source_overrides(&patched), ["[patch.crates-io]"]);
    }

    #[test]
    fn a_build_script_or_outside_target_fails_the_layout() {
        let manifest = "[package]\nname = \"x\"\n\n[dependencies]\n";
        let none: [String; 0] = [];
        assert_eq!(layout_problems("x", manifest, &none), Vec::<String>::new());
        let bin = "[package]\nname = \"x\"\n[[bin]]\nname = \"x\"\npath = \"src/main.rs\"\n";
        assert_eq!(layout_problems("x", bin, &none), Vec::<String>::new());
        for (what, manifest, files) in [
            ("build.rs", manifest.to_owned(), vec!["build.rs".to_owned()]),
            (
                "build key",
                "[package]\nbuild = \"gen.rs\"\n".to_owned(),
                vec![],
            ),
            (
                "lib path",
                format!("{manifest}[lib]\npath = \"../actions.rs\"\n"),
                vec![],
            ),
            (
                "dotdot path",
                format!("{manifest}[lib]\npath = \"src/../../x.rs\"\n"),
                vec![],
            ),
            (
                "bin path",
                format!("{manifest}[[bin]]\nname = \"y\"\npath = \"tools/y.rs\"\n"),
                vec![],
            ),
        ] {
            assert!(
                !layout_problems("x", &manifest, &files).is_empty(),
                "not caught: {what}"
            );
        }
    }

    #[test]
    fn renames_globs_and_path_attributes_are_refused() {
        for text in [
            "use std::fs as filesystem;\nfn f() { filesystem::write(p, b); }\n",
            "use std::os::unix::fs as ufs;\n",
            "use rustix::fs as rfs;\n",
            "use std::process as p;\n",
            "use std::fs::File as F;\n",
            "use std::{fs as f, io};\n",
            "extern crate std as s;\n",
            "use std::fs::*;\nfn f() { write(p, b); }\n",
            "use std::{fs::*, io};\n",
            "use rustix::*;\n",
            "#[path = \"../actions.rs\"]\nmod actions;\n",
            "#[ path = \"x.rs\" ]\nmod x;\n",
            "#![path = \"x\"]\n",
            "fn f() {}\nuse std::fs as f",
        ] {
            let scanned = scan_crate(&[file("src/lib.rs", text)]).expect(text);
            assert!(!hiding_forms(&scanned).is_empty(), "not caught: {text}");
        }
        // Allowed: trait imports, casts, other attributes, test modules.
        let text = "use std::io::Write as _;\nuse std::fs::File;\n#[derive(Debug)]\nstruct S;\n\
                    fn f(x: u32) -> u64 { x as u64 }\n#[cfg(test)]\nmod tests {\n    \
                    use super::*;\n    use std::fs as f;\n}\n";
        let scanned = scan_crate(&[file("src/lib.rs", text)]).expect("scan");
        assert_eq!(hiding_forms(&scanned), Vec::<String>::new());
    }

    /// The core and the CLI reach the OS only through the `Host` port.
    #[test]
    fn core_and_cli_reach_the_os_only_through_the_port() {
        let crates = production_crates();
        let scanned: Vec<Scanned> = production_sources()
            .into_iter()
            .filter(|s| {
                OS_FREE_CRATES
                    .iter()
                    .any(|c| s.path.starts_with(Path::new("crates").join(c).join("src")))
            })
            .collect();
        for c in OS_FREE_CRATES {
            assert!(
                crates.iter().any(|p| p == c),
                "{c} is not a production crate"
            );
            let src = Path::new("crates").join(c).join("src");
            assert!(
                scanned.iter().any(|s| s.path.starts_with(&src)),
                "{c} not scanned"
            );
        }
        let found = os_access(&scanned);
        assert!(found.is_empty(), "{}", found.join("\n"));
    }

    #[test]
    fn os_access_in_core_or_cli_is_caught() {
        for text in [
            "use std::fs;\n",
            "fn f() { std::fs::read(p); }\n",
            "use std::{fs, io};\n",
            "use std::os::unix::fs::PermissionsExt;\n",
            "fn f() { rustix::fs::statx(d, p, f, m); }\n",
            "fn f() { libc::getpid(); }\n",
            "use std::process;\n",
            "fn f() { std::process::exit(1); }\n",
            "use std::process::{Command, ExitCode};\n",
            "use std::{process::Command};\n",
        ] {
            let scanned = scan_crate(&[file("src/lib.rs", text)]).expect(text);
            assert!(!os_access(&scanned).is_empty(), "not caught: {text}");
        }
        let text = "use std::process::ExitCode;\nuse std::os::unix::ffi::OsStrExt;\n\
                    use crate::process::Pid;\nfn main() -> ExitCode { ExitCode::SUCCESS }\n\
                    fn g(h: &impl Host) { h.statx(Base::Root, p); }\n";
        let scanned = scan_crate(&[file("src/lib.rs", text)]).expect("scan");
        assert_eq!(os_access(&scanned), Vec::<String>::new());
    }

    #[test]
    fn an_allowlist_entry_marked_gated_must_take_a_permit() {
        let text = "fn gated(_: &Permit, d: &Fd) { rustix::fs::unlinkat(d, n, f) }\n\
                    fn sneaky(d: &Fd) { rustix::fs::unlinkat(d, n, f) }\n\
                    fn state(d: &Fd) { rustix::fs::renameat(d, a, d, b) }\n";
        let scanned = scan_crate(&[file("src/lib.rs", text)]).expect("scan");
        let entry = |function, gate| Allowed {
            file: "src/lib.rs",
            function,
            gate,
            matches: 1,
        };
        let good = [
            entry("gated", Gate::Permit),
            entry("state", Gate::Ungated("diskgc's own state")),
        ];
        assert_eq!(check_allowed(&scanned, &good), Vec::<String>::new());
        let bad = [entry("sneaky", Gate::Permit)];
        let problems = check_allowed(&scanned, &bad);
        assert!(problems[0].contains("takes no Permit"), "{problems:?}");
        let unexplained = [entry("state", Gate::Ungated(" "))];
        assert!(check_allowed(&scanned, &unexplained)[0].contains("needs its reason"));
        let missing = [entry("absent", Gate::Permit)];
        assert!(!check_allowed(&scanned, &missing).is_empty());
    }

    /// Lexer corner cases that once hid code: raw C strings, and an escaped
    /// quote in a char literal.
    #[test]
    fn lexer_corner_cases_hide_nothing() {
        for text in [
            "fn f() { let s = cr\"x\"; }\n",
            "fn f() { let s = cr#\"x\"#; }\n",
        ] {
            assert!(gated_in(text).is_err(), "{text}");
        }
        let text = "fn a() { let q = ['\\'','\"']; }\nfn b() { std::fs::remove_file(p); }\n";
        let found = gated_in(text).expect("scan");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("in fn b"), "{found:?}");
        let text = "fn a() { let q = ['\\\\', '\\u{27}', b'\\'']; std::fs::remove_file(p); }\n";
        assert_eq!(gated_in(text).expect("scan").len(), 1);
    }

    /// E5: a production module loaded by `#[path]` from a file that is also
    /// declared as a test module would escape the walk's exclusion; the
    /// `#[path]` ban catches it.
    #[test]
    fn a_path_module_reusing_a_test_module_file_is_caught() {
        let crate_files = [
            file(
                "src/lib.rs",
                "#[cfg(test)]\nmod evil;\n#[path = \"evil.rs\"]\nmod evil_prod;\n",
            ),
            file("src/evil.rs", "pub fn f() { std::fs::remove_file(p); }\n"),
        ];
        let scanned = scan_crate(&crate_files).expect("scan");
        assert!(scanned.iter().all(|s| s.path != Path::new("src/evil.rs")));
        let forms = hiding_forms(&scanned);
        assert!(
            forms.iter().any(|f| f.contains("#[path] attribute")),
            "{forms:?}"
        );
    }

    /// Domain names that look like gated ones pass; the real forms fail.
    #[test]
    fn command_include_and_syscall_match_only_their_real_forms() {
        let legit = "enum Action { Command { argv: Vec<String> } }\n\
                     struct S { include: bool, syscall: u32 }\n\
                     fn f(a: &Action, s: &S) -> u32 {\n    \
                     match a { Action::Command { .. } => {} }\n    \
                     if s.include { s.syscall } else { 0 }\n}\n";
        assert_eq!(gated_in(legit).expect("scan"), Vec::<String>::new());
        for bad in [
            "fn f() { std::process::Command::new(\"x\"); }\n",
            "use std::process::Command;\n",
            "use std::process::{Command, Stdio};\n",
            "fn f() { include!(\"x.rs\"); }\n",
            "fn f() { syscall(87); }\n",
            "fn f() { syscall!(87); }\n",
        ] {
            assert!(!gated_in(bad).expect(bad).is_empty(), "not caught: {bad}");
        }
    }

    /// The test-only crates are excluded from the scan, so they must never
    /// ship: no production crate depends on them except as a
    /// dev-dependency.
    #[test]
    fn test_only_crates_are_never_production_dependencies() {
        let repo = repo_root();
        for entry in std::fs::read_dir(repo.join("crates")).expect("list crates") {
            let dir = entry.expect("entry").path();
            let name = dir
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            if TEST_ONLY_CRATES.iter().any(|(t, _)| *t == name) {
                continue;
            }
            let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).expect("manifest");
            let mut section = String::new();
            for (n, line) in manifest.lines().enumerate() {
                let line = line.trim();
                if line.starts_with('[') {
                    section = line.to_owned();
                }
                let names_test_crate = TEST_ONLY_CRATES
                    .iter()
                    .any(|(t, _)| line.contains(t) && !line.starts_with('#'));
                assert!(
                    !names_test_crate || section.ends_with("dev-dependencies]"),
                    "crates/{name}/Cargo.toml:{}: test-only crate outside dev-dependencies \
                     ({section}): {line}",
                    n + 1
                );
            }
        }
    }

    // ---- Negative controls: each must be caught. ----

    /// Every [`GATED`] pattern has a scripted case, and each case is caught
    /// by exactly one pattern: a pattern added without a control, or one
    /// removed from the list, fails here.
    #[test]
    fn every_gated_operation_is_caught() {
        let cases = [
            "rustix::fs::unlink(p);",
            "std::fs::remove_file(p);",
            "std::fs::remove_dir(p);",
            "std::fs::remove_dir_all(p);",
            "rustix::fs::rmdir(p);",
            "rustix::fs::unlinkat(d, n, f);",
            "rustix::fs::rename(a, b);",
            "rustix::fs::renameat(d, a, d, b);",
            "rustix::fs::renameat_with(d, a, d, b, f);",
            "std::fs::rename(a, b);",
            "rustix::fs::openat(d, n, OFlags::WRONLY, m);",
            "rustix::fs::openat(d, n, OFlags::RDONLY | OFlags::TRUNC, m);",
            "rustix::fs::openat(d, n, OFlags::RDWR, m);",
            "rustix::fs::openat(d, n, OFlags::CREATE, m);",
            "rustix::fs::openat(d, n, OFlags::APPEND, m);",
            "rustix::fs::openat(d, n, OFlags::TMPFILE, m);",
            "let f = std::fs::File::create(p);",
            "let f = File::create(p);",
            "std::fs::File::options().write(true).truncate(true).open(p);",
            "let f = std::fs::File::create_new(p);",
            "let f = OFlags::from_bits(0o1001);",
            "let f = OFlags::from_bits_retain(0o1001);",
            "let f = OFlags::from_bits_truncate(0o1001);",
            "std::fs::write(p, b);",
            "fs::write(p, b);",
            "std::fs::copy(a, b);",
            "OpenOptions::new().write(true).truncate(true).open(p);",
            "file.set_len(0);",
            "rustix::fs::ftruncate(fd, 0);",
            "rustix::fs::fallocate(fd, FallocateFlags::PUNCH_HOLE, 0, 1);",
            "rustix::fs::chmod(p, m);",
            "rustix::fs::fchmod(fd, m);",
            "rustix::fs::chmodat(d, p, m, f);",
            "std::fs::set_permissions(p, perm);",
            "rustix::net::bind(fd, &addr);",
            "std::process::Command::new(\"docker\");",
            "let pid = unsafe { fork() };",
            "let pid = unsafe { vfork() };",
            "execve(p, a, e);",
            "execveat(d, p, a, e, f);",
            "posix_spawn(pid, p, fa, at, a, e);",
            "setns(fd, CloneFlags::NEWNS);",
            "rustix::thread::unshare(UnshareFlags::NEWNS);",
            "rustix::thread::move_into_link_name_space(fd, None);",
            "rustix::thread::move_into_thread_name_spaces(fd, f);",
            "rustix::process::chroot(p);",
            "rustix::process::pivot_root(a, b);",
            "rustix::mount::mount_bind(a, b);",
            "libc::open(p, f);",
            "let r = syscall(87, p);",
            "include!(\"more.rs\");",
            // Grouped imports name the operation without its path.
            "use std::fs::{copy, read};",
            "use std::fs::{self, write};",
            "use std::{fs::{remove_file}};",
            "use std::fs::{File}; fn g() { let _ = File::create(p); }",
        ];
        let mut covered = vec![false; GATED.len()];
        for case in cases {
            let text = format!("fn cleanup() {{\n    {case}\n}}\n");
            let scanned = scan_crate(&[file("src/lib.rs", &text)]).expect("scan");
            let matching: Vec<usize> = (0..GATED.len())
                .filter(|&k| !find(&scanned, &[GATED[k]], &[]).0.is_empty())
                .collect();
            assert!(
                matching.len() == 1,
                "{case}: caught by {:?}, expected exactly one pattern",
                matching.iter().map(|&k| GATED[k]).collect::<Vec<_>>()
            );
            covered[matching[0]] = true;
        }
        let uncovered: Vec<_> = GATED
            .iter()
            .zip(&covered)
            .filter(|(_, c)| !**c)
            .map(|(p, _)| p)
            .collect();
        assert!(
            uncovered.is_empty(),
            "patterns without a negative control: {uncovered:?}"
        );
    }

    #[test]
    fn harmless_look_alikes_are_not_caught() {
        let text = "fn f(buf: &mut Vec<u8>, w: &mut impl std::io::Write) {\n    \
                    buf.truncate(1);\n    std::io::copy(&mut &buf[..], w).ok();\n    \
                    w.write(b\"remove_file unlinkat\").ok();\n    \
                    // std::fs::remove_file(p) in a comment\n    \
                    let _ = std::fs::read(\"/x\");\n    std::thread::spawn(|| ());\n}\n\
                    trait T { fn unlinkat(&self); fn rename(&self) {} }\n";
        assert_eq!(gated_in(text).expect("scan"), Vec::<String>::new());
    }

    #[test]
    fn code_after_an_inline_test_module_is_scanned() {
        let text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { std::fs::remove_file(p); }\n}\n\
                    fn late() { std::fs::remove_file(p); }\n";
        let found = gated_in(text).expect("scan");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("in fn late"), "{found:?}");
    }

    #[test]
    fn a_cfg_test_in_a_doc_comment_hides_nothing() {
        let text = "/// Example: #[cfg(test)] mod tests {}\n//! #[cfg(test)]\n\
                    fn prod() { std::fs::remove_file(p); }\n";
        assert_eq!(gated_in(text).expect("scan").len(), 1);
    }

    #[test]
    fn a_test_only_item_that_is_not_a_module_is_refused() {
        for text in [
            "#[cfg(test)]\nfn helper() {}\nfn prod() { std::fs::remove_file(p); }\n",
            "#[cfg(test)]\nuse std::fs::remove_file;\n",
            "#[cfg(test)]\nconst X: u8 = 1;\n",
        ] {
            let err = gated_in(text).expect_err(text);
            assert!(err.contains("must gate a module"), "{err}");
        }
        // Allowed forms.
        for text in [
            "#[cfg(test)]\n#[allow(dead_code)]\nmod tests { fn t() { std::fs::remove_file(p); } }\n",
            "#[cfg(test)]\npub(crate) mod tests {}\n",
        ] {
            assert_eq!(gated_in(text).expect(text), Vec::<String>::new(), "{text}");
        }
    }

    /// A raw identifier is read as the identifier, so it hides nothing
    /// (serde field names such as `r#where` need it).
    #[test]
    fn raw_identifiers_are_read_as_identifiers() {
        for (text, caught) in [
            ("fn f() { std::fs::r#rename(a, b); }\n", "rename"),
            ("fn f() { rustix::fs::r#unlinkat(d, n, f); }\n", "unlinkat"),
            ("fn f() { r#std::r#fs::r#remove_file(p); }\n", "remove_file"),
        ] {
            let found = gated_in(text).expect(text);
            assert_eq!(found.len(), 1, "{text}: {found:?}");
            assert!(found[0].contains(caught), "{text}: {found:?}");
        }
        let text = "fn f() { std::r#fs::read(p); }\n";
        let scanned = scan_crate(&[file("src/lib.rs", text)]).expect("scan");
        assert_eq!(os_access(&scanned).len(), 1, "{text}");
        let text = "struct S { r#where: u8, r#type: u8, r#match: u8 }\n";
        assert_eq!(gated_in(text).expect(text), Vec::<String>::new());
    }

    #[test]
    fn unreadable_syntax_is_an_error_not_a_guess() {
        for text in [
            "/* std::fs::remove_file(p) */ fn f() {}\n",
            "fn f() { let s = r\"x\"; }\n",
            "fn f() { let s = r#\"x\"#; }\n",
            "fn f() { let s = br#\"x\"#; }\n",
            "fn f() { let s = br\"x\"; }\n",
        ] {
            assert!(gated_in(text).is_err(), "{text}");
        }
    }

    #[test]
    fn out_of_line_modules_nested_or_test_only_are_handled() {
        let crate_files = [
            file(
                "crates/x/src/lib.rs",
                "mod actions;\n#[cfg(test)]\nmod scan_tests;\n",
            ),
            file(
                "crates/x/src/actions/mod.rs",
                "pub fn clean() { std::fs::remove_file(p); }\n",
            ),
            file(
                "crates/x/src/scan_tests.rs",
                "fn t() { std::fs::remove_file(p); }\n",
            ),
        ];
        let scanned = scan_crate(&crate_files).expect("scan");
        let (found, _) = find(&scanned, GATED, &[]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("crates/x/src/actions/mod.rs:1"),
            "{found:?}"
        );
        // A test module whose file is missing is an error.
        let err = scan_crate(&crate_files[..1]).expect_err("missing test module file");
        assert!(err.contains("scan_tests"), "{err}");
    }

    /// The directory walk finds nested modules (on a real temp tree).
    #[test]
    fn the_walk_finds_nested_modules() {
        let root = std::env::temp_dir().join(format!("diskgc-scan-walk-{}", std::process::id()));
        let nested = root.join("src/actions/deep");
        std::fs::create_dir_all(&nested).expect("create tree");
        std::fs::write(root.join("src/lib.rs"), "mod actions;\n").expect("write");
        std::fs::write(root.join("src/actions/mod.rs"), "mod deep;\n").expect("write");
        std::fs::write(
            nested.join("mod.rs"),
            "fn f() { std::fs::remove_file(p); }\n",
        )
        .expect("write");
        let files = rust_files(&root, Path::new("src"));
        std::os::unix::fs::symlink(&nested, root.join("src/linked")).expect("symlink");
        let with_link = rust_files(&root, Path::new("src"));
        let _ = std::fs::remove_dir_all(&root);
        let files = files.expect("walk");
        let err = with_link.expect_err("a symlinked directory must be refused");
        assert!(err.contains("src/linked: a symlink"), "{err}");
        let scanned = scan_crate(&files).expect("scan");
        let (found, _) = find(&scanned, GATED, &[]);
        assert_eq!(found.len(), 1, "{files:?} {found:?}");
        assert!(
            found[0].starts_with("src/actions/deep/mod.rs:1"),
            "{found:?}"
        );
    }

    #[test]
    fn only_the_allowed_function_may_name_a_gated_operation() {
        let text = "fn permitted(_: &Permit) { rustix::fs::unlinkat(d, n, f) }\n\
                    fn other() { rustix::fs::unlinkat(d, n, f) }\n";
        let scanned = scan_crate(&[file("src/lib.rs", text)]).expect("scan");
        let allowed = [Allowed {
            file: "src/lib.rs",
            function: "permitted",
            gate: Gate::Permit,
            matches: 1,
        }];
        let (found, hits) = find(&scanned, GATED, &allowed);
        assert_eq!(hits, vec![1]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("in fn other"), "{found:?}");
    }
}
