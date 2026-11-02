---
id: TASK-94
title: 'Profiles: rustup and elan toolchains (command action, pressure tier)'
status: In Progress
assignee: []
created_date: 2026-10-19 11:48
updated_date: '2026-11-02T15:24:00Z'
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

2026-10-20 TASK94 safety audit: No profile shipped. Neither rustup nor elan is on the host or project Nix dev-shell PATH. Existing command actions substitute only candidate path, while uninstall requires the exact manager toolchain identifier; Elan identifiers can include origin/channel (leanprover/lean4:v4.16.0) and linked toolchains can be symlinks. Rustup documents that its settings.toml schema is private, so default and directory overrides require manager CLI queries; pins also come from rust-toolchain and rust-toolchain.toml under every configured project root, plus path-based references. Elan GC is experimental and only tracks project pins for directories Elan previously visited; it cannot certify all configured roots. A safe profile needs a toolchain-specific source and exact name binding, complete fail-closed pin/default checks and locked recheck, plus pinned offline manager fixtures proving uninstall and preservation. AC2 requires a real-machine dry run, which the current user instruction forbids; do not mark Done or ship until that conflict is resolved or AC2 is revised.

2026-10-20 isolated offline container counterexamples (Nix-packaged Elan 4.2.1, Rustup 1.29.0; diskgc-e2e-base image, fresh /tmp homes, --network none, /nix/store read-only): Elan recognizes an installed leanprover/lean4:v4.1.0 directory while an unvisited project has lean-toolchain pointing to it, but gc --json classifies it unused and gc --delete uninstalls it. Rustup accepts toolchain uninstall custom while custom is default: warns and removes it. Thus manager GC/uninstall exit success does not preserve project pins/defaults. No profile shipped. Need manager-name-to-object binding, complete pin/default/override inventory and explicit per-name authorization, lock-time recheck, and disposable fixture that exercises these properties. AC2 real-machine dry run remains barred by the user no-host-testing instruction. See docs/safety.md toolchain subsection.

2026-11-02: Mark steer: profiles + MockFS first. In progress in tmp/worktrees/t94 (branch t94-toolchains).
<!-- SECTION:NOTES:END -->
