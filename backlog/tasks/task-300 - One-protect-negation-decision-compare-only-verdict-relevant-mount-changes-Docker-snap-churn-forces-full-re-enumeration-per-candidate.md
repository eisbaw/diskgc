---
id: TASK-300
title: >-
  One protect/negation decision; compare only verdict-relevant mount changes
  (Docker/snap churn forces full re-enumeration per candidate)
status: Done
assignee: []
created_date: '2026-11-01T16:39:00Z'
updated_date: '2026-11-10T21:02:00Z'
labels:
  - safety
  - performance
  - refactor
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Periodic full review #3 on master fc33d98 (2026-11-01; Codex tmp/reviews/codex-full3.md + mped), static reading. mped SHOULD 1+2: policy.rs judge (Refresh), tier_policy.rs DeletePolicy::skip and policy_skip each decide when to re-enumerate; skip re-implements judge by hand; overlaps_checked_in/_until and excluded_by_in/_until differ only in reading mountinfo. Any mount change anywhere (docker overlay mounts, snap refresh, FUSE portal) makes mounts != protection.mounts and forces full protect+negated re-enumeration per candidate; a CI host with Docker can spend the whole --timeout. Fix: one judge with Refresh from (stage, action, staleness); staleness compares only ancestry of each protect/negated root and the candidate plus mounts within/at them (Mounts::unchanged_for). Input to TASK-274 and TASK-294. Measure with Docker churning during a reclaim with protect configured.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Mark 2026-11-10 ('do all first four', TASK-334 step 4): do it, after TASK-334's profile.

2026-11-10 merged to master (commit 36465cf1, local). places::unchanged_for decides staleness; one refresh table in executor/policy.rs. Gates: check 0, e2e 0. Container, 20k rc dirs + protect, 120 s, one run each: quiet 15,923 -> 15,381 reclaimed (noise), tmpfs churn ~20 events/s 9,908 -> 13,597. Limits: exactness rests on the consumer audit and a primitive-level property (1M cases), no full-run equivalence property; a filesystem change no table line shows is no longer caught by coincidence of an unrelated mount event (documented).
<!-- SECTION:NOTES:END -->
