---
id: TASK-77
title: 'Profiles: go-mod-cache and bun-cache (plain delete; the tools are not on PATH)'
status: To Do
assignee: []
created_date: '2026-10-18 20:54'
updated_date: '2026-10-18 23:15'
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
- [ ] #1 Both profiles with examples; go-mod-cache deletes a read-only tree via make_writable (model test)
- [ ] #2 A read-only dry run here matches du
- [ ] #3 Docs say why plain delete, not the tool command, on Nix machines
<!-- AC:END -->
