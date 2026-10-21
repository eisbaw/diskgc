---
id: TASK-107
title: >-
  Adjudicate TASK-23 bite proof for observational limit and redundant deadline
  stops
status: Done
assignee: []
created_date: '2026-10-21 21:39'
updated_date: '2026-10-21 21:41'
labels:
  - wave-2
  - testing
  - decision
dependencies: []
references:
  - docs/testing.md
  - docs/safety.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Resolve TASK-23 AC2 proof semantics for two cases where literal single-check removal cannot represent the behavior. A post-exec TMPDIR export is invisible to Linux /proc initial-environment reads when no descriptor is held, so the fixture documents a false negative and uses counterfactual visibility as its bite proof. No-next-work has two independent deadline stops, so the behavioral fixture must fail when both are removed while the later candidate is independently eligible. Record the decision and exact evidence on TASK-23 without changing the safety behavior.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Decide whether the post-exec TMPDIR scenario is proven by its model and disposable-container negative test plus counterfactual visibility change, or specify a new observable guarantee to implement.
- [x] #2 Decide whether removing both independent deadline stops is the appropriate bite proof for no-next-work; retain a test where the second candidate is independently shown eligible and no reclaim enters it.
- [x] #3 Record the accepted criterion and exact evidence on TASK-23 through backlog CLI; only then mark TASK-23 complete if all remaining requirements hold.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision 2026-10-21: accept a counterfactual visibility proof for the post-exec TMPDIR observational limit. docs/safety.md says /proc/<pid>/environ exposes the initial environment only; ModelHost fs_dry_run::dormant_post_exec_tmpdir_is_a_known_process_scan_false_negative and disposable Linux container e2e::dormant_exported_tmpdir_is_invisible_and_broad_builtins_are_absent pin the eligible false negative without deletion. Changing ModelHost proc_environ to expose the later TMPDIR makes the named fixture fail. There is no safety guard that can observe the later export in this contract. Accept joint removal of ReclaimRun.record None-return and protected_before_reclaim deadline check for no-next-work bite proof. Either removed alone leaves the other guard effective; both removed make signals::a_blocking_candidate_operation_reports_overrun_and_starts_no_next_candidate fail and start reclaim on /home/mark/src/new/target. That fixture separately proves the second target eligible in dry run and no entry into its reclaim in the baseline. This adjudicates TASK-23 scenario proof only. The broader docs/testing.md obligation to test each safety guard independently remains for wave hardening TASK-13; no claim that either deadline guard alone is killed by this scenario.
<!-- SECTION:NOTES:END -->
