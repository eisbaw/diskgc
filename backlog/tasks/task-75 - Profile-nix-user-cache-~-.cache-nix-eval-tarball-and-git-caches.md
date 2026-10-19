---
id: TASK-75
title: 'Profile: nix-user-cache (~/.cache/nix eval, tarball and git caches)'
status: Done
assignee: []
created_date: '2026-10-18 20:54'
updated_date: '2026-10-19 22:57'
labels:
  - home
  - wave-4
dependencies:
  - TASK-18
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: ~/.cache/nix/{gitv3, tarball-cache, tarball-cache-v2, eval-cache-v*} held 4.1 GiB here, fully regenerable (needs the network). A running nix keeps the sqlite files open and maps the git packs, so the in-use check covers it. Two tarball-cache versions were both being written (two Nix versions on the host). Plain delete of those named entries, idle-gated. Source: read-only home survey 2026-10-18 (scratchpad home-survey/summary.txt; sizes on the dev laptop at that date).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Profile with examples; a read-only dry run here lists the entries with sizes matching du
- [x] #2 A nix evaluation running during the dry run makes its cache in_use (container, or read-only on a real machine)
- [x] #3 Never matches ~/.cache/nix itself or anything outside the named entries
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-20 implementation checkpoint: embedded nix-user-cache with examples, golden/model tests, verified_with Nix 2.34.8. Real-host read-only dry run: 5 candidates, 5 eligible, 0 skips, 4,457,385,984 bytes, matching du. Real Nix 2.34.9 pure evaluator container test held its eval-cache SQLite fd; diskgc dry run marked that exact candidate in_use, zero oracle changes. First attempt used --impure, which disables Nix eval-cache; corrected pure evaluation passed. Distinguishing pattern mutation made golden test red. Integrated just check passed; full e2e/reviews pending.

Final gate 2026-10-20: nix develop -c just check passed; nix develop -c just e2e passed 90/90 including pure Nix evaluator in-use case; independent QA and MPED reviews GO.
<!-- SECTION:NOTES:END -->
