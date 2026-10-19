---
id: TASK-96
title: 'Decision for Mark: are Codex session logs (~/.codex/sessions) disposable?'
status: To Do
assignee: []
created_date: '2026-10-19 11:48'
labels:
  - home
dependencies: []
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the home survey (2026-10-18) found ~6.3 GiB of Codex session logs going back months; codex resume needs them and Codex never prunes them (Claude Code prunes its own after 30 days). Whether they are cache or the user's history is the user's call (raised 2026-10-19, left for Mark). If yes: a codex-sessions profile deleting day dirs with an idle filter (e.g. idle-30d).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Mark's decision recorded; if yes, a codex-sessions profile with examples and a dry run on this machine
<!-- AC:END -->
