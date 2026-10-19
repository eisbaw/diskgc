---
id: TASK-76
title: 'Profile: xdg-cache-entry (top-level ~/.cache entries, idle-gated)'
status: To Do
assignee: []
created_date: '2026-10-18 20:54'
updated_date: '2026-10-18 23:18'
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
- [ ] #1 Profile with examples; the exclusion of known data dirs (huggingface, borg, ...) is decided and documented
- [ ] #2 A read-only dry run here with idle-30d lists eligible entries with sizes matching du
- [ ] #3 Overlap with dedicated cache profiles (pip-cache, nix-user-cache, ...) is defined
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-80 (checkout setting): ~/.cache/pre-commit holds git clones of hook repositories, so an xdg-cache-entry candidate holding it is contains_checkout under the default (checkouts: refuse). Decide this profile's checkouts setting explicitly (and record why), or accept that such entries are kept.
<!-- SECTION:NOTES:END -->
