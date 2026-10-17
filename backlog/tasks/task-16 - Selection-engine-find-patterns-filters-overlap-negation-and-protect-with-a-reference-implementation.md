---
id: TASK-16
title: >-
  Selection engine: find patterns, filters, overlap negation and protect, with a
  reference implementation
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-17 22:45'
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
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
