---
id: TASK-298
title: >-
  ensure refuses (exit 2) under hidepid / outside the initial PID namespace even
  when --min-free is already met
status: To Do
assignee: []
created_date: '2026-11-01T16:39:00Z'
updated_date: '2026-11-01T16:40:00Z'
labels:
  - contract
  - safety
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #3 on master fc33d98 (2026-11-01; Codex tmp/reviews/codex-full3.md + mped), static reading. mped BLOCKER: invocation.rs run calls process_visibility before anything is measured, so ensure exits 2 even when the goal is met; PRD: 'ensure exits 0 without taking the lock when space is already there', reference.md --min-free says the same. Every containerised CI pre-job hook (GitLab docker executor, actions-runner-controller) fails although nothing needs freeing; WSL2 may be hit (check). Same for reclaiming watch start (watch/mod.rs start step 4b) - decide: watch samples per cycle, so refuse at the first cycle that would reclaim, or keep start refusal. Fix: take the first sample before the visibility check; goal met -> exit 0 as the usual no-op; refuse only when a reclaim would start. Dry run: unchanged (refused, it predicts a reclaim). Model tests red first. Independent of TASK-296's policy.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-01: in progress in tmp/worktrees/t298 (branch t298-goal-met-first).
<!-- SECTION:NOTES:END -->
