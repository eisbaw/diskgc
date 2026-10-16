---
id: TASK-27
title: 'Hardening wave 2: fuzzing, mutation testing, doc validation, deferred findings'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 23:16'
labels:
  - wave-2
  - hardening
dependencies:
  - TASK-26
references:
  - docs/testing.md
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: wave-2 surfaces are frozen; harden them once. cargo-fuzz on the grammar parser and the YAML loader, cargo-mutants on selection and executor, doc validation of docs/reference.md and docs/safety.md, every finding deferred during wave 2.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No surviving mutants in selection semantics, or each justified
- [ ] #2 Each fuzzer (grammar, YAML loader, mountinfo, maps, environ parsers) runs 30 minutes without crashes; crashes found become regression tests
- [ ] #3 fuzz and mutants recipes exist in the justfile
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Forward-carried from task-1: The musl target is set only in flake.nix (CARGO_BUILD_TARGET in the crane args). There is no .cargo/config.toml, and the dev shell builds for gnu. cargo-fuzz was not run in task-1. It needs a nightly toolchain, so add a separate dev shell (e.g. devShells.fuzz from a dated rust-overlay nightly) instead of changing rust-toolchain.toml. 'just fuzz' is a placeholder (prints NOT RUN, exits 0).

Forward-carried from task-1 review (mped): doc validation needs docs/ as a build input, which conflicts with crane's source filter that keeps docs out (task-1 AC4); run doc checks outside the package build (a separate check or recipe) so docs edits still do not rebuild the binary.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
