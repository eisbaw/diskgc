---
id: TASK-324
title: >-
  Explainable results: per-profile, per-reason summary with bytes in dry run,
  ensure and watch human output (and JSON)
status: To Do
assignee: []
created_date: '2026-11-02T21:19:00Z'
labels:
  - ux
  - explainability
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-02 asked whether a diskgc tally is explainable (does a per-profile size breakdown explain why we can or can't delete something). Read-only dry run at 40b2d85 over ~ and /tmp with rustup-toolchains || elan-toolchains || pytest-tmp: per-candidate verdicts explain themselves, but there is no per-profile/per-reason byte breakdown, kept groups carry no sizes ('137 kept: future_time'), and the run hit the 10m deadline (exit 124) after ~2,940 of 6,429 candidates. Add to the human summary (and a JSON summary object) one block per profile: found, eligible (count + estimated bytes), would-reclaim/reclaimed, and kept counted and SIZED by reason (sizes where the tally reached the candidate; 'unsized' with the count otherwise, e.g. refused before sizing), plus not-judged-before-the-deadline count. This is the core of survey (TASK-116/38). Contract: JSON additions only; the human format is not a contract. Model + insta tests; docs-vs-binary.
<!-- SECTION:DESCRIPTION:END -->
