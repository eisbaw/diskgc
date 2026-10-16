---
id: TASK-22
title: 'Journey: laptop owner''s /tmp under pressure with config groups and protect'
status: To Do
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-16 22:50'
labels:
  - wave-2
  - journey
dependencies:
  - TASK-21
references:
  - PRD.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: exercise profiles, tiers and protection as a user. In a container whose /tmp mirrors the survey (pytest-of-* inside nix-shell dirs, rc dirs, a Claude Code temp dir, a live shell's TMPDIR, sockets), write a config with stale and pressure groups and protect: [claude-code-tmp], then run ensure --dry-run and ensure.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Protected and in-use entries survive; the deletion oracle agrees with the dry-run plan for independent candidates
- [ ] #2 Findings are filed as tasks
- [ ] #3 The fixtures use user profiles (the built-in library ships in wave 4)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
