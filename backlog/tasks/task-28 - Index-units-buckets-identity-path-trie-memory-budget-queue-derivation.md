---
id: TASK-28
title: 'Index: units, buckets, identity, path trie, memory budget, queue derivation'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-25 14:18'
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
- [ ] #1 best-fit picks the smallest covering unit, else the largest (unit tests)
- [ ] #2 At the memory ceiling the index collapses units into blocks (re-validating site and members first, evicting what vanished) and never exceeds the budget; coverage is never partial because of memory, and every block member is still found and checked at reclaim (model: collapse_keeps_every_member_checked, collapse_revalidates_members)
- [ ] #3 98,000 tiny rc dirs with a model budget that fits a tenth of them stay within the budget after every insertion and collapse, and repeated collapse ends (model: index_never_exceeds_its_budget)
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

Mark's decisions 2026-10-25 (his own, not the emulator): (a) Memory: finite size, but when the ceiling is met the index COLLAPSES existing entries into coarser blocks instead of stopping (coarser = more stat work at delete time); a collapse must first re-validate that the collapsed elements' files/inodes are still valid. Replaces architecture.md's 'stops adding units, coverage: partial'. (b) No latency targets: drop the PRD's '10 s to first deletion' acceptance. (c) Start now, in parallel with the 0.1 release. Index questions not answered by the docs go to Mark directly.

Design settled 2026-10-25 in fe54fe6 (branch index-design, not merged): docs/architecture.md The index / Memory budget and collapse / Order within a tier / Refusals and index updates / The tally / reclaim walk; safety.md Discovery; testing.md index scenarios. AC review needed: #1 (coverage partial no longer comes from the memory budget; collapse instead) and #3 (no eager buckets; becomes index_never_exceeds_its_budget over 98,000 rc dirs with a small budget).

AC rewrite: design settled in fe54fe6 + 076c5ec (branch index-design); Mark's decisions 2026-10-25. Old #1 (budget sets coverage partial) and #3 (98,000 rc dirs in at most 10 bucket units) replaced: no eager buckets; collapse at the ceiling.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
