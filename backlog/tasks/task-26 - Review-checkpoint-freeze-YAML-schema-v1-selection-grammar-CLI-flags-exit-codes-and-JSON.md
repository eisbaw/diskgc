---
id: TASK-26
title: >-
  Review checkpoint: freeze YAML schema v1, selection grammar, CLI flags, exit
  codes and JSON
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-17 20:42'
labels:
  - wave-2
  - review
  - irreversible
dependencies:
  - TASK-25
  - TASK-22
  - TASK-55
  - TASK-17
references:
  - docs/reference.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: these surfaces become a contract once anyone deploys diskgc; this is the last cheap moment to change them. Run mped-architect, a CLI/UX reviewer and a safety reviewer on docs/reference.md against the implementation; resolve findings before freezing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each reviewer's findings are fixed or explicitly accepted with a reason recorded in the task
- [ ] #2 docs/reference.md and the implementation agree (spot-checked by tests for every flag, field and grammar rule)
- [ ] #3 The command-action, Docker and Nix parts of the schema are marked provisional until the release review
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Pre-release items from the task-7 review (mped), to settle before the grammar freezes:
H1: did-you-mean suggestions for unknown names and unknown qualifiers.
H2: the mixing error says 'add parentheses' even where no parenthesisation is accepted (e.g. (profile:a || profile:b && filter:f), or when both readings break a static rule); say so instead.
H3: warn on profile:a && !profile:a (selects nothing) and on duplicate terms (profile:a || profile:a).
H4: escape control characters when echoing an expression in an error.
H5: errors restated from syntax errors (wrong qualifier at a term start, profile:F after &&) lose the column; carry it.
H7: stable error codes for --json, and a sentence in docs/reference.md that machines match codes, not message text (also on task-15).
H8d: the doc-drift test reads only docs/reference.md; PRD.md's expressions (e.g. in How it works) are untested; add PRD.md to workspaceTestSrc and test them.

From task-7's final review (mped, QA), for the grammar freeze: ((profile:a) && filter:f) alone gets 'drop them' with no rewrite though profile:a && filter:f is accepted; ((profile:a || profile:b) || profile:c) && filter:f reports a nesting error instead of suggesting the flat group (a || b || c) && f; PRD.md's Composition row says '|| only with parentheses' but profile:a || profile:b is valid (fix with H8d).
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
