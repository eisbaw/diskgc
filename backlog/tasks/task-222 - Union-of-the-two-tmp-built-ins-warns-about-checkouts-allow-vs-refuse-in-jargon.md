---
id: TASK-222
title: >-
  Union of the two /tmp built-ins warns about checkouts: allow vs refuse in
  jargon
status: Done
assignee: []
created_date: '2026-10-25 13:05'
updated_date: '2026-10-25 14:27'
labels:
  - shipping
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
rc-walk Debian walk: ensure --select 'profile:pytest-tmp || profile:nix-shell-rc' /tmp warns on every run: profile `pytest-tmp` says `checkouts: allow`; in a union, a candidate also admitted by any refusing source uses `checkouts: refuse`. The two /tmp built-ins cannot match the same candidate, and a new user combining the shipped profiles cannot act on it. Warn only when the union can actually change a verdict, or say it in user terms. Dump: tmp/walks/2026-10-25-rc-walk-debian/dumps/15-ensure-config-tmp.txt.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-25 polish (branch polish 7abae1b (not merged)): the per-run union warning is removed; when a union actually refuses a candidate an allow source admitted, the skip detail appends 'X allows checkouts, but Y also selected this candidate and refuses them' (plan and reclaim; tested in fs_reclaim union_checkout_policy_uses_only_terms_admitted_by_their_filters). Walk: tmp/walks/2026-10-25-polish/dumps/b14b-dry-select-tmp-idle.txt -> tmp/walks/2026-10-25-polish/dumps/a14b-dry-select-tmp-idle.txt (no warning).

Merged to master as 72fccd8 on 2026-10-25T14:27Z.
<!-- SECTION:NOTES:END -->
