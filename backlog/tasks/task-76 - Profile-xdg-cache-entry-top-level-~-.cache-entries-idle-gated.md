---
id: TASK-76
title: 'Profile: xdg-cache-entry (top-level ~/.cache entries, idle-gated)'
status: Done
assignee: []
created_date: '2026-10-18 20:54'
updated_date: '2026-10-20 01:10'
labels:
  - home
  - wave-4
dependencies:
  - TASK-18
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: idle top-level entries of $XDG_CACHE_HOME (bun 3.6 GiB idle 71 days, microsoft-edge idle 608 days, wine, node-gyp, act, electron, nix-index, zig, ...) held ~5.6 GiB here. The freedesktop cache spec says apps must tolerate losing ~/.cache. Roots required ($XDG_CACHE_HOME), paths *, type any, intended with filter:idle-30d. Risks: some tools keep state or data there (huggingface models, borg's chunk cache, user tools' own dirs). Decide an exclusion list or a protect example, and how it composes with the dedicated cache profiles (overlap). Source: read-only home survey 2026-10-18 (scratchpad home-survey/summary.txt; sizes on the dev laptop at that date).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Profile with examples; the exclusion of known data dirs (huggingface, borg, ...) is decided and documented
- [x] #2 A read-only dry run here with idle-30d lists eligible entries with sizes matching du
- [x] #3 Overlap with dedicated cache profiles (pip-cache, nix-user-cache, ...) is defined
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-80 (checkout setting): ~/.cache/pre-commit holds git clones of hook repositories, so an xdg-cache-entry candidate holding it is contains_checkout under the default (checkouts: refuse). Decide this profile's checkouts setting explicitly (and record why), or accept that such entries are kept.

Implementation in isolated worktree /tmp/diskgc-profile-xdg: adds declarative find.exclude for fs sources; xdg-cache-entry uses required XDG_CACHE_HOME root, paths [*], type any, explicit checkouts refuse, and literal exclusions for known state/data and dedicated cache directories. Core/model golden tests and docs added. nix develop -c just check passed on 2026-10-20 before a final small overlap assertion; host dry run and du comparison deferred while concurrent E2E timing gate runs. Do not mark AC2 complete until the dry run is reviewed.

2026-10-20 integrated built-in dry run on this laptop, read-only: ensure --dry-run --json --min-free 500G --select profile:xdg-cache-entry --roots /home/mpedersen/.cache /home/mpedersen/.cache. Status dry_run, 103 candidates, 63 eligible (1,199,771,648 estimated bytes), 33 not_idle, 4 contains_checkout, 3 contains_ipc; excluded names do not become candidates. The six largest extant eligible candidates matched du -s -B1 byte for byte. Built-in now enforces min_idle_for: 30d even without a tier filter, and verified_with names XDG Base Directory Specification 0.8. The exact exclusion list and dedicated-cache overlap policy are in docs/reference.md; golden and model tests pin them. Kernel exclusion test and final integrated gates remain pending; task stays To Do.

Focused real-kernel container test fs_reclaim_exact_exclusion_keeps_valuable_cache passed: an excluded cache holding valuable data remained untouched, and an eligible sibling was deleted under the oracle. Nested positive path under an excluded intermediate is covered by the exact_exclusion_still_traverses_a_deeper_positive_pattern model test. A present null or blank find.exclude is rejected by the parser. Full just check/e2e integration remains pending.

Final integration on 2026-10-20: post-fix nix develop -c just e2e passed 93/93, including real-kernel exact-exclusion deletion oracle and model excluded-mount regression; QA and MPED reviews GO. This task covers an opt-in 30d idle cache profile, not a guarantee that every unknown app stores only rebuildable data; docs require local plan/protect review.
<!-- SECTION:NOTES:END -->
