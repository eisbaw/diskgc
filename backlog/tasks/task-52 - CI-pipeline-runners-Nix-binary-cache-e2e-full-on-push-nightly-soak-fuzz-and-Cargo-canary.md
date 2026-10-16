---
id: TASK-52
title: >-
  CI pipeline: runners, Nix binary cache, e2e-full on push, nightly soak, fuzz
  and Cargo canary
status: To Do
assignee: []
created_date: '2026-10-16 22:47'
labels:
  - wave-1
  - tooling
dependencies:
  - TASK-2
references:
  - docs/testing.md#commands
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Why: docs/testing.md relies on CI for e2e-full, nightly soak and fuzz, the outside-container interlock canary and a latest-stable Cargo canary. Choose the CI provider and runner setup (rootless Podman with subuid/newuidmap, or a privileged runner for nested containers), a Nix binary cache, and the scheduled jobs; record the decision in docs/architecture.md#deployment.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 just e2e-full runs on every push and is green on the scaffold
- [ ] #2 A nightly job runs soak, fuzz and the Cargo race tests against the latest stable Cargo (canary), reporting failures without blocking pushes
- [ ] #3 The interlock canary (a do-nothing binary) is run outside a container in CI and must refuse
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
