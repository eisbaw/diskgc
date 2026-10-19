---
id: TASK-77
title: 'Profiles: go-mod-cache and bun-cache (plain delete; the tools are not on PATH)'
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
Why: ~/go/pkg/mod 2.6 GiB (read-only tree: delete needs make_writable) and ~/.cache/.bun/install/cache 3.6 GiB (no hardlinks into node_modules, so the size is really freed). go and bun are not on PATH outside nix dev shells here, so the tools' own cleaners (go clean -modcache) cannot run from a diskgc service: use plain delete. Builds re-download and fail offline. Source: read-only home survey 2026-10-18 (scratchpad home-survey/summary.txt; sizes on the dev laptop at that date).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Both profiles with examples; go-mod-cache deletes a read-only tree via make_writable (model test)
- [x] #2 A read-only dry run here matches du
- [x] #3 Docs say why plain delete, not the tool command, on Nix machines
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-20 implementation checkpoint: embedded go-mod-cache (Go 1.26.7) and bun-cache (Bun 1.3.3), each with examples, golden/model tests, verified_with. Real-host read-only dry runs: Go 1/1 eligible, 2,786,992,128 bytes; Bun 2/2 eligible, 3,833,298,944 bytes; no skips, both matching du. Model deletion shows go-mod-cache make_writable enables removal of read-only module directories. Four distinguishing pattern mutations made golden tests red; restored. Integrated just check passed; full e2e/reviews pending.

Final gate 2026-10-20: nix develop -c just check passed; nix develop -c just e2e passed 90/90 including built-in go-mod-cache deleting a nested 0555 tree through ensure with zero oracle violations; independent QA and MPED reviews GO.
<!-- SECTION:NOTES:END -->
