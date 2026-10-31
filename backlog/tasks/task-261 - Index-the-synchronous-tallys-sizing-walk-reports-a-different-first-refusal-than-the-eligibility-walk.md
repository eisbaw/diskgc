---
id: TASK-261
title: >-
  Index: the synchronous tally's sizing walk reports a different first refusal
  than the eligibility walk
status: To Do
assignee: []
created_date: '2026-10-31T01:54:00Z'
labels:
  - index
  - test
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-252: tally/size.rs checks no owners, so another user's 0333 directory is unreadable in the tally and not_owner in the eligibility walk. Same path, same deletion set, different reason. The index_authority property narrows its skip-reason comparison for skips 'refused by this run's tally' (coupled to that detail text). Make the sizing walk's first refusal agree (cf. TASK-245) and drop the narrowing.
<!-- SECTION:DESCRIPTION:END -->
