---
id: TASK-374
title: >-
  Space wait hardening: exact-goal and deadline-during-wait tests, per-deletion
  lag in the model
status: To Do
assignee: []
created_date: '2026-11-10T17:46:00Z'
labels:
  - hardening
dependencies: []
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-334 step 0 (6d2a3572) reviews: QA mutants for a goal met exactly and for a deadline during a wait survive the committed tests; split 'deadline hit' out of Reached in resample_while; ModelHost free_lag is per mount and releases in deletion order, so out-of-order release is untestable; the continue-after-signal-at-tier-boundary mutant survives. Also open from the profile: ~115 ms per nix-shell-rc candidate outside the actions is unexplained (tmp/t334/).
<!-- SECTION:DESCRIPTION:END -->
