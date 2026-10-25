---
id: TASK-234
title: >-
  Nightly-only Cargo container tests pass silently if the nightly toolchain is
  dropped from the manifest
status: To Do
assignee: []
created_date: '2026-10-25 21:01'
labels:
  - test
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-205 review (2026-10-25): cargo_reclaim_keeps_a_fine_grain_locked_target and cargo_race_split_build_dir_is_never_reclaimed only log 'not applicable' on stable toolchains; if the pinned nightly is ever removed from the toolchain manifest they pass without testing anything. Make the full tier fail when no toolchain exercises them. Also: the nightly variants run only in the full tier, not in just e2e.
<!-- SECTION:DESCRIPTION:END -->
