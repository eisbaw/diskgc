---
id: TASK-258
title: 'watch: remaining model and container scenarios of docs/testing.md'
status: To Do
assignee: []
created_date: '2026-10-31T00:59:00Z'
labels:
  - watch
  - testing
dependencies: []
priority: medium
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-32 (implemented in diskgc-model tests/watch.rs and diskgc-e2e watch_suite.rs). Still open: model watch_cycle_deadline (a tier slower than --timeout: deadline record, no deletion after it, a running child gets SIGTERM); watch_root_vanished_after_start_still_reclaims_other_tiers; watch_signals for SIGINT and SIGHUP (ModelSignal has no Hup yet) and for a signal during the host-lock wait; watch_retry_back_off with a Docker tier once a backend reports backend_unavailable (the doubling is tested with a stacked mount). Container: stdout closed by the reader exits 3 (watch_process_reclaims_and_streams_json's second half), watch_process_stops_on_each_signal incl. SIGHUP under nohup, watch_and_ensure_share_a_filesystem (/proc/locks sampled), background_tally_does_not_load_the_system (needs watch --tally-only, now available).
<!-- SECTION:DESCRIPTION:END -->
