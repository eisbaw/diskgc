---
id: TASK-327
title: >-
  node-modules/python-venv follow-ups: container tests with real npm and python;
  uv self-editable installs; the rule-2 narrowing ruling
status: To Do
assignee: []
created_date: '2026-11-02T22:59:00Z'
labels:
  - profiles
  - test
  - needs-mark
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-79 (merged 2026-11-02, dev library). (1) AC4 carried: container tests with real npm/pnpm/yarn and python/uv producers (read-only use, running dev server, editable installs, npm link). (2) Mark: rule 2 narrowed to reads inside the project's dependency directories, falling back to project-wide reads on link-outs (PENDING his confirmation in safety.md). (3) Mark: uv installs every project editable into its own .venv (a .pth pointing at the project's own source), so the narrowing never applies to uv projects (16 venvs, 2.2 GiB on his laptop); proposal: a .pth/egg-link pointing into the venv's OWN project counts as inside, not a link-out. (4) Mark: ~/node_modules + ~/package.json + ~/package-lock.json (an npm install in home, Dec 2025) make every node project under home a workspace member (filtered); his files, his call. (5) Find what read every .venv/CACHEDIR.TAG on 2026-10-30 23:33 (7 venvs kept). (6) TASK-323 (walk cache, module split).
<!-- SECTION:DESCRIPTION:END -->
