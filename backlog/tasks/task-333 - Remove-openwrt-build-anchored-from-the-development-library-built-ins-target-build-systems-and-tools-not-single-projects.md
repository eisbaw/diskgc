---
id: TASK-333
title: >-
  Remove openwrt-build-anchored from the development library (built-ins target
  build systems and tools, not single projects)
status: Done
assignee: []
created_date: '2026-11-03T22:52:00Z'
updated_date: '2026-11-03T23:18:00Z'
labels:
  - profiles
  - scope
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Mark 2026-11-03: 'i find openwrt too specific -- the others are build-systems so that is OK, but we wont cater to every single opensource project in existance, like openwrt aosp kernel etc'. Remove openwrt-build-anchored (TASK-101) from builtin.yaml, its frozen definition/golden set, vectors, the reference.md library and liveness rows, safety.md/testing.md mentions, and the release suite's excluded list; keep buildroot-output-anchored, yocto-tmpdir-anchored, bazel-output-base-anchored and the shared build_trees.rs helpers they use (drop OpenWrt-only code). State the scope rule once in reference.md's library intro (single home): built-ins key on build-system or tool markers; project-specific trees are user-written profiles. Never shipped in any catalog, so the name is not frozen.
<!-- SECTION:DESCRIPTION:END -->
