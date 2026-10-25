---
id: TASK-105
title: Seven-day per-profile tally history
status: To Do
assignee: []
created_date: '2026-10-21 19:53'
updated_date: '2026-10-25 20:18'
labels:
  - wave-3
  - index
  - observability
dependencies:
  - TASK-30
references:
  - docs/architecture.md
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Persist bounded time-series samples from completed per-profile tallies so operators can see growth and shrinkage over the last seven days. The history is advisory and must never authorize a deletion or silently turn partial, failed, or unknown measurements into zero. Build on the reviewed state and snapshot Host operation from TASK-30.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Expose a read-only per-profile trend or enough timestamped samples to compute velocity; at least two comparable complete samples are required. No automatic deletion-order change is made by this task.
- [ ] #2 Model and disposable-container tests cover growth, shrinkage, missing and partial tallies, clock jumps, scope changes, restart persistence, and retention bounds.
- [ ] #3 Retain seven days of observed samples using a bounded cadence or aggregation, such as hourly buckets; cap both samples per scope and total scopes, expire old data, and never invent measurements for missing hours. Use an allowlisted typed Host state target compatible with TASK-30, private 0600 atomic persistence, and serialized writers or a single writer so concurrent tally processes cannot lose updates. Corrupt or inaccessible history is logged as advisory and cannot block a fresh tally or weaken deletion safety.
- [ ] #4 A completed tally records timestamp, candidate count, coverage, and both discovered and eligible measured bytes for each profile; refused or unknown-size candidates are represented explicitly rather than as zero. Document that per-profile totals can overlap when profiles match the same unit. Samples are keyed by filesystem, owner, effective profile definition, resolved roots, and filters so incompatible scopes are never compared.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
COMPASS round 4 (2026-10-25): not in the PRD: deferred.
<!-- SECTION:NOTES:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 Definition of Done in docs/testing.md
<!-- DOD:END -->
