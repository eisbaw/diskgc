---
id: TASK-49
title: 'Release review and final hardening: v1 freeze'
status: To Do
assignee: []
created_date: '2026-10-16 22:41'
updated_date: '2026-10-16 22:55'
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

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
