---
id: TASK-366
title: >-
  Project contracts: one transiently unreadable command line marks every project
  in use (argv_naming), so project profiles rarely reclaim on a busy host
status: To Do
assignee: []
created_date: '2026-11-06T11:02:00Z'
labels:
  - busy-host
  - project-contracts
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-348 e2e run 1 on bf1d954d (2026-11-06), raw logs tmp/worktrees/t348/tmp/t348-evidence-e2e3/: candidate_catalog_project_tree_family_matches_the_development_build failed: a container process (pid 2515150) was readable in the scan but exiting when its command line was read ('no listed task gave its command line while live'); fs_kind/project.rs argv_naming keeps EVERY project in_use when any visible process's command line cannot be read; inuse/held.rs::collect folds every cmdline error except Gone/Exhausted into the same 'may name the project' text, so Failed and Unsettled are treated alike. Safe (only keeps), but on a busy laptop python-venv/node-modules rarely reclaim. Do: an Unsettled (exiting/churning) command-line read counts as busy (back off and retry, TASK-364 semantics) or is re-read, while a lasting unreadable command line still blocks; deterministic model test from this shape; keep 'never clear on an unread argv'. Relates to TASK-364 (do together or right after).
<!-- SECTION:DESCRIPTION:END -->
