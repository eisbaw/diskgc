---
id: TASK-94
title: 'Profiles: rustup and elan toolchains (command action, pressure tier)'
status: To Do
assignee: []
created_date: '2026-10-19 11:48'
labels:
  - home
dependencies:
  - TASK-37
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: ~/.rustup/toolchains (5.2 GiB) and ~/.elan/toolchains (3.8 GiB) on the dev laptop (home survey 2026-10-18). Toolchains are pinned by project files diskgc cannot see in general (rust-toolchain.toml, lean-toolchain) and by tools (kani pins a nightly); the default toolchain must never go. Use the tool's own uninstall (rustup toolchain uninstall, elan toolchain gc/uninstall), only for toolchains idle for long, intended for pressure tiers. The tools may not be on PATH outside dev shells (Nix machines): the profile must say what it needs.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Profiles with examples; the default toolchain and toolchains pinned by rust-toolchain.toml/lean-toolchain under the configured roots are kept (model test)
- [ ] #2 A dry run on a real machine lists candidates with sizes matching du
- [ ] #3 Docs state the pinning limits honestly (pins outside the roots, tool pins like kani)
<!-- AC:END -->
