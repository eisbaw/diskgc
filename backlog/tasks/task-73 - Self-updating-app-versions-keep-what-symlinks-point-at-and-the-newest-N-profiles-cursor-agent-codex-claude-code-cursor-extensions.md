---
id: TASK-73
title: >-
  Self-updating app versions: keep what symlinks point at and the newest N;
  profiles cursor-agent, codex, claude-code, cursor extensions
status: To Do
assignee: []
created_date: '2026-10-18 20:53'
updated_date: '2026-10-19 12:35'
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
- [ ] #4 Profiles cursor-agent-versions, codex-releases, claude-code-versions and cursor-extensions with examples; a dry run here keeps both live cursor-agent versions plus the newest N and lists the rest with sizes matching du
- [ ] #5 A version a running process executes from is in_use (container test)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Widened 2026-10-19 (user: self-updating software leaves old versions accruing, e.g. cursor under ~/.local). Measured read-only here: ~/.local/share/cursor-agent/versions 31 dirs 5.2 GB (TWO live versions: ~/.local/bin/agent -> 2026.06.24, ~/.local/bin/cursor-agent -> 2026.07.16), ~/.codex/packages/standalone/releases 10 dirs 3.4 GB + app-server-daemon 0.4 GB (current link), ~/.local/share/claude/versions 3 dirs 0.8 GB (~/.local/bin/claude), ~/.cursor/extensions 33 dirs 0.45 GB (old extension versions beside new), ~/.local/share/uv/python 0.4 GB (uv's own 'uv python uninstall' instead). Safe rule: a version dir is eligible only if (1) no symlink in the configured link dirs (~/.local/bin, ~/bin) or the app's own current link resolves into it - several can be live at once; (2) not among the newest N (default 2) by version/mtime; (3) idle; (4) not in use (a running binary from it is in_use via /proc exe). The ~/bin/claude-2.1.* binaries (14) look hand-managed: out of scope unless Mark says otherwise.

2026-10-19 user: ignore ~/bin/claude-* entirely (not a target). The ~/.local/share Cursor case is ~/.local/share/cursor-agent/versions (the only cursor dir there).
<!-- SECTION:NOTES:END -->
