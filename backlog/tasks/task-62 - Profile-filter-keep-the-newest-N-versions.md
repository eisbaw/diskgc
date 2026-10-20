---
id: TASK-62
title: 'Profile filter: keep the newest N versions'
status: Done
assignee: []
created_date: '2026-10-17 11:12'
updated_date: '2026-10-20 02:26'
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
- [x] #1 A profile with keep_newest: N never selects any of the N newest siblings (model test)
- [x] #2 The installed version (resolved symlink target or the tool's current pointer) is never selected even if it is not among the newest
- [x] #3 Golden match sets for cursor-agent and codex layouts
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implementation candidate in isolated /tmp/diskgc-version-profiles, not yet integrated or committed: source and positive filter keep_newest N rank matching siblings by mtime with deterministic name tie break; missing timestamps or incomplete listings retain candidates. Model golden tests cover rank and filter selection; just check passes. The installed pointer rule is paired in TASK-73.

2026-10-20 integrated test-grounded result: keep_newest ranks matching siblings by mtime with descending name-byte tie break, retaining candidates on missing timestamps or incomplete listings. The model test keep_newest_filter_really_narrows_a_source_term bites the mutation that drops the keep_newest filter: the bare tier selects v2 and v3, while keep_newest: 2 retains both. The shipped Cursor agent and Codex release golden sets and linked-current protections are covered in TASK73, with read-only host dry runs. Final integrated E2E gate pending.

2026-10-20 final integrated gate: nix develop -c just check passed; nix develop -c just e2e passed 107/107. Independent QA and MPED architecture reviews returned GO. Earlier pending-gate note is superseded.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
