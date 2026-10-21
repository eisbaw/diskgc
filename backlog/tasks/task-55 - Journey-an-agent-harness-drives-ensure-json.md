---
id: TASK-55
title: 'Journey: an agent harness drives ensure --json'
status: In Progress
assignee: []
created_date: '2026-10-16 22:47'
updated_date: '2026-10-21 11:18'
labels:
  - wave-2
  - journey
dependencies:
  - TASK-21
references:
  - docs/reference.md#output
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the agent harness is one of the PRD's three users and had no journey. Write a small harness script (as an agent harness would) that runs ensure --json before tasks and branches on exit codes 0, 1, 3 and 75 using the JSON, in a container with competing reclaims and a misconfigured tier.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The script handles each exit code without parsing human output
- [x] #2 Findings (missing JSON fields, unclear reasons) are filed as tasks
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-21: docs/agent-pre-job.py branches on real ensure JSON and exits 0/met, 1/exhausted, 3/error, and 75/lock_timeout. Focused Nix container journey agent_harness_uses_json_for_all_reclaim_decisions passed 1/1; every traced run had a clean deletion oracle. Exit 0 includes min-free met with target-free unmet; exit 1 uses two repeated roots; exit 3 reports a misconfigured PrivateTmp tier with tier/profile/candidate/message; exit 75 comes from a real competing ensure holding the host lock. Reviewed these JSON reports for missing fields or unclear reasons; none found, so no follow-up task was needed. Full just e2e, QA, and MPED gates remain pending before DoD/commit.

Agent JSON pre-job journey passed the final full gate (158/158 Nix image, 7/7 native Docker), with clean deletion oracle. Parallel qa-test-runner and mped-architect reviews found no blockers. No missing JSON field or unclear reason was observed; no follow-up task required.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
