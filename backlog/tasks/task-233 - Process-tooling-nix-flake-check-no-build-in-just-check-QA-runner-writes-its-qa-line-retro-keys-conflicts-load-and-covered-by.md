---
id: TASK-233
title: >-
  Process tooling: nix flake check --no-build in just check; QA runner writes
  its qa line; retro keys conflicts/load and covered-by
status: To Do
assignee: []
created_date: '2026-10-25 20:18'
labels:
  - process
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
COMPASS round 4 (2026-10-25): (1) add 'nix flake check --no-build' (~1 min) to just check so flake evaluation breaks (72fccd8) are caught per merge; (2) e2e expectations should reference reason enums or a check should match their literals against the enum (TASK-190 test strings); (3) the QA runner writes its own qa: note; tools/retro.py counts a tip e2e-all as covering the earlier shas since the last QA run (covered-by), so qa coverage is measurable; (4) add conflicts: and load: keys to the retro lines (experiments 2 and 3 unmeasured until then).
<!-- SECTION:DESCRIPTION:END -->
