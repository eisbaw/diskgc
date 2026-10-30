---
id: TASK-246
title: >-
  Index updates after skips: missing eviction, refusal hints and checked_at from
  the reclaim walk
status: To Do
assignee: []
created_date: '2026-10-26 08:05'
labels:
  - wave-3
dependencies:
  - TASK-31
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
architecture.md Refusals and index updates: a skip updates checked_at and the refusal hint; stale and missing evict; a part-way deletion marks for re-tally. TASK-31 implements eviction on completed deletion, block member removal and stale eviction only (executor/indexed.rs). The rest matters once the index outlives the run (TASK-30 snapshot, TASK-32 watch live index).
<!-- SECTION:DESCRIPTION:END -->
