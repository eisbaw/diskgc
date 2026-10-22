---
id: TASK-110
title: Consolidate development scratch under repo-local tmp and tally before tasks
status: To Do
assignee: []
created_date: '2026-10-22 11:36'
updated_date: '2026-10-22 11:38'
labels:
  - tooling
  - safety
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Development agents have accumulated 503 /tmp/diskgc* entries using 133.79 GiB on 2026-10-22, including 88 registered Git worktrees. About 116.8 GiB is build target output; the root filesystem had 36 GiB free. Establish one project-owned, gitignored ./tmp/ parent for new worktrees, build outputs, logs and other agent scratch where feasible. Before starting each backlog implementation task, inspect this project's scratch size and filesystem free space, including legacy /tmp/diskgc* until migrated. This is development-workflow storage, not a change to the product's real /tmp profile behavior. Current user constraint: no host deletion without a new explicit instruction; inventory and migration planning are read-only.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 New development worktrees and controllable scratch paths are created beneath a single repo-local ./tmp/ parent; document intentional exceptions such as Nix store and container-private /tmp.
- [ ] #2 ./tmp/ is ignored by Git, and a check proves nested worktrees/build outputs there do not appear as untracked project files.
- [ ] #3 The migration plan inventories existing /tmp/diskgc* worktrees and build outputs, distinguishes active/dirty data, and does not remove host data without explicit user authorization.
- [ ] #4 Tests or a demonstrated end-to-end workflow show that repo-local worktrees, builds and E2E runs still function and do not create new uncontrolled /tmp/diskgc* directories.
- [ ] #5 A repeatable pre-task tally uses filesystem-bounded, no-symlink-following traversal and reports total and per-child ./tmp/ usage, legacy /tmp/diskgc* usage, free space, and change since the preceding tally; a documented free-space or growth threshold produces a clear warning before each backlog implementation task.
<!-- AC:END -->
