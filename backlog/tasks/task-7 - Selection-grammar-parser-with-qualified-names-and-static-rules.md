---
id: TASK-7
title: Selection grammar parser with qualified names and static rules
status: Done
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 20:42'
labels:
  - wave-1
  - config
dependencies:
  - TASK-1
references:
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the --select expression and tier selects are part of the frozen YAML/CLI contract. Implement the grammar in docs/reference.md#selection-grammar: qualified profile:/filter: names, parenthesised profile groups that distribute, unions of parenthesised terms, no unparenthesised mixing of || and &&, ! on single names only; static checks (one source per term, one kind and one action per expression, kind-supported predicates, wrong-qualifier errors naming what the name is).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 profile:a || profile:b && filter:f is a parse error with a message showing the two parenthesised readings
- [x] #2 Every grammar example in docs/reference.md parses and normalises as documented (table-driven test)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented in crates/diskgc-core/src/select/ (mod.rs: AST and compile; parse.rs: lexer and recursive-descent parser; check.rs: Catalog interface and static rules; tests.rs). parse(text) -> Expr (normalised: groups distributed by the pure fn distribute, written order kept, no dedupe); check(expr, &impl Catalog) -> Checked {expr, kind} or CheckErrors (all broken rules, deduplicated); compile = both, and restates a syntax error about a name in the wrong position (filter:X at a term start or in a group, profile:X after &&) as the wrong-qualifier error when the catalog says X is of the other sort.

AC #1: select::tests::ac1_unparenthesised_mix_shows_both_readings, plus the messages table row in docs/reference.md (doc_messages_are_the_messages). Bite: dropping the second reading from Parser::mixed_or_and failed 5 tests.
AC #2: the examples are now a table in docs/reference.md (Expression | Normalised | Selects). doc_examples_normalise_as_documented checks every row; every_doc_expression_is_classified fails on any qualified expression in the section's prose not listed in PROSE (and on stale PROSE entries) and compiles every select: / --select '...' in the doc's code blocks. Bites: reversing distribute's order (6 failures); editing a Normalised cell (1); adding an unclassified prose example (1).
Static rules: one test each (unknown names, wrong qualifiers, kinds differ, actions differ, unsupported predicate, non-fs kind shapes, negated kind, dedupe, empty hand-built Expr). Bites: never comparing kinds (2 failures); Predicate::supported_by always true (3); a source written filter: accepted (2).
Properties (proptest): print->parse identity on random ASTs; random surface forms (groups, (one), ((group) && f), whitespace variants) parse to their distribution; token soup never panics and every suggested rewrite parses.

reference.md changes: NAME defined (ASCII letters, digits, - and _); whitespace rule; examples table with normalised forms; nesting reading stated (the only nesting is a profile group at the start of a parenthesised union member, as the grammar allows); an error-format paragraph and a messages table (all texts are tested against the code); idle-2d added to the built-in filter list (the example config and the grammar examples used it, the library table did not list it).
flake.nix: workspace-tests gets workspaceTestSrc = workspaceFiles + docs/reference.md, so only that check sees the doc.

Limits: suggestions are offered only if they parse; for a mix inside one paren level (e.g. (profile:a || profile:b && filter:f)) neither reading parses alone, so the message says only 'add parentheses'. Error columns exist for syntax errors only (the normalised AST has no positions). The fake catalog in tests.rs mirrors the built-in table by hand (kinds and stand-in action names), so a library change can leave it stale until task-8 provides the real catalog.

Review round 1 (mped GO with should-fixes; user decisions S1, S7):
S1 strict parentheses (user): grammar now uterm := profile | '(' nterm ')', term := profile | nterm, nterm := sources ('&&' factor)+, sources := profile | '(' profile ('||' profile)+ ')'. New errors ParenOne and UnnarrowedGroup (replaces GroupInUnion), each with a rewrite shown only if accepted; ((profile:a)) suggests dropping both pairs. Tests every_grammar_form_is_accepted (strict list), every_parenthesis_has_a_purpose. Bites: (profile:a) accepted -> 2 failures; group without && accepted in a union -> 3.
S7 NAME = [a-z0-9][a-z0-9_-]* (user): is_name, InvalidName error (the lexer reads words wider than NAME so Foo is reported as an invalid name), reference.md; every_built_in_name_is_a_name reads the library table. Bite: uppercase allowed -> 2 failures.
S2: non-enumerating kinds (docker-images, nix-store, docker-build-cache) take exactly one term (CheckError::OneTerm); one_action_per_expression now uses fs sources with different actions. Bite: rule disabled -> 2 failures.
S3: check-level fixes kept only if the rewrite adds no new broken rule; requalified fixes only if the rewritten text is accepted; compile also drops parser suggestions the static rules reject. Properties compile_suggestions_compile_on_surface_forms / _on_token_soup (2048 cases each) plus rejected_fixes_are_not_suggested (mped's 6 cases) and compile_drops_syntax_suggestions_the_rules_reject. Bites: check fixes never dropped -> 3 failures; requalified not verified -> 2; syntax suggestions not re-checked -> 0 failures at first (the soup rarely hits it), so the deterministic test was added, then 2 failures.
S4: compile errors quote the text as written (CheckErrors.expr); bare check keeps the normalised form. Test compile_errors_quote_the_text_as_written.
H6 done: Kind and PredicateName (renamed from Predicate) live in the leaf module diskgc_core::kind, re-exported from select. H8 a-c done: the exact code-block expression list, one doc path constant, and a section split that ignores headings inside code blocks.

Committed as a7bce15 (tree 9a1c9f6) after qa-test-runner GO and mped-architect GO (two rounds). User decisions applied: strict parentheses; NAME is [a-z0-9][a-z0-9_-]* (the earlier note's 'ASCII letters, digits, - and _' and the '(one)' surface form in Properties are superseded). Post-commit (rides with the next commit): reference.md says a suggested rewrite fixes its error without breaking another rule; docker-build-cache wording says it takes idle_for filters (several allowed); the PROSE list in select/tests.rs updated.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
