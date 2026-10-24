---
id: TASK-132
title: >-
  Harness: keep Podman temp files of shifted-uidmap tests under repo tmp/;
  re-enable liveness proptest persistence
status: To Do
assignee: []
created_date: '2026-10-24 08:24'
labels:
  - hardening
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-112 step 4 (orchestrator review, 2026-10-24). (1) The three --pid=host / shifted --uidmap container tests unset TMPDIR because Podman under the shifted map cannot traverse the dev shell TMPDIR inside the 0700 home; Podman temp files then land outside the repo tmp/, contrary to CLAUDE.md (keep host test temp files beneath tmp/). Find a root fix (e.g. a dedicated repo tmp/ subdir with a traversable path, or podman --tmpdir / runroot settings) rather than unsetting. (2) Proptest regression persistence was turned off for the liveness property tests to avoid committing seeds produced by mutated code during a manual mutation sweep; restore persistence and instead keep mutation sweeps from writing seeds (e.g. PROPTEST_DISABLE_FAILURE_PERSISTENCE only in the sweep script).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Shifted-uidmap container tests keep Podman temp files under the repo tmp/ (documented)
- [ ] #2 Liveness property tests persist regressions again; mutation sweeps do not write seeds
<!-- AC:END -->
