---
id: TASK-122
title: >-
  Classify the liveness contract of every shipped built-in source (TASK-112
  admission rule)
status: To Do
assignee: []
created_date: '2026-10-23 16:33'
labels:
  - profiles
  - design
dependencies:
  - TASK-112
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: TASK-112's design (docs/safety.md Producer liveness) makes a liveness contract class (sound, producer-equivalent, verified heuristic, tolerant, delegated, none) a precondition for every built-in source; the rule's liveness table (docs/reference.md Liveness contracts) covers only the profiles TASK-112 decided. The shipped development-catalog built-ins predate the rule. Known gap: claude-code-tmp documents that an active session can hold no scratch fd (reference.md note), i.e. class none or heuristic without a recorded decision. Also review chromium-tmp, playwright-tmp, node-compile-cache, renode-tmp, zephyr-tmp, jest-cache, xdg-cache-entry-v2, the version/extension profiles and the cache/command profiles. For each: name the producer, the class with evidence (tool docs or a container probe), the residual, and either a liveness-table row or withdrawal before release. If two or more /tmp profiles want the pytest protocol's temp-root-users rule, promote it to a generic where predicate (none exists before that second user).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every built-in source in builtin.yaml has a row in docs/reference.md Liveness contracts with its class and residual
- [ ] #2 A test keeps the liveness table and builtin.yaml in sync (every source has exactly one row)
- [ ] #3 Every row of class none is withdrawn from the built-in library or explicitly recorded as a Mark decision
<!-- AC:END -->
