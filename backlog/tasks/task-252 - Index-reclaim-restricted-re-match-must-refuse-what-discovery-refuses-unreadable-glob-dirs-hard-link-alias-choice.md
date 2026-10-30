---
id: TASK-252
title: >-
  Index reclaim: restricted re-match must refuse what discovery refuses
  (unreadable glob dirs, hard-link alias choice)
status: To Do
assignee: []
created_date: '2026-10-30T22:54:00Z'
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
