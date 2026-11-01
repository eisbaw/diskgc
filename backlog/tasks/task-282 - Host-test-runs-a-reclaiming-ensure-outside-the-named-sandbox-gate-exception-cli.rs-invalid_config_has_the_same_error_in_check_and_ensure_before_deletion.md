---
id: TASK-282
title: >-
  Host test runs a reclaiming ensure outside the named sandbox-gate exception
  (cli.rs invalid_config_has_the_same_error_in_check_and_ensure_before_deletion)
status: To Do
assignee: []
created_date: '2026-11-01T04:11:00Z'
updated_date: '2026-11-01T06:52:00Z'
labels:
  - test
  - safety
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the TASK-272 implementer (2026-11-01): crates/diskgc/tests/cli.rs invalid_config_has_the_same_error_in_check_and_ensure_before_deletion runs a reclaiming 'ensure' (no --dry-run) on the developer host; it exits 2 at the config check before touching anything, and dev builds are sandbox-gated, but CLAUDE.md names exactly one host test allowed to do that (a_reclaiming_ensure_outside_a_container_exits_2_naming_the_sandbox_gate) and says 'Do not add another'. Fix: make it a dry run (the config error is identical), or move it into a container test, or fold it into the named exception's fixture (own roots and PATH). Then add a guard test that greps the host test files for reclaiming ensure/watch invocations outside the allowlisted one, so this cannot recur. Check the other host test files for the same pattern.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: in progress in tmp/worktrees/t282 (branch t282-host-test).
<!-- SECTION:NOTES:END -->
