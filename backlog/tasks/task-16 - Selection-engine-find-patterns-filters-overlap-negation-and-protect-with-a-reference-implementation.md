---
id: TASK-16
title: >-
  Selection engine: find patterns, filters, overlap negation and protect, with a
  reference implementation
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-18 12:42'
labels:
  - wave-2
  - irreversible
  - config
dependencies:
  - TASK-15
references:
  - docs/reference.md#selection-grammar
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: what gets selected must equal the documented set semantics exactly. Write the reference implementation first, from the examples in docs/reference.md, then the engine: segment-wise find patterns (no **), filter evaluation, !filter, !profile and protect by overlap (ignoring the negated profile's where), parenthesised distribution. Decide here, before the schema freeze, how profiles that find at any depth under a project root work without ** (explicit depths or one root per project), and document it in docs/reference.md. Tests use user profiles in fixtures (the built-in library ships in wave 4).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Property test: eligible set equals the reference implementation for random profiles, expressions and trees
- [ ] #2 Property test: ! and protect never add a candidate; a || b equals b || a
- [ ] #3 profile:tmp-entry && !profile:claude-code-tmp excludes /tmp/claude-1000 as a whole (model test with both written as user profiles in the fixture)
- [ ] #4 The any-depth rule is decided and documented in docs/reference.md
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-8 (profile model): Finds::Paths { patterns, entry_type: dir|file|any } are only checked structurally at load (relative, no empty, '.', '..' or '**'-containing segment, {uid} the only placeholder); the glob semantics within a segment are yours to define and document. A source may have its own where (Source.where_), which narrows it in every term (reference.md, Profiles fields); !profile: ignores it. Library::idle_for(&term) gives a term's combined idle requirement; has/has_not and !filter: evaluation are not implemented. Examples (Source.examples) are stored but not matched.

From task-8's review (S1): IdleFor::verdict(last_use, now) is three-way (Idleness::Idle / NotIdle / Unknown(Unknown::Unanswered | InFuture)), and Idleness::passes(negated) is false for Unknown under both filter: and !filter:, so an unknown age never selects a candidate; the skip reason is Unknown::skip_reason() (unsupported_fs, future_time; future_time is new in docs/reference.md's skip reasons). has / has_not need the same three-way result: a directory that cannot be listed or a name the kernel will not answer about is unknown, and must not pass !filter:has-... either. Also: a source's own where now allows only has / has_not (user decision U1), and protect, like !profile:, ignores where (docs/reference.md). find.type is defined: dir = directory, file = regular file, any = directory or regular file; symlinks, sockets, FIFOs and devices are never candidates. Root normalisation and identity dedupe of overlapping roots are filed on task-61 (39).

From task-10: executor::Tier::select refuses unions, !filter: and !profile: (exit 2 naming task-16/20) because the executor runs one positive cargo-target term. When complements arrive: cargo_target::eligibility/reclaim take only Option<IdleFor> (the term's positive requirement); extend that parameter to the term's idle rule (Idleness::passes(negated) per !filter:), and decide which skip reason a failed complement reports (reference.md's enum has none for 'too idle'; not_idle would mislead). A union of cargo-target terms with only positive idle filters reduces exactly to the shortest requirement; dedupe targets by identity across terms (task-7 H9).

From task-68 (profile spike, 2026-10-18). Code: crates/diskgc-core/src/fs_kind/ (pattern.rs: the matcher; plan.rs: discovery and judgement for the dry run).
- Glob syntax, decided and documented in reference.md (find.paths row): `*` matches any run of bytes in one name (also empty, also a leading dot), everything else literal; `?`, `[`, `]`, `\` are refused at load (reserved). Open: Python's tempfile.mkdtemp dirs are /tmp/tmpXXXXXXXX (3,457 here) and are only expressible as `tmp*` (overbroad) unless `?` or classes get a meaning: decide here.
- Nesting, a spike choice that needs a decision (it defines a profile's matching set and its golden sets): discovery never descends into a candidate, so a match inside another match of the same profile is not found (cmake-build's FetchContent sub-builds inside build/). Alternative: list both and let the reclaim find the inner gone.
- Any-depth (AC #4): explicit depths work and are cheap on this laptop: pytest-cache (4 patterns, depth <= 3) over $HOME and $HOME/topics in 6.5 s, cmake-build (`*`, `*/*`, `*/*/*` + has) in 6.4 s, cachedir-tagged (same) in 38 s. They miss deeper ones (pytest-cache found 63, find(1) 65 at depth <= 5 below ~/topics and ~/nvidia).
- has / has_not: implemented only as a source's own `where` (reference.md now says: a regular file of that name directly in the candidate directory, not followed; a file candidate holds nothing). A directory that cannot be read is neither: the spike refuses it as `unreadable`. As a term filter (`filter:has-cachedir-tag`) Tier::select refuses it ("task-16") rather than ignoring it, because Library::idle_for silently ignores non-idle predicates. Open: the skip-reason enum has `has_not` but no reason for a failed `has_not` (a forbidden file present), and `has_not` reads as "failed `has`".
- A source `where` takes each predicate once and names one regular file, so west-build cannot say "CMakeCache.txt plus a zephyr/ directory". Decide: list of names, a directory marker, or leave it.
- Directories on the way that cannot be read (another user's nix-shell.* below /tmp) are reported as plan skips although they are not candidates (as cargo discovery does). Decide whether plan entries may name non-candidates.

From task-68's review: the nested-match rule is a decision for Mark, not settled by the spike. The spike's dry run never descends into a candidate, so a match inside another match of the same profile is not found (the outer wins). It defines the profile's matching set (and its golden sets), so Mark decides it (outer only, or both) before the freeze.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
