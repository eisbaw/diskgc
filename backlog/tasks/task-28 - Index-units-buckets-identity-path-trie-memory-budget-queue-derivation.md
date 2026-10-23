---
id: TASK-28
title: 'Index: units, buckets, identity, path trie, memory budget, queue derivation'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-23 16:19'
labels:
  - wave-3
  - architecture
dependencies:
  - TASK-26
references:
  - docs/architecture.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: freeing space must not wait for a survey. Implement the index per docs/architecture.md#the-index: single and bucket units, identity (mount, inode, birth time with fallback), profile sets per path, per-mount trie, 64 MB budget with coverage partial, queue derivation with oldest and best-fit orders.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Hitting the memory budget sets coverage: partial and the walk still finds the missing candidates via per-tier discovery
- [ ] #2 best-fit picks the smallest covering unit, else the largest (unit tests)
- [ ] #3 98,000 tiny rc dirs fit in at most 10 bucket units (model test)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-14: ModelHost recomputes derived values (reachability for nlink and space, path_of for mount points and messages) from scratch, so each stat or lookup is linear in the filesystem's size. AC #3's 98,000-dir model test would be quadratic: add indexes to diskgc-model state.rs (e.g. a parent map and cached reachability invalidated on change) before writing it, and measure.

From task-68 (mped S5): the dry run of tmp-entry over this laptop's /tmp (~155,000 top-level entries; stopped by the 20 min deadline at 79,484) printed 79,484 plan entries, 14.6 MB of JSON on stdout. Bucketing (reference.md Output: "a bucket is one plan entry with a member count") is needed for /tmp-scale plans; nix-shell-rc alone has ~105k members of ~3 KiB each.

Orchestrator 2026-10-23 (COMPASS consult): dependency narrowed from TASK-27 (fuzz/doc hardening wave) to TASK-26 (schema freeze); the index cornerstone must not wait on hardening.

Orchestrator 2026-10-23 (design audit): design gaps to settle in docs/architecture.md as part of this task (PRD marks index internals tentative, so decide and document rather than pre-design): bucket split/merge when an estimate crosses 64 MiB; definition of bucket 'name family' (currently only under survey); eviction of vanished units; what is dropped at the memory budget beyond 'stops adding'; how best-fit expands a bucket into members; index update after an action.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
