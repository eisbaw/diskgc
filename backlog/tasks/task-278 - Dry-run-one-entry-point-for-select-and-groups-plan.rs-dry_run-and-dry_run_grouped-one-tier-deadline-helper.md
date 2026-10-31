---
id: TASK-278
title: >-
  Dry run: one entry point for --select and groups (plan.rs dry_run and
  dry_run_grouped), one tier-deadline helper
status: To Do
assignee: []
created_date: '2026-10-31T21:54:00Z'
labels:
  - refactor
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up of TASK-270 (mped S5): ensure now has one entry point (executor/entry.rs), but the dry run still has two paths (plan.rs dry_run and dry_run_grouped, mutating for_tier.tier) with the same drift risk, and the per-tier deadline is computed twice (bounded_tier_timeout as Duration and inline tier_deadline as MonoTime). Do the same as TASK-270: --select as a group of one, one tier-deadline helper; pin by model tests and the dry-run insta/vector outputs.
<!-- SECTION:DESCRIPTION:END -->
