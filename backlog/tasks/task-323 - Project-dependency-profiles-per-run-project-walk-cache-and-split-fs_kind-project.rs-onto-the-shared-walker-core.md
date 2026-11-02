---
id: TASK-323
title: >-
  Project-dependency profiles: per-run project-walk cache and split
  fs_kind/project.rs onto the shared walker core
status: To Do
assignee: []
created_date: '2026-11-02T19:11:00Z'
labels:
  - refactor
  - performance
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From the TASK-79 review (mped S4/S5, 2026-11-02). (1) Cost: the project walk repeats per candidate (N walks of a monorepo; locate runs twice per candidate, reading the workspace manifest twice). Cache (project DevIno + mount id, contract) -> Walked per plan; in reclaim reuse only below a stated age or take one project's candidates consecutively (state the widened unseen-change window in safety.md Cost); prerequisite: rule 1 ignores directory mtime (TASK-79 S1); cap memory (Tree.objects can reach tens of MB at the 2M budget). (2) fs_kind/project.rs does four jobs (instance check + discovery; a second tree walker; rules 1+2 assembly; rule 3 placement + foreign-namespace translation); ProjectWalk re-implements engine::walk's invariants (no symlink, no mount crossing, identity recheck after open_beneath, budget, error classes): use a shared walker core with an exclusion predicate/visitor; move visible_use next to inuse/held.rs and translate next to the mountinfo model.
<!-- SECTION:DESCRIPTION:END -->
