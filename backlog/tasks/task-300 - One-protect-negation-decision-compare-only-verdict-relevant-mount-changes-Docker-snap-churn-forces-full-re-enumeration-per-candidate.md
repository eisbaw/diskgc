---
id: TASK-300
title: >-
  One protect/negation decision; compare only verdict-relevant mount changes
  (Docker/snap churn forces full re-enumeration per candidate)
status: To Do
assignee: []
created_date: '2026-11-01T16:39:00Z'
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
