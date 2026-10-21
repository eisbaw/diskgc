---
id: TASK-23
title: 'Model scenarios from docs/testing.md (races, owners, deadlines, root refusal)'
status: In Progress
assignee: []
created_date: '2026-10-16 22:40'
updated_date: '2026-10-21 12:19'
labels:
  - wave-2
  - testing
dependencies:
  - TASK-21
references:
  - docs/testing.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the required scenarios pin the honest limits and the race handling. Implement the model scenarios in docs/testing.md that do not need the index: rename or replace during delete, candidate renamed mid-delete, mixed owners, TMPDIR exported after exec, deadline overruns, command actions refused under --allow-root; plus: an interrupted delete leaves directories with fresh mtimes (pin how they are treated by idle filters), and a file with mtime 1 (copied from the Nix store) but a fresh ctime is not idle. The bucket scenario belongs to the index task.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each listed scenario exists as a named fixture and passes
- [ ] #2 Each scenario is shown to fail when the check it covers is removed (bite proof, once)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Scenario mapping: entry replacement during delete, mixed owners, interrupted-delete fresh directory mtime, and Unix-second-1 mtime with fresh ctime are named in crates/diskgc-model/tests/task23_scenarios.rs (four focused Nix tests pass). Entry renamed out and candidate renamed mid-delete are in crates/diskgc-model/tests/engine.rs; post-exec TMPDIR invisibility is in fs_dry_run.rs; blocking deadline overrun and no later work is in signals.rs; root command refusal is in command_port.rs and config.rs. Bite proof: in a disposable Podman container with host repo mounted read-only, changed only the container copy of engine/walk.rs from m.max(c) to m; a_file_with_old_mtime_and_fresh_ctime_is_not_idle failed as expected (left Idle, right NotIdle). Host source passed 4/4 before mutation. This proves ctime check bite; AC2 for every scenario remains open pending other mutation proofs. No host files deleted.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
