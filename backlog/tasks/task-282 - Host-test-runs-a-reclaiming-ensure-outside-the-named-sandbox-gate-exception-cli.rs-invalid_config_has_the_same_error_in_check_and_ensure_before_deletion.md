---
id: TASK-282
title: >-
  Host test runs a reclaiming ensure outside the named sandbox-gate exception
  (cli.rs invalid_config_has_the_same_error_in_check_and_ensure_before_deletion)
status: To Do
assignee: []
created_date: '2026-11-01T04:11:00Z'
updated_date: '2026-11-01T08:41:00Z'
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

2026-11-01 implementer: committed 3a682d9 on t282-host-test (rebased on master 759338a; not merged). cli.rs reclaiming argv converted to --dry-run/--tally-only; the_release_catalog_refuses_a_reclaiming_watch removed (container twin release_candidate_refuses_a_reclaiming_watch + main.rs unit message pin); test_agent_pre_job PATH confined to the stand-in; guard tools/host_reclaim_scan.py (+ test, in just check). Gates: nix develop -c just check exit 0 (tmp/t282-check2.log, 1457 Rust + 109 Python tests). just e2e NOT run (t292 benchmark in a walk container at the time). Bite: dropping --dry-run in cli.rs and adding a reclaiming watch to test_agent_pre_job.py each went red, restored green. Follow-ups: diskgc-linux host.rs gate unit tests (deleting_outside_a_container_is_refused_by_the_gate, the_engine_walks_here_but_its_delete_is_refused_by_the_gate) attempt deletion on the host behind the gate, unnamed in CLAUDE.md/testing.md; one shared definition of reclaiming argv with docs_vs_binary.host_refusal; CLAUDE.md says watch without --dry-run but watch's read-only form is --tally-only.
<!-- SECTION:NOTES:END -->
