---
id: TASK-75
title: 'Profile: nix-user-cache (~/.cache/nix eval, tarball and git caches)'
status: To Do
assignee: []
created_date: '2026-10-18 20:54'
updated_date: '2026-10-18 23:14'
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
- [ ] #1 Profile with examples; a read-only dry run here lists the entries with sizes matching du
- [ ] #2 A nix evaluation running during the dry run makes its cache in_use (container, or read-only on a real machine)
- [ ] #3 Never matches ~/.cache/nix itself or anything outside the named entries
<!-- AC:END -->
