---
id: TASK-367
title: >-
  cmake/meson anchored profiles match package builds inside another build
  system's tree (OpenWrt build_dir/*/openwrt-build, Buildroot
  output/build/<pkg>/buildroot-build)
status: In Progress
assignee: []
created_date: '2026-11-07T15:33:00Z'
updated_date: '2026-11-07T17:30:00Z'
labels:
  - profiles
  - admission
  - '0.2'
dependencies: []
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-354 finding 1 (2026-11-07): cmake-build-anchored and meson-build-anchored match per-package build dirs that OpenWrt and Buildroot create inside their own trees; after deletion that build system still believes the package is built, so its next build fails until reconfigured (from their makefiles, not reproduced). Pending Mark's decision; cheap guard proposed: refuse when the candidate's parent holds .stamp_* or .built files (the outer build system's markers). Blocks admitting cmake/meson to 0.2.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mark 2026-11-07 chose option 1: a GENERAL nested-build guard -- cmake/meson never take a build dir inside another build system's managed tree, detected by (a) stamp files in the parent (.stamp_* / .built / .configured* / .prepared*, verified against real trees) and (b) nesting under a build root diskgc already recognises (Buildroot output/, Yocto TMPDIR, ...); then admit cmake/meson. Discussed as: meta-build systems (Buildroot, OpenWrt, Yocto, dpkg-buildpackage, Spack, conda-build) build each upstream package with its own build system inside a tree they manage and track progress with stamps; deleting the inner build dir leaves the outer stamp saying 'built'.
<!-- SECTION:NOTES:END -->
