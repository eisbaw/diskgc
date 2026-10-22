---
id: TASK-110
title: Consolidate development scratch under repo-local tmp and tally before tasks
status: Done
assignee: []
created_date: '2026-10-22 11:36'
updated_date: '2026-10-22 12:53'
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
- [x] #1 New development worktrees and controllable scratch paths are created beneath a single repo-local ./tmp/ parent; document intentional exceptions such as Nix store and container-private /tmp.
- [x] #2 ./tmp/ is ignored by Git, and a check proves nested worktrees/build outputs there do not appear as untracked project files.
- [x] #3 The migration plan inventories existing /tmp/diskgc* worktrees and build outputs, distinguishes active/dirty data, and does not remove host data without explicit user authorization.
- [x] #4 Tests or a demonstrated end-to-end workflow show that repo-local worktrees, builds and E2E runs still function and do not create new uncontrolled /tmp/diskgc* directories.
- [x] #5 A repeatable pre-task tally uses filesystem-bounded, no-symlink-following traversal and reports total and per-child ./tmp/ usage, legacy /tmp/diskgc* usage, free space, and change since the preceding tally; a documented free-space or growth threshold produces a clear warning before each backlog implementation task.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-22: Consolidated controllable development scratch under gitignored repo tmp/ through the dev shell, E2E runner and Just recipes; added pre-task size tally with bounded, atomic sample history, growth/free-space warnings, and a read-only path-level legacy inventory. The legacy /tmp/diskgc* inventory has 503 entries using 133.79 GiB: 88 registered worktrees, 60 dirty and 28 clean by Git status, with process references only a snapshot because some fd tables are unreadable. Existing host data was not deleted or moved. Verified nested tmp/worktrees/task110-smoke worktree and Git ignore behavior; targeted E2E created no new legacy entries. Parallel MPED and QA reviews passed. Exact nix develop -c just e2e exited 0: Python 4/4, workspace tests, Podman 217/217, Docker 7/7, historical ctime VM and four pidfd VMs passed. /tmp/diskgc* count remained 503; free space fell from 20 GiB to 14 GiB during the gate.
<!-- SECTION:NOTES:END -->
