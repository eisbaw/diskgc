---
id: TASK-73
title: >-
  Predicate: keep the target of a named symlink (current version of a
  self-updating tool)
status: To Do
assignee: []
created_date: '2026-10-18 20:53'
updated_date: '2026-10-18 23:16'
labels:
  - home
  - wave-4
  - irreversible
dependencies:
  - TASK-62
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: self-updating CLIs keep every release under a versions dir and point a symlink at the one in use (~/.local/share/cursor-agent/versions/* with ~/.local/bin/{agent,cursor-agent}; ~/.codex/packages/*/releases/* with standalone/current). The version in use can be months old, so idle_for alone would delete it. Extends task-62 (keep newest N): add a predicate that keeps any candidate that is (or contains) the resolved target of a named symlink, then profiles cursor-agent-versions and codex-releases (~8.3 GiB net reclaimable here). Source: read-only home survey 2026-10-18 (scratchpad home-survey/summary.txt; sizes on the dev laptop at that date).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A candidate that is the resolved target of the named symlink is never eligible (model test, incl. a dangling and a relative symlink)
- [ ] #2 cursor-agent-versions and codex-releases written with examples and verified by a read-only dry run
- [ ] #3 docs/reference.md documents the predicate (single home)
<!-- AC:END -->
