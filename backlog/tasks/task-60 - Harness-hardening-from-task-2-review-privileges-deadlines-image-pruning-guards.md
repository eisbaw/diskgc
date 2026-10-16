---
id: TASK-60
title: >-
  Harness hardening from task-2 review: privileges, deadlines, image pruning,
  guards
status: To Do
assignee: []
created_date: '2026-10-16 23:54'
labels:
  - wave-1
  - hardening
  - testing
dependencies:
  - TASK-2
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: lower-priority findings from the task-2 review, fixed once the harness is stable. (1) Set no-new-privileges and assert CapAmb and CapInh are 0 for the code under test (the bounding set still holds SYS_ADMIN and SYS_PTRACE). (2) The budget does not cover check, the nix build, test listing or image loading; give those deadlines too. (3) Old base-image tags accumulate; prune them (a disk GC tool should not leak disk). (4) Scratch::mount calls sandbox::check() again as a second guard. (5) A clippy disallowed-methods rule for std::fs writes outside scratch.rs. (6) An automated test for e2e-one keeping the container on failure (verified manually only).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each of the six items is implemented with a test or a recorded reason for not doing it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
