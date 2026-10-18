---
id: TASK-49
title: 'Release review and final hardening: v1 freeze'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-17 23:56'
labels:
  - wave-5
  - review
  - hardening
  - irreversible
dependencies:
  - TASK-48
references:
  - PRD.md
  - docs/testing.md
priority: high
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: v1 freezes the CLI, JSON and YAML contracts for users. Final deep review (mped-architect, safety, CLI/UX), full mutation run, soak property tests, release notes; confirm the PRD's What good / What bad lists each map to passing tests.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every bullet of the PRD's What good and What bad lists names its passing test(s)
- [ ] #2 Full cargo-mutants run: no surviving mutants in safety-critical modules, or each justified
- [ ] #3 Release notes list honest limits as stated in PRD.md
- [ ] #4 The sandbox gate is removed only by the release flag in the v1 package; development builds still contain it (test)
- [ ] #5 The provisional schema parts (command actions, Docker, Nix) are frozen or changed after review
- [ ] #6 Built-in profile names shipped in wave 4 are reviewed and frozen (their meaning never changes under a name)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From task-50: the v1 release package, and only it, enables the cargo feature release-without-sandbox-gate of diskgc-linux, set in one place (e.g. the diskgc package's cargoExtraArgs in flake.nix). Update the test sandbox_gate_flag_is_enabled_nowhere (crates/diskgc-linux/src/sandbox.rs) to allow exactly that line. The e2e binaries, the test driver and every check must stay gated. Remove the 'Until v1 is released' bullet from PRD Safety boundaries and update docs/safety.md, Pre-release sandbox gate. Today the shipped binary links no deleting code, so nothing checks the binary itself. Once it does, consider a flake check that the non-release package contains the marker path /.diskgc-test-sandbox (sandbox::check linked in) and the release package does not.

0.1 comes first (2026-10-18): task-67 releases the ensure MVP and is the first package built with release-without-sandbox-gate; it sets the flag in one place and updates sandbox_gate_flag_is_enabled_nowhere. This task builds on 0.1: AC #4 then means the v1 package keeps that single flag site, and the 'Until v1 is released' text in the implementation note above is superseded by the PRD and safety.md changes for 0.1.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
