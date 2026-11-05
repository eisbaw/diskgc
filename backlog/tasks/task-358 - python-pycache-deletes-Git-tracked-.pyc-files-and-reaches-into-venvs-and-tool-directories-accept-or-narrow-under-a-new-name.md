---
id: TASK-358
title: >-
  python-pycache deletes Git-tracked .pyc files and reaches into venvs and tool
  directories: accept or narrow under a new name
status: To Do
assignee: []
created_date: '2026-11-05T16:54:00Z'
labels:
  - admission
  - profiles
  - '0.2'
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-350 host evidence (2026-11-05): of 692 __pycache__ under ~, 5 hold Git-tracked .pyc (some with no source, in saleae_logic_8_pro); others are inside venvs, ~/.claude/skills, ~/.cache/torch. Recoverable from Git but dirties the tree. Mark decides: accept the tolerated residual, or a successor profile skipping hidden directories, venvs and tracked files.
<!-- SECTION:DESCRIPTION:END -->
