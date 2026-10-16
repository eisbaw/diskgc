---
id: TASK-55
title: 'Journey: an agent harness drives ensure --json'
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
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
- [ ] #1 The script handles each exit code without parsing human output
- [ ] #2 Findings (missing JSON fields, unclear reasons) are filed as tasks
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
