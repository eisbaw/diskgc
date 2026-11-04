---
id: TASK-325
title: >-
  Dry run is ~0.2 s per candidate (6,429 pytest-tmp candidates hit the 10 min
  deadline): batch in-use scans like the reclaim walk (TASK-292)
status: In Progress
assignee: []
created_date: '2026-11-02T21:19:00Z'
updated_date: '2026-11-04T03:35:00Z'
labels:
  - performance
  - dry-run
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-02 asked whether a diskgc tally is explainable (does a per-profile size breakdown explain why we can or can't delete something). Read-only dry run at 40b2d85 over ~ and /tmp with rustup-toolchains || elan-toolchains || pytest-tmp: per-candidate verdicts explain themselves, but there is no per-profile/per-reason byte breakdown, kept groups carry no sizes ('137 kept: future_time'), and the run hit the 10m deadline (exit 124) after ~2,940 of 6,429 candidates. ~0.2 s per candidate in the dry-run plan versus ~1.1 ms per candidate in the reclaim walk after TASK-292 (found-ahead batches per scan, one mount-point lookup per directory). Profile the plan path (likely one in-use scan per candidate or small batches, repeated mountinfo reads, liveness contract per candidate) and apply the same batching within the freshness rules. Goal: the host dry run above completes well within the default timeout. Also look at why 137 pytest dirs are future_time (tests setting future mtimes?) and whether the reason detail says so.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-04: Mark: 'nix-store goes first then the visibility'. In progress in tmp/worktrees/t325.
<!-- SECTION:NOTES:END -->
