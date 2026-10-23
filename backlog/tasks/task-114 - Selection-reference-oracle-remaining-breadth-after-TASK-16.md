---
id: TASK-114
title: 'Selection reference oracle: remaining breadth after TASK-16'
status: To Do
assignee: []
created_date: '2026-10-23 15:50'
labels:
  - testing
  - selection
dependencies:
  - TASK-16
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-16 AC1 was closed on 2026-10-23 with two independent finite-path oracles (crates/diskgc-modelgen/tests/selection_reference.rs, selection_general_reference.rs) plus the core pure-evaluator differential (crates/diskgc-core/src/select/tests.rs). Breadth deliberately left out of those oracles: (1) find patterns deeper than two segments and trees deeper than two levels; (2) integrated unions of three or more terms (the core differential generates 2..5, the integrated oracle 2), and groups mixing several factors; (3) Cargo and command actions and other kinds in the oracle family (mixed-kind unions are refused by Tier::select); (4) multi-owner {uid} expansion beyond the saved OWNER_CONFIG case; (5) randomized races between plan and reclaim; (6) evidence budget: proptest uses RngSeed::Fixed and 32 cases in just check, and the 1024-case just soak is in no gate (a distribute mutant was caught only at 1024 generated cases, hence the saved grouped_union_applies_its_factor_to_every_member regression).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Generated trees and patterns reach at least depth 3 with an independent oracle; a mutation of segment matching beyond depth 2 turns it red
- [ ] #2 Integrated unions of 3+ terms, including grouped forms with several factors, match the oracle
- [ ] #3 A decision is recorded on whether just soak (or a rotated seed) joins a regular gate
<!-- AC:END -->
