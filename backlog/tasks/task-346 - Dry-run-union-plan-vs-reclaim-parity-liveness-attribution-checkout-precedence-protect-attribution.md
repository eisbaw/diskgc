---
id: TASK-346
title: >-
  Dry-run union plan vs reclaim parity: liveness attribution, checkout
  precedence, protect attribution
status: To Do
assignee: []
created_date: '2026-11-05T01:23:00Z'
labels:
  - dry-run
  - union
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Codex review of TASK-337 (2026-11-05), pre-existing, confirmed with model probes; planning-only (reclaim deletion unaffected), but the dry run must predict the reclaim: (1) executor/plan.rs plan_fs_union: A fails a tag filter, B admits but its nix-shell owner lives: plan reports filtered/A, reclaim in_use/B; fix: separate term admission from candidate-wide contract vetoes, keep the vetoing source. (2) checkout precedence: A allows checkouts, B refuses, a shared checkout has an open file: plan stops at in_use/A, reclaim refuses contains_checkout/B before scanning (delete and command unions); fix: finish admission and checkout policy before the in-use check. (3) executor/plan.rs dry_run_inner: A fails its filter, B admits, protect covers it: plan reports B protected, reclaim's early protection screen attributes A; fix: one protection screen and attribution in both paths. Model tests comparing plan and reclaim for each.
<!-- SECTION:DESCRIPTION:END -->
