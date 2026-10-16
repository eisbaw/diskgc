---
id: TASK-1
title: 'Scaffold: Nix flake, Cargo workspace, justfile, static musl build'
status: To Do
assignee: []
created_date: '2026-10-16 22:37'
updated_date: '2026-10-16 22:49'
labels:
  - wave-1
  - foundation
dependencies: []
references:
  - PRD.md
  - docs/architecture.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: everything else builds on this. Create the Nix flake (crane, rust-overlay pinned by rust-toolchain.toml, statically linked musl package), the Cargo workspace with the crates named in docs/architecture.md (diskgc-core, diskgc-linux, diskgc-model, diskgc-modelgen, diskgc; diskgc-e2e as a separate project outside the workspace), and a justfile with the recipes in docs/testing.md (check, e2e, e2e-one, soak, ci) wired to the dev shell.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 nix build produces a statically linked diskgc binary (verified with file/ldd) that runs on a non-Nix system image
- [ ] #2 just check runs fmt, clippy (warnings are errors) and the (empty) test suites green in the dev shell
- [ ] #3 cargo test --workspace does not compile diskgc-e2e
- [ ] #4 Editing a file under docs/ does not trigger a rebuild (crane source filtering)
- [ ] #5 The dev profile uses debug = "line-tables-only" with dependency debug info off
- [ ] #6 The musl target is set only through crane's build target (not .cargo/config.toml), so cargo-fuzz on the gnu target still works
- [ ] #7 diskgc-e2e has its own clippy check in the flake, although it is outside the workspace
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
