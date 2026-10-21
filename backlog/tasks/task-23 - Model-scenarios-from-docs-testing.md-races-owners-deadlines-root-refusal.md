---
id: TASK-23
title: 'Model scenarios from docs/testing.md (races, owners, deadlines, root refusal)'
status: Done
assignee:
  - Codex
created_date: '2026-10-16 22:40'
updated_date: '2026-10-21 21:54'
labels:
  - wave-2
  - testing
dependencies:
  - TASK-21
  - TASK-107
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
- [x] #1 Each listed scenario exists as a named fixture and passes
- [x] #2 For each named scenario, record a failing mutation of its applicable safety check. For the post-exec TMPDIR visibility limit, instead record the false negative on ModelHost and LinuxHost plus a failing counterfactual with post-exec environment visible. For no-next-work after deadline, record failure when both independent stop checks are removed, while proving the next candidate is otherwise eligible.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Verify named model scenarios and mutation evidence. TASK-107 adjudicated the two proof exceptions precisely: counterfactual visibility for the documented post-exec TMPDIR limit, and a joint mutation of independent deadline stops for no-next-work. Retain the baseline safety behavior and record the remaining per-guard mutation obligation under wave hardening TASK-13.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Scenario mapping: entry replacement during delete, mixed owners, interrupted-delete fresh directory mtime, and Unix-second-1 mtime with fresh ctime are named in crates/diskgc-model/tests/task23_scenarios.rs (four focused Nix tests pass). Entry renamed out and candidate renamed mid-delete are in crates/diskgc-model/tests/engine.rs; post-exec TMPDIR invisibility is in fs_dry_run.rs; blocking deadline overrun and no later work is in signals.rs; root command refusal is in command_port.rs and config.rs. Bite proof: in a disposable Podman container with host repo mounted read-only, changed only the container copy of engine/walk.rs from m.max(c) to m; a_file_with_old_mtime_and_fresh_ctime_is_not_idle failed as expected (left Idle, right NotIdle). Host source passed 4/4 before mutation. This proves ctime check bite; AC2 for every scenario remains open pending other mutation proofs. No host files deleted.

2026-10-21 TASK23 proof pass: added a later walk verdict to interrupted_delete_makes_the_remaining_directory_not_idle; nix develop -c cargo test -p diskgc-model --tests --locked passed (all model test binaries). In disposable Podman debian:trixie-slim, host repo and Nix store were read-only mounts; source/target mutations lived only in container overlay. Each mutation compiled and named test failed exit 101: checked_unlink identity comparison removed -> replacing_an_entry_after_walk_keeps_the_replacement (false Complete); owner allowlist check removed -> mixed_owner_directory_is_refused_before_any_delete (false accepted WalkRecord); walk newest_change m.max(c) -> m -> a_file_with_old_mtime_and_fresh_ctime_is_not_idle (false Idle); exclude directory timestamps from newest_change -> interrupted_delete_makes_the_remaining_directory_not_idle (later walk false Idle); checked_unlink ENOENT Changed -> Removed -> an_entry_renamed_out_after_the_walk_is_changed_and_kept and a_candidate_renamed_mid_delete_is_still_the_object_deleted (false Complete); disable Stack held-fd fast path -> candidate renamed mid-delete test (1 vs 7 removed entries); disable ReclaimRun.record_overrun condition -> a_blocking_candidate_operation_reports_overrun_and_starts_no_next_candidate (overruns empty); disable config root-mode action validation -> mode_checks_refuse_owners_mismatch_and_root_commands (unexpected Ok); disable initial-environment path match -> dormant_post_exec_tmpdir_is_a_known_process_scan_false_negative (initial TMPDIR incorrectly eligible). A counterfactual ModelHost proc_environ returning a post-exec TMPDIR for pid 20 makes the same limit fixture fail (post-exec candidate in_use rather than eligible). This last mutation demonstrates the documented observational limit, not removal of a safety guard; there is no live post-exec environment in the Host contract. AC2 remains open for that wording and for no-new-work half of the deadline fixture's bite. No host deletion or host source mutation.

Follow-up no-next-work bite (2026-10-21): the original signals fixture's second Cargo target had Oct 14 last-build age against Oct 15 clock, so it was not idle-3d eligible and the no-next-work assertion was vacuous. Refactored the test model helper to supply an Oct 2 second target only for this fixture, asserted a separate dry-run plan includes it as eligible, and asserted reclaim never even logs entry into the second candidate. Focused 22/22 signals tests and cargo fmt check pass. Disposable Podman isolated source-copy mutations: remove only Reclaim::record's None=>return after the first stopped outcome: named fixture stays green, because protected_before_reclaim checks the passed deadline; remove only that pre-reclaim deadline guard: named fixture stays green because record returns; remove both: named fixture fails exit 101 and log shows reclaiming /home/mark/src/new/target after deadline. The two guards are deliberate redundant stops; no individual guard removal can make new work start. The grouped mutation proves the strengthened fixture detects the unsafe behavior. AC2 remains open because its literal guard-removal wording does not fit the post-exec TMPDIR observational limit (and a single no-next guard removal is resisted by the other stop).

2026-10-21 verification: nix develop -c cargo test -p diskgc-model --test task23_scenarios --test signals --test engine --test fs_dry_run --test command_port --locked passed 129/129 tests (4+22+43+54+6). The already recorded isolated Podman mutation pass proves all enumerated safety checks where a unique check exists, and a grouped mutation proves no-next-work with both independent deadline stops removed. Post-exec TMPDIR remains an explicitly documented /proc visibility limit, not a check that can be removed. TASK-107 records the precise acceptance decision needed for the literal AC2 wording. No host deletion, and no source code change in this verification pass.

TASK-107 decision applied 2026-10-21: AC2 now states the actual scenario proof standard. The post-exec TMPDIR case uses the named ModelHost negative fixture at crates/diskgc-model/tests/fs_dry_run.rs:2352, the disposable LinuxHost dry-run at diskgc-e2e/src/temp_safety_suite.rs:152, and the recorded counterfactual ModelHost proc_environ mutation that makes the named fixture fail. The limit is explicitly documented at docs/safety.md and docs/testing.md; no host delete was run. The deadline case uses crates/diskgc-model/tests/signals.rs:187: the second target is independently eligible in dry run, baseline never enters its reclaim, and removing both ReclaimRun.record None-return and protected_before_reclaim deadline guard makes the fixture fail by entering that reclaim. Each single removal stays green because the other stop remains. The other unique-check mutation evidence is listed above. This closes the scenario-level TASK-23 criterion; it does not assert that either deadline guard alone has a failing mutant. The independent per-guard mutation obligation from docs/testing.md is deferred to hardening TASK-13.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Named model scenarios and disposable-container mutation evidence pass. TASK-107 adjudicated post-exec TMPDIR as a documented observational limit with counterfactual visibility proof, and redundant deadline stops by a joint mutation with an independently eligible second candidate. Required nix develop -c just e2e passed: 169/169 Nix containers and 7/7 Docker containers; fmt, Clippy, workspace tests, and release checks passed. Independent QA and MPED review GO. Per-guard mutation hardening remains TASK-13; no host deletion.
<!-- SECTION:FINAL_SUMMARY:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
