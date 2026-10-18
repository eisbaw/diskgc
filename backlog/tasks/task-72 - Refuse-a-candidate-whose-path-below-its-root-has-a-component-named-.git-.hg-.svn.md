---
id: TASK-72
title: >-
  Refuse a candidate whose path below its root has a component named
  .git/.hg/.svn
status: Done
assignee: []
created_date: '2026-10-18 18:47'
updated_date: '2026-10-18 19:01'
labels:
  - wave-2
  - safety
dependencies: []
references:
  - docs/safety.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: the task-71 guard (contains_checkout) looks only inside a candidate. A broad pattern can find a candidate that lies inside a checkout's metadata, e.g. proj/.git/objects under a roots: required '*/*/*' pattern, and deleting that destroys the repository's history. Proposal (mped S3, task-71 review): a path-only check of the names between the profile root and the candidate, no syscalls, parallel to the keep-marker ancestor check; a component named .git, .hg or .svn refuses the candidate (contains_checkout). NEEDS MARK'S OK: it extends his task-71 decision (markers inside the candidate) to markers above it. Until decided, docs/safety.md (No source checkout, Not recognised) lists such candidates as a limit.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Mark's decision recorded in the task
- [x] #2 If accepted: a model test shows proj/.git/objects found by a broad pattern is contains_checkout, and a bite removing the check turns it red; the rule lives in docs/safety.md (The eligibility walk)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Approved by Mark (2026-10-18) and implemented in task-71's round 2 (same change): the fs planner refuses a candidate with a .git/.hg/.svn name on its path below the root as contains_checkout (model test a_candidate_inside_a_checkouts_metadata_is_contains_checkout_by_its_path, bite red). See task-71's notes.
<!-- SECTION:NOTES:END -->
