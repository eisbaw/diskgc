---
id: TASK-62
title: 'Profile filter: keep the newest N versions'
status: To Do
assignee: []
created_date: '2026-10-17 11:12'
labels:
  - wave-2
  - profiles
  - dogfood
dependencies:
  - TASK-8
references:
  - docs/reference.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the 2026-10-17 dogfooding run found old agent CLI versions the grammar cannot select safely: ~/.local/share/cursor-agent/versions (32 versions, 5.4G) and ~/.codex/packages releases (10, 3.7G). idle_for alone could select the installed-but-not-running version. Add a where-filter that keeps the newest N siblings (by version order or mtime, decided in the task) and selects the rest, with golden match sets, and ship profiles for these two tools.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A profile with keep_newest: N never selects any of the N newest siblings (model test)
- [ ] #2 The installed version (resolved symlink target or the tool's current pointer) is never selected even if it is not among the newest
- [ ] #3 Golden match sets for cursor-agent and codex layouts
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
