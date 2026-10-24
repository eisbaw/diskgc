---
id: TASK-28
title: 'Index: units, buckets, identity, path trie, memory budget, queue derivation'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-24 07:45'
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

Forward-carried from TASK-64 (2026-10-23): a flock (or a bound Unix socket) is released when the LAST reference to its open file goes, which can be after close()/exit returned: any /proc/<pid>/fd reader (an in-use scan, diskgc's own included, lsof) holds a transient reference for the length of its syscall (measured up to 0.5 ms on 7.2; about 1 in 60 immediate retries busy under four readers). Never assert a release with one attempt right after a close; wait with a bound (RELEASE_BOUND, docs/testing.md Negative feedback). A held-lock assertion needs no wait.

Correction to the TASK-64 forward-carry above (2026-10-23, measured): which /proc reads hold a file reference depends on the kernel (fs/proc/fd.c): reading /proc/<pid>/fdinfo on every supported kernel, statting or reading an fd link from 5.11, listing the fd directory from 6.7. diskgc's scan lists and stats fd links, so it can delay another program's lock release only from 5.11 on; on 5.8.16 those reads gave no late release in about 1.7 million attempts. A test still waits with RELEASE_BOUND on every kernel (other readers, e.g. lsof, read fdinfo). docs/safety.md Host lock has the table.

Orchestrator 2026-10-24 (COMPASS round 2): next cornerstone after the 0.1 batch. Bucketing matters now: nix-shell-rc would otherwise print ~105k plan rows. Index the ModelHost state first or the 98k rc-dir model test is quadratic. TASK-117 (watch design) runs alongside.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
