---
id: TASK-94
title: 'Profiles: rustup and elan toolchains (command action, pressure tier)'
status: To Do
assignee: []
created_date: '2026-10-19 11:48'
updated_date: '2026-10-20 02:45'
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

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-20 read-only feasibility audit: host ~/.rustup/toolchains has 1.90.0 (1,695,272,960 B; pinned by topics/zellij/rust-toolchain.toml), nightly-2025-11-21 (2,302,726,144 B), nightly (1,574,916,096 B; rustup default). ~/.elan/toolchains has leanprover/lean4:v4.16.0 (1,337,815,040 B), v4.30.0-rc1 (2,784,309,248 B; pinned by nvidia/ntech/Clift_NDCfw/Clift/lean-toolchain), and lean4-patched symlink (pinned by topics/formal_verification/lean-toolchain). Neither rustup nor elan is on host PATH or the project Nix dev-shell PATH. No profile shipped: command actions only substitute candidate path, but both uninstall CLIs require a toolchain name; current predicates cannot prove default, directory overrides, configured-root pins or tool-created pins, and rustup may uninstall the default with only a warning. Implement an exact tool-specific name binding and fail-closed pin/default guard, rechecked under lock, before enabling either command action. Preserve explicit project-root coverage limits and tool-created pins (for example kani) in docs.
<!-- SECTION:NOTES:END -->
