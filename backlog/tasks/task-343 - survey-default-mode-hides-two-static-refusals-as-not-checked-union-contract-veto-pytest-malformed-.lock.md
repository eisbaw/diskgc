---
id: TASK-343
title: >-
  survey default mode hides two static refusals as 'not checked' (union contract
  veto; pytest malformed .lock)
status: Done
assignee: []
created_date: '2026-11-04T21:37:00Z'
updated_date: '2026-11-05T05:38:00Z'
labels:
  - survey
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Codex review of the survey --in-use delta (2026-11-04), reproduced by Codex with focused model tests on b5b39920. Default (no --in-use) survey promises the process-independent checks still run (reference.md Survey), but: (1) fs_kind/plan.rs contract_veto: with python-venv || a broad venv profile, an old .venv whose project changed yesterday is refused not_idle by the project term, yet the unchecked union bypasses that veto and reports 'not checked'; fix: apply every recognising source's static contract veto before bypassing process-dependent rules. (2) fs_kind/plan.rs judge_walked / liveness.rs prepare: an old pytest session containing a .lock with non-PID content is producer_unknown with --in-use but 'not checked' by default; prepare handles only project contracts; fix: gather and apply pytest's process-independent refusals before the no-scan return. Conservative (never a false eligible; survey deletes nothing) but a contract break in survey's documented default. Tests: both as model tests that fail before the fix.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-05 orchestrator: implementer in tmp/worktrees/t343 (branch t343-survey-static, from master 8c8f9351).

Merged f8879ec3757311bbe456ba5f205aee8d3900966e (2026-11-05). Follow-ups: default-mode tests for failed listing of P and unreachable temp root; Owner PID local-filesystem rule statically?; fold check/veto into check_on/veto_on after TASK-340.
<!-- SECTION:NOTES:END -->
