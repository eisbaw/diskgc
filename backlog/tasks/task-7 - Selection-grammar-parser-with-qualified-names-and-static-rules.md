---
id: TASK-7
title: Selection grammar parser with qualified names and static rules
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:55'
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
- [ ] #1 profile:a || profile:b && filter:f is a parse error with a message showing the two parenthesised readings
- [ ] #2 Every grammar example in docs/reference.md parses and normalises as documented (table-driven test)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
