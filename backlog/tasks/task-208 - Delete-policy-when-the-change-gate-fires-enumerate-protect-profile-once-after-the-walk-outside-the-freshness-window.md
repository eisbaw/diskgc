---
id: TASK-208
title: >-
  Delete policy: when the change gate fires, enumerate protect/!profile once
  after the walk, outside the freshness window
status: To Do
assignee: []
created_date: '2026-10-25 11:37'
labels:
  - shipping
  - performance
  - safety
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-200 review (finding 4). When a candidate's walk shows a change since the tier's enumeration (or the wall clock drifted > 3 s vs monotonic), DeletePolicy::skip re-enumerates every protect and negated tree inside clear_and_delete's freshness window. With a large protect (61k dirs: 2-3 s) that expires the clear on every attempt: such a candidate costs three full enumerations and ends in_use_unknown, as every candidate did before TASK-200. Sound alternative: enumerate once after the walk and before the in-use check, and judge the final check against that set (any match made before the walk ended is in it). For a drifted clock, refresh the tier's Protection once rather than per candidate.
<!-- SECTION:DESCRIPTION:END -->
