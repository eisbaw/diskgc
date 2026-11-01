---
id: TASK-278
title: >-
  Dry run: one entry point for --select and groups (plan.rs dry_run and
  dry_run_grouped), one tier-deadline helper
status: To Do
assignee: []
created_date: '2026-10-31T21:54:00Z'
updated_date: '2026-11-01T05:37:00Z'
labels:
  - refactor
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up of TASK-270 (mped S5): ensure now has one entry point (executor/entry.rs), but the dry run still has two paths (plan.rs dry_run and dry_run_grouped, mutating for_tier.tier) with the same drift risk, and the per-tier deadline is computed twice (bounded_tier_timeout as Duration and inline tier_deadline as MonoTime). Do the same as TASK-270: --select as a group of one, one tier-deadline helper; pin by model tests and the dry-run insta/vector outputs.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Periodic full review #2 on master 794e0e1 (2026-11-01; Codex tmp/reviews/codex-full2-794e0e1.md + mped), static reading, not reproduced. additions: drop Request.tier, a RunPolicy value instead of copying protections/max_entries onto every tier, (Tier, Duration) pairs; one prepare_backend shared by ensure and the dry run; the grouped dry run must exclude a tier on preflight error like entry.rs prepare_tier does (today it returns on the first tier's error) with a model test; one start_common for main.rs ensure_with and watch.rs watch_with; expose tally -> queue -> reclaim stages (helps TASK-251 lazy sizing, survey, TASK-263 Visibility).
<!-- SECTION:NOTES:END -->
