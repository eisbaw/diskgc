---
id: TASK-201
title: >-
  Shared CARGO_TARGET_DIR across worktrees links one worktree's crates against
  another's: gate results unreliable
status: To Do
assignee: []
created_date: '2026-10-25 09:01'
labels:
  - tooling
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found in TASK-184 (2026-10-25). Cargo's metadata hash of a workspace crate does not include the worktree path (diskgc_core-ea87323b91a3e6e5 in tmp/target of the main checkout and of tmp/worktrees/t184 alike), and its fingerprint dep-info names the other tree's source files. With `nix develop -c env CARGO_TARGET_DIR=<main>/tmp/target just check` (the phase3.1 gotcha's advice) from two worktrees: (1) a t184 run on 07e840b passed with 323 diskgc-core and 31 cli tests and without the commit's new test, i.e. it ran another tree's test binaries (tmp/t184-check2.log); (2) a later run failed compiling t184's crates/diskgc/src/main.rs against another tree's diskgc-core (Report::new takes 3 arguments; tmp/t184-check4.log). Any gate evidence produced with a shared target while another worktree built may be wrong. Fix the class: one target dir per worktree (disk cost ~3+ GB each; prune merged worktrees), or a lock that serialises whole gate runs, not only builds; update the phase3.1 gotcha; audit gate claims made with the shared target.
<!-- SECTION:DESCRIPTION:END -->
