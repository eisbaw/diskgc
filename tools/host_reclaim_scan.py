#!/usr/bin/env python3
"""Fail when a host test source writes a reclaiming diskgc argv (TASK-282).

The rule, the scope and the limits are docs/testing.md's (its opening
paragraph); this is how the scan reads a source. Each string literal
`"ensure"` or `"watch"` is a finding unless its argv, the bracket group
(`[...]` or `(...)`) directly around it, holds that command's read-only
flag (READ_ONLY_FLAG) among its own literals (not a nested group's). A literal
with no such group (a `let`, a match arm) is a finding; the one exemption is
a lookup by that literal alone (`json["watch"]`, `usage["ensure"]`). So argv
built in pieces (`.arg("ensure")`, `[&["ensure"], rest].concat()`) is a
finding even when a later piece adds `--dry-run`: write it as one list.
Findings count against the function last defined before them (ALLOWED pins
each allowlisted function's count), so a stray literal after an allowlisted
function's body raises its count and is reported.

Run by `just check` through tools/test_host_reclaim_scan.py; by hand,
`python3 tools/host_reclaim_scan.py` prints each finding and exits 1.
"""

import ast
import io
import keyword
import re
import sys
import tokenize
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Every source whose code runs on the developer's machine and could spawn
# diskgc: `cargo test` targets outside src/, `cargo run --example`, the fuzz
# project's seed test, and the Python tests `just check` discovers.
SCANNED = (
    "crates/*/tests/**/*.rs",
    "crates/*/benches/**/*.rs",
    "crates/*/examples/**/*.rs",
    "fuzz/tests/**/*.rs",
    "fuzz/fuzz_targets/**/*.rs",
    "tools/test_*.py",
)

# Each reclaiming command and the flag that makes it read-only (`watch` has
# no dry run; `ensure` no tally-only mode).
READ_ONLY_FLAG = {"ensure": "--dry-run", "watch": "--tally-only"}

# (path, function) -> (reclaiming literals it holds, why it may).
# Each entry is a review point: the count pins it, so a second reclaiming
# argv in an allowlisted function is reported too.
ALLOWED = {
    (
        "crates/diskgc/tests/cli.rs",
        "a_reclaiming_ensure_outside_a_container_exits_2_naming_the_sandbox_gate",
    ): (
        1,
        "the one named host exception (CLAUDE.md, docs/testing.md): proves the "
        "pre-release sandbox gate refuses a reclaiming ensure, on a fixture of "
        "its own (roots, PATH and config)",
    ),
    ("tools/test_docs_vs_binary.py", "test_reclaiming_argv_needs_dry_run"): (
        2,
        "argv given as data to docs_vs_binary.host_refusal, a pure function; "
        "nothing is spawned",
    ),
}


class ScanError(Exception):
    """A source the scan cannot read with confidence: fail closed."""


# ---- Tokens ---------------------------------------------------------------
# Both readers return (kind, value, line) with kind "str" (a string literal's
# value), "open"/"close" (a bracket), "fn" (a function's name, where it is
# defined) or "word" (any other code token that can precede a bracket).

_RUST_STRING = re.compile(r'(?:b|c)?"((?:[^"\\]|\\.)*)"', re.S)
_RUST_RAW = re.compile(r'(?:b|c)?r(#*)"(.*?)"\1', re.S)
_RUST_CHAR = re.compile(r"b?'(?:[^'\\\n]|\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]{1,6}\}|.))'")
_RUST_WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def rust_tokens(text):
    """A Rust source's tokens. Comments are skipped (block comments nest);
    an unterminated string or comment raises ScanError."""
    tokens, i, n, line = [], 0, len(text), 1
    expect_fn_name = False
    while i < n:
        c = text[i]
        if c == "\n":
            line += 1
            i += 1
            continue
        if c.isspace():
            i += 1
            continue
        if text.startswith("//", i):
            end = text.find("\n", i)
            i = n if end < 0 else end
            continue
        if text.startswith("/*", i):
            depth, j = 1, i + 2
            while depth:
                if j >= n:
                    raise ScanError(f"line {line}: unterminated block comment")
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            line += text.count("\n", i, j)
            i = j
            continue
        m = _RUST_RAW.match(text, i) or _RUST_STRING.match(text, i)
        if m:
            value = m.group(2) if m.re is _RUST_RAW else m.group(1)
            tokens.append(("str", value, line))
            line += text.count("\n", i, m.end())
            i = m.end()
            continue
        if c == '"' or (c in "bcr" and re.match(r'(?:b|c)?r?#*"', text[i:i + 8])):
            raise ScanError(f"line {line}: unterminated string literal")
        if c == "'":
            m = _RUST_CHAR.match(text, i)
            i = m.end() if m else i + 1  # a char literal, or a lifetime's quote
            continue
        m = _RUST_WORD.match(text, i)
        if m:
            word = m.group()
            tokens.append(("fn" if expect_fn_name else "word", word, line))
            expect_fn_name = word == "fn"
            i = m.end()
            continue
        expect_fn_name = False
        if c in "([{":
            tokens.append(("open", c, line))
        elif c in ")]}":
            tokens.append(("close", c, line))
        else:
            tokens.append(("word", c, line))
        i += 1
    return tokens


def python_tokens(text):
    """A Python source's tokens, read by the standard tokenizer."""
    tokens, expect_fn_name = [], False
    try:
        for tok in tokenize.generate_tokens(io.StringIO(text).readline):
            line = tok.start[0]
            if tok.type == tokenize.STRING:
                try:
                    value = ast.literal_eval(tok.string)
                except (ValueError, SyntaxError) as error:
                    raise ScanError(f"line {line}: string literal {tok.string!r}: {error}")
                tokens.append(("str", value if isinstance(value, str) else None, line))
            elif tok.type == tokenize.OP and tok.string in "([{":
                tokens.append(("open", tok.string, line))
            elif tok.type == tokenize.OP and tok.string in ")]}":
                tokens.append(("close", tok.string, line))
            elif tok.type in (tokenize.NAME, tokenize.OP, tokenize.NUMBER):
                tokens.append(("fn" if expect_fn_name else "word", tok.string, line))
                expect_fn_name = tok.string == "def"
                continue
            else:
                continue
            expect_fn_name = False
    except (tokenize.TokenError, SyntaxError) as error:
        raise ScanError(str(error)) from error
    return tokens


# ---- Findings -------------------------------------------------------------


_CLOSER = {"(": ")", "[": "]", "{": "}"}


def _group(tokens, k):
    """The bracket group directly around token k: (open index, close index),
    or None when k is not inside one."""
    depth = 0
    for start in range(k - 1, -1, -1):
        kind = tokens[start][0]
        if kind == "close":
            depth += 1
        elif kind == "open":
            if depth == 0:
                break
            depth -= 1
    else:
        return None
    depth = 0
    for end in range(k + 1, len(tokens)):
        kind = tokens[end][0]
        if kind == "open":
            depth += 1
        elif kind == "close":
            if depth == 0:
                if tokens[end][1] != _CLOSER[tokens[start][1]]:
                    raise ScanError(f"line {tokens[end][2]}: mismatched {tokens[end][1]}")
                return start, end
            depth -= 1
    raise ScanError(f"line {tokens[k][2]}: unclosed bracket")


# Words after which `[` starts a list, not an index (`for c in ["watch"]`).
_NOT_INDEXABLE = frozenset(keyword.kwlist) | {
    "as", "box", "break", "const", "dyn", "else", "for", "if", "impl", "in", "let",
    "match", "move", "mut", "ref", "return", "static", "unsafe", "where", "while",
}


def _is_index(tokens, start, end):
    """`name["watch"]`, `call()["watch"]`, `pair.1["watch"]`: a lookup by
    that key alone, not an argv."""
    if tokens[start][1] != "[" or end != start + 2 or start == 0:
        return False
    kind, value, _ = tokens[start - 1]
    if kind == "close":
        return value in ")]"
    named = value.isidentifier() or value.isdigit()
    return kind == "word" and named and value not in _NOT_INDEXABLE


def reclaiming_literals(tokens):
    """(line, function, literal) of each reclaiming argv literal."""
    found, function = [], None
    for k, (kind, value, line) in enumerate(tokens):
        if kind == "fn":
            function = value
        if kind != "str" or value not in READ_ONLY_FLAG:
            continue
        group = _group(tokens, k)
        if group and tokens[group[0]][1] != "{":
            start, end = group
            if _is_index(tokens, start, end):
                continue
            depth, own = 0, []
            for kind2, value2, _ in tokens[start + 1:end]:
                if kind2 == "open":
                    depth += 1
                elif kind2 == "close":
                    depth -= 1
                elif kind2 == "str" and depth == 0:
                    own.append(value2)
            if READ_ONLY_FLAG[value] in own:
                continue
        found.append((line, function, value))
    return found


def scan_text(relative, text, allowed):
    """Findings in one source, as messages; `allowed` is ALLOWED's shape."""
    reader = python_tokens if relative.endswith(".py") else rust_tokens
    try:
        literals = reclaiming_literals(reader(text))
    except ScanError as error:
        return [f"{relative}: cannot scan with confidence: {error}"]
    by_function = {}
    for line, function, literal in literals:
        by_function.setdefault(function, []).append((line, literal))
    messages = []
    for function, hits in by_function.items():
        expected = allowed.get((relative, function), (0, None))[0]
        if len(hits) == expected:
            continue
        for line, literal in hits:
            messages.append(
                f"{relative}:{line}: in {function or 'no function'}: `{literal}` argv "
                f"without {READ_ONLY_FLAG[literal]} reclaims; the host may run only "
                f"the named sandbox-gate test (docs/testing.md). Not an argv? Compare "
                f"against text that is not the bare word, or allowlist it with a reason"
                + (f" (allowlisted for {expected}, found {len(hits)})" if expected else "")
            )
    for (path, function), (expected, _) in allowed.items():
        if path == relative and function not in by_function:
            messages.append(
                f"{relative}: allowlisted {function} holds no reclaiming argv "
                f"(expected {expected}); remove or fix the entry"
            )
    return messages


def scanned_files(root=ROOT):
    files = set()
    for pattern in SCANNED:
        files.update(p for p in root.glob(pattern) if p.is_file())
    return sorted(files)


def scan(root=ROOT, allowed=None):
    """Every finding under `root`, including allowlist entries whose file
    is not scanned at all."""
    allowed = ALLOWED if allowed is None else allowed
    files = scanned_files(root)
    relatives = {p.relative_to(root).as_posix() for p in files}
    messages = []
    for path in files:
        messages += scan_text(path.relative_to(root).as_posix(), path.read_text(), allowed)
    for path, function in allowed:
        if path not in relatives:
            messages.append(f"{path}: allowlisted ({function}) but not a scanned file")
    return messages


def main():
    messages = scan()
    for message in messages:
        print(f"host reclaim scan: {message}", file=sys.stderr)
    return 1 if messages else 0


if __name__ == "__main__":
    sys.exit(main())
