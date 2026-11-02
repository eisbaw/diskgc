---
id: TASK-318
title: >-
  Test-power follow-ups: index_authority generator coverage gaps, a structured
  marker for busy-host breaker skips, proptest persistence still off in liveness
status: To Do
assignee: []
created_date: '2026-11-02T14:01:00Z'
labels:
  - test
dependencies: []
priority: low
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-295 (2026-11-02): (1) generator gaps (QA): the re-match branch where discovery itself refuses is never reached; stale 'replaced' entries are rare (7 in 1,024); breaker-order differences never occur in generated cases, so the wildcard is exercised only by the saved replay (task295-breaker-order.yaml). (2) product hardening (MPED): a breaker skip has no structured marker in the report/JSON; the test and any harness match its message text; add a typed skip cause (like unknown_cause). (3) diskgc-core liveness properties have failure_persistence: None with no stated reason (grep -rn 'failure_persistence: None' crates). (4) check whether watch_process_reclaims_to_target_then_stays_quiet_and_stops_on_sigterm depends on the background tally under host PSI (possible TASK-310 sibling).
<!-- SECTION:DESCRIPTION:END -->
