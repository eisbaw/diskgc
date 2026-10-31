---
id: TASK-252
title: >-
  Index reclaim: restricted re-match must refuse what discovery refuses
  (unreadable glob dirs, hard-link alias choice)
status: Done
assignee: []
created_date: '2026-10-30T22:54:00Z'
updated_date: '2026-10-31T02:09:00Z'
labels:
  - irreversible
  - safety
  - index
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Codex cross-model review of TASK-31 at 82cb9af (2026-10-30, tmp/reviews/codex-t31-82cb9af.md), each single-event, reproduced by Codex with temporary model probes; not yet reproduced by us. Master only; 0.1 has no index.

(1) fs_kind/discover.rs Walk::names: with paths ['*/*'], indexed /tmp/p/f, then the user chmods /tmp/p to 0333. Discovery refuses the listing as unreadable; the restricted lookup by name traverses (x without r) and deletes f. Outside the two documented AC1 exceptions. Fix direction: the restricted walk refuses a glob directory discovery could not list, keeping discovery's unreadable refusal.

(2) executor/indexed.rs Reclaim::rematch: indexed /tmp/z/f; /tmp/a holds .diskgc-keep; after the tally a hard link /tmp/a/f -> z/f is created. Full discovery meets a/f first, deduplicates z/f by inode and refuses the kept candidate; the restricted re-match sees only z/f and deletes it. Fix direction: a multiply linked file is not re-matched from the index; leave it to the tier's discovery (block expansion too). Decide first whether discovery's alias rule is a safety rule (cf. union_checkout_marker_uses_every_admitted_refusing_hardlink_alias).

AC: each case a failing model test first (red on 82cb9af), green after; the index_authority property generator extended to produce unreadable glob dirs and hard-link aliases.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Alias rule: discovery's found-once dedup picks which name of a multiply linked file is reported and judged; it is not a safety rule (a keep marker keeps the names on its path; removing another name leaves the kept name and its data; with the unkept name sorted first discovery itself removes it). Parity fix anyway (index authority). Fix 1: discover_beneath opens each glob directory on its way as discovery's listing would (Host::listable) and refuses what discovery cannot list. Fix 2: a restricted walk sets multiply linked files (nlink != 1 or unreported) aside (Discovery::aliased); rematch and block expansion leave them to the tier's discovery; a block of only such members is not evicted. Found by the extended property: a refusal of this run's tally now applies only to a unit re-matched at its path (rematch order). Follow-ups for the orchestrator to file: (a) tally/size.rs sizing-walk refusal reason differs from the eligibility walk's first refusal (unreadable vs not_owner); index_authority.rs narrows the reason comparison for tally-refused skips until then. (b) cross-root aliases (a bind-mounted second root): a restricted walk skips roots beside its path, so a directory reached through two roots is re-matched under the root the unit names, not the outermost; not addressed here.

2026-10-31: merged (ff) at 265989d after one review round (mped + Codex). Follow-ups: TASK-261 (sizing refusal reason), TASK-260 (protect/bind bypass, pre-existing). No container run (disk).
<!-- SECTION:NOTES:END -->
