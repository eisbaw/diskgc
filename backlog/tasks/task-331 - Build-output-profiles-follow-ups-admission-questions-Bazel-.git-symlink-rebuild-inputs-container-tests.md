---
id: TASK-331
title: >-
  Build-output profiles follow-ups: admission questions, Bazel .git symlink,
  rebuild inputs, container tests
status: To Do
assignee: []
created_date: '2026-11-03T20:16:00Z'
labels:
  - profiles
  - needs-mark
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-101 (merged 2026-11-03, dev library). (1) Mark: user edits inside build trees are lost (OpenWrt QUILT=1 work, Buildroot output/build/<pkg> edits and *-menuconfig results, Yocto tmp/work); Yocto's fetcher leaves .git clones in tmp/work, so most real TMPDIRs are refused as checkouts. (2) A .git symlink in a Bazel execroot may trip the checkout guard and refuse every output base: check on a real git workspace. (3) Admission test gaps: a user profile pinning the build_dir/output name check; a Bazel workspace on another filesystem; a TMPDIR with no abi_version. (4) Hardening: Bazel output base name == md5(workspace); judge OpenWrt build_dir and staging_dir idle together; bound reads outside the root (Bazel workspace, br2-externals, layers). (5) Unchecked rebuild inputs: Buildroot overlay/patch paths in .config; Yocto DL_DIR/SSTATE_DIR under TMPDIR. (6) Fold the CMake/Meson/Autotools anchors onto the new read helpers. (7) Container tests with real Buildroot/Yocto/Bazel; AC2 OpenWrt size vs du once a tree is owned by the user; AC3 with survey.
<!-- SECTION:DESCRIPTION:END -->
