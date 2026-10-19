---
id: TASK-92
title: >-
  diskgc --help lists the available profiles; profiles list/show with --json
  (agent onboarding)
status: To Do
assignee: []
created_date: '2026-10-19 11:48'
labels:
  - profiles
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the user's goal (2026-10-19): AI agents must be able to onboard themselves from the binary alone. diskgc --help must say which profiles exist (built-ins and the config's user profiles), what each finds, its category, kind and action, whether it needs roots, and its checkouts setting, and point to diskgc profiles list / show NAME for details. Pulled forward from task-17 (list/show), without its golden-set machinery. Output contract: human text plus --json (one object); documented in docs/reference.md (Commands, profiles command).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 diskgc --help ends with a 'Profiles' section listing every built-in profile (name, category, one-line description) and how to list user profiles and see details
- [ ] #2 diskgc profiles list [--config FILE] [--json] lists built-in and user profiles with name, category, kind, action, roots (fixed or required), checkouts; profiles show NAME [--json] prints the full definition incl. examples
- [ ] #3 An e2e or CLI test runs the real binary and checks --help and profiles list/show against the built-in library (no drift: a new built-in appears without editing the help text)
- [ ] #4 docs/reference.md documents the output and the JSON shape (pre-freeze addition noted on task-26)
<!-- AC:END -->
