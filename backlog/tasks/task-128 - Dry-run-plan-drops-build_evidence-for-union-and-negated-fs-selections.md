---
id: TASK-128
title: Dry-run plan drops build_evidence for union and negated fs selections
status: To Do
assignee: []
created_date: '2026-10-23 22:08'
labels:
  - contract
  - report
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
docs/reference.md (Output) says a source-anchored build candidate whose source was established at discovery has build_evidence {marker, source, idle_age_seconds}. executor::plan_fs sets it; plan_fs_union (used for every fs selection with || or !profile:) never does, so 'profile:meson-build-anchored && !profile:pin' plans the same candidates without build_evidence. Found by TASK-121 vectors builtins/meson-build-anchored.yaml and builtins/west-build-anchored.yaml, which pin the current (wrong) rows without source: and say so; when fixed, add source: to their plan rows. Report-contract only: no deletion decision depends on it.
<!-- SECTION:DESCRIPTION:END -->
