---
id: TASK-243
title: 'Reclaim walk on the index for Cargo-target, union and !profile: tiers'
status: To Do
assignee: []
created_date: '2026-10-26 08:05'
labels:
  - wave-3
  - architecture
dependencies:
  - TASK-31
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-31 wired the index into ensure for fs tiers of one source without !profile: only (executor/indexed.rs takes_index). Cargo-target tiers, filesystem unions and tiers with !profile: are not tallied by the synchronous tally and discover per tier as before the index (correct, only slower). Extend re-match/expansion (cargo_target discovery restricted to a path, as fs_kind::discover_beneath) and the union judgement to take candidates from the index, with the index-authority property extended to them.
<!-- SECTION:DESCRIPTION:END -->
