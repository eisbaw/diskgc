---
id: TASK-226
title: >-
  docs-vs-binary runs the --help example on the host with the gate-lifted
  release binary; force --dry-run or refuse
status: To Do
assignee: []
created_date: '2026-10-25 14:43'
labels:
  - safety
  - tooling
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
RC QA on 72fccd8 (2026-10-25): tools/docs_vs_binary.py's '--help Start here' probe runs whatever the help example says, without adding --dry-run. Against the release artifact (sandbox gate lifted) on the host, a help example without --dry-run would be a real reclaim on the developer's machine (CLAUDE.md forbids it). Fix: on the host, refuse to run any ensure command lacking --dry-run (fail the check loudly), or inject --dry-run; reclaiming probes only with --container NAME. Test the refusal.
<!-- SECTION:DESCRIPTION:END -->
